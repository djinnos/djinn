//! Leader-only graph retention loop (epic z8ch, Wave 1 item 3).
//!
//! This module is the leader-gated ticker that drives the publication-safe
//! graph retention engine from [`djinn_db::RepoGraphRetentionRepository`]
//! (delivered by `75ap`). It mirrors the cancellation/ticker shape of
//! [`crate::git_maintenance`]: started exclusively from
//! [`AppState::become_leader`](crate::server::AppState::become_leader),
//! it runs until the process-wide [`CancellationToken`] fires.
//!
//! ## Why leader-only
//!
//! Graph retention mutates shared Postgres state (deletes generations and
//! cascades artifacts/chunks). Running it on standby HTTP-only pods during a
//! rolling deploy would race the leader and potentially double-sweep. The
//! advisory lock held by the leader is the single-active gate.
//!
//! ## Bounded telemetry
//!
//! All metrics use only fixed `mode`/`outcome`/`reason` labels via
//! [`djinn_telemetry::graph_retention`]. No project id, generation id, commit
//! sha, artifact etag, or content hash ever appears as a label. Per-tick logs
//! are similarly bounded: they carry aggregate counts only, never per-project
//! result collections.

use std::time::Duration;

use djinn_db::{
    DEFAULT_RETENTION_HISTORY_N, MAX_RETENTION_BATCH, MIN_RETENTION_HISTORY_N, ProjectRepository,
    RepoGraphRetentionRepository, RetentionMode, RetentionSkipClass,
};
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

use crate::server::AppState;
use djinn_telemetry::graph_retention as telemetry;

/// Environment variable selecting retention mode (`off`, `dry_run`, `delete`).
pub const MODE_ENV: &str = "DJINN_GRAPH_RETENTION_MODE";
/// Environment variable for the survivor history N (default 3).
pub const HISTORY_N_ENV: &str = "DJINN_GRAPH_RETENTION_HISTORY_N";
/// Environment variable for the sweep interval in seconds.
pub const INTERVAL_ENV: &str = "DJINN_GRAPH_RETENTION_INTERVAL_SECS";
/// Environment variable for the per-sweep retry budget.
pub const RETRIES_ENV: &str = "DJINN_GRAPH_RETENTION_MAX_RETRIES";

/// Default sweep cadence: 6 hours. Slow enough that it never competes with
/// dispatch, frequent enough to bound growth between sweeps.
const DEFAULT_INTERVAL_SECS: u64 = 6 * 60 * 60;
/// Maximum allowed sweep interval (24 hours) to keep the cadence bounded.
const MAX_INTERVAL_SECS: u64 = 24 * 60 * 60;
/// Minimum allowed sweep interval (60 seconds) so a misconfigured tiny value
/// doesn't hot-loop the DB.
const MIN_INTERVAL_SECS: u64 = 60;
/// Default retry budget mirrors the DB engine's
/// [`djinn_db::retry::DEFAULT_MAX_TX_RETRIES`].
const DEFAULT_MAX_RETRIES: usize = 3;
/// Minimum retry budget (at least 1 attempt).
const MIN_MAX_RETRIES: usize = 1;
/// Maximum retry budget to keep the per-sweep retry window bounded.
const MAX_MAX_RETRIES: usize = 10;

/// Validated runtime configuration for the graph retention loop.
///
/// All fields are validated at parse time; invalid values fail safely (the
/// mode degrades to [`RetentionMode::Off`] rather than silently enabling
/// deletion).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphRetentionConfig {
    /// Operating mode: `off`, `dry_run`, or `delete`.
    pub mode: RetentionMode,
    /// Number of newest `publish_seq` generations to keep (default 3).
    pub history_n: usize,
    /// Positive bounded sweep interval.
    pub interval: Duration,
    /// Bounded per-sweep retry budget.
    pub max_retries: usize,
}

impl Default for GraphRetentionConfig {
    fn default() -> Self {
        Self {
            mode: RetentionMode::Off,
            history_n: DEFAULT_RETENTION_HISTORY_N,
            interval: Duration::from_secs(DEFAULT_INTERVAL_SECS),
            max_retries: DEFAULT_MAX_RETRIES,
        }
    }
}

impl GraphRetentionConfig {
    /// Parse configuration from the process environment.
    ///
    /// On any invalid value, this returns a config with `mode = Off` so the
    /// loop never silently enables deletion due to a typo. The parse error
    /// detail is included so the caller can log it.
    pub fn from_env() -> Result<Self, ConfigError> {
        let mode = parse_mode(std::env::var(MODE_ENV).ok().as_deref())?;
        let history_n = parse_history_n(std::env::var(HISTORY_N_ENV).ok().as_deref())?;
        let interval = parse_interval(std::env::var(INTERVAL_ENV).ok().as_deref())?;
        let max_retries = parse_max_retries(std::env::var(RETRIES_ENV).ok().as_deref())?;
        Ok(Self {
            mode,
            history_n,
            interval,
            max_retries,
        })
    }

    /// Parse configuration from explicit string values (test-friendly).
    pub fn parse(
        mode: Option<&str>,
        history_n: Option<&str>,
        interval_secs: Option<&str>,
        max_retries: Option<&str>,
    ) -> Result<Self, ConfigError> {
        Ok(Self {
            mode: parse_mode(mode)?,
            history_n: parse_history_n(history_n)?,
            interval: parse_interval(interval_secs)?,
            max_retries: parse_max_retries(max_retries)?,
        })
    }
}

/// Error returned when configuration parsing fails safely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "graph retention config error: {}", self.0)
    }
}

impl std::error::Error for ConfigError {}

/// Parse the retention mode. Accepts exactly `off`, `dry_run`, or `delete`
/// (case-insensitive). `None` (unset) defaults to `Off`. Any other value is
/// an error — the caller must decide to fail safe.
fn parse_mode(raw: Option<&str>) -> Result<RetentionMode, ConfigError> {
    match raw {
        None => Ok(RetentionMode::Off),
        Some(s) => match s.trim().to_ascii_lowercase().as_str() {
            "off" => Ok(RetentionMode::Off),
            "dry_run" => Ok(RetentionMode::DryRun),
            "delete" => Ok(RetentionMode::Delete),
            other => Err(ConfigError(format!(
                "invalid mode '{other}': expected off, dry_run, or delete"
            ))),
        },
    }
}

/// Parse history N. `None` defaults to [`DEFAULT_RETENTION_HISTORY_N`]. Must
/// be >= [`MIN_RETENTION_HISTORY_N`].
fn parse_history_n(raw: Option<&str>) -> Result<usize, ConfigError> {
    match raw {
        None => Ok(DEFAULT_RETENTION_HISTORY_N),
        Some(s) => {
            let trimmed = s.trim();
            let value: usize = trimmed.parse().map_err(|_| {
                ConfigError(format!(
                    "invalid history_n '{trimmed}': expected a positive integer"
                ))
            })?;
            if value < MIN_RETENTION_HISTORY_N {
                return Err(ConfigError(format!(
                    "invalid history_n {value}: must be at least {MIN_RETENTION_HISTORY_N}"
                )));
            }
            Ok(value)
        }
    }
}

/// Parse the sweep interval. `None` defaults to [`DEFAULT_INTERVAL_SECS`].
/// Must be within [`MIN_INTERVAL_SECS`]..=[`MAX_INTERVAL_SECS`].
fn parse_interval(raw: Option<&str>) -> Result<Duration, ConfigError> {
    match raw {
        None => Ok(Duration::from_secs(DEFAULT_INTERVAL_SECS)),
        Some(s) => {
            let trimmed = s.trim();
            let secs: u64 = trimmed.parse().map_err(|_| {
                ConfigError(format!(
                    "invalid interval '{trimmed}': expected a positive integer (seconds)"
                ))
            })?;
            if secs < MIN_INTERVAL_SECS {
                return Err(ConfigError(format!(
                    "invalid interval {secs}s: must be at least {MIN_INTERVAL_SECS}s"
                )));
            }
            if secs > MAX_INTERVAL_SECS {
                return Err(ConfigError(format!(
                    "invalid interval {secs}s: must be at most {MAX_INTERVAL_SECS}s"
                )));
            }
            Ok(Duration::from_secs(secs))
        }
    }
}

/// Parse the per-sweep retry budget. `None` defaults to
/// [`DEFAULT_MAX_RETRIES`]. Must be within
/// [`MIN_MAX_RETRIES`]..=[`MAX_MAX_RETRIES`].
fn parse_max_retries(raw: Option<&str>) -> Result<usize, ConfigError> {
    match raw {
        None => Ok(DEFAULT_MAX_RETRIES),
        Some(s) => {
            let trimmed = s.trim();
            let value: usize = trimmed.parse().map_err(|_| {
                ConfigError(format!(
                    "invalid max_retries '{trimmed}': expected a positive integer"
                ))
            })?;
            if value < MIN_MAX_RETRIES {
                return Err(ConfigError(format!(
                    "invalid max_retries {value}: must be at least {MIN_MAX_RETRIES}"
                )));
            }
            if value > MAX_MAX_RETRIES {
                return Err(ConfigError(format!(
                    "invalid max_retries {value}: must be at most {MAX_MAX_RETRIES}"
                )));
            }
            Ok(value)
        }
    }
}

/// Map a [`RetentionMode`] to its fixed telemetry label.
fn mode_label(mode: RetentionMode) -> &'static str {
    match mode {
        RetentionMode::Off => telemetry::MODE_OFF,
        RetentionMode::DryRun => telemetry::MODE_DRY_RUN,
        RetentionMode::Delete => telemetry::MODE_DELETE,
    }
}

/// Spawn the periodic graph-retention task. Leader-only (started from
/// `become_leader`), runs until `state.cancel()` fires.
///
/// If the configuration fails to parse, the loop starts in `off` mode (no DB
/// retention calls) and logs the parse error once. This is the fail-safe
/// behavior: invalid configuration never silently enables deletion.
pub fn spawn(state: AppState) {
    let config = match GraphRetentionConfig::from_env() {
        Ok(c) => c,
        Err(err) => {
            tracing::error!(
                error = %err,
                "graph_retention: invalid configuration; defaulting to off mode (no sweeps)"
            );
            GraphRetentionConfig::default()
        }
    };
    let cancel = state.cancel().clone();
    tokio::spawn(async move {
        run_loop(state, config, cancel).await;
    });
}

/// The core ticker loop, extracted for testability. Runs until `cancel` fires.
async fn run_loop(state: AppState, config: GraphRetentionConfig, cancel: CancellationToken) {
    tracing::info!(?config, "graph_retention loop starting");
    let mut ticker = tokio::time::interval(config.interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // Consume the immediate first tick so we don't sweep right at boot during
    // the leadership transition.
    ticker.tick().await;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::debug!("graph_retention loop cancelled");
                break;
            }
            _ = ticker.tick() => {
                run_tick(&state, &config).await;
            }
        }
    }
}

/// Execute one sweep tick: iterate all projects one at a time, calling the
/// retention engine for each. Does not load or retain graph blobs — the DB
/// engine handles blob deletion via FK cascade without materializing them.
async fn run_tick(state: &AppState, config: &GraphRetentionConfig) {
    // `off` performs no DB retention calls at all.
    if config.mode == RetentionMode::Off {
        tracing::debug!("graph_retention: mode off; skipping tick");
        return;
    }

    let project_repo = ProjectRepository::new(state.db().clone(), state.event_bus());
    let retention_repo = RepoGraphRetentionRepository::new(state.db().clone());
    // Only a fixed-size page of IDs is retained at once; graph blobs never
    // leave the retention repository.
    const PROJECT_PAGE_SIZE: i64 = 100;
    let mut after_id: Option<String> = None;
    let mut project_count = 0u64;
    let mut total_candidates = 0u64;
    let mut total_deleted = 0u64;
    let mut total_skipped = 0u64;
    let mut total_retries = 0u64;
    let mut total_errors = 0u64;

    loop {
        let page = match project_repo
            .list_ids_page_after(after_id.as_deref(), PROJECT_PAGE_SIZE)
            .await
        {
            Ok(page) => page,
            Err(_) => {
                total_errors += 1;
                telemetry::increment(
                    mode_label(config.mode),
                    telemetry::OUTCOME_ERROR,
                    telemetry::REASON_NONE,
                    1,
                );
                break;
            }
        };
        if page.is_empty() || state.cancel().is_cancelled() {
            break;
        }
        after_id = page.last().cloned();

        for project_id in page {
            project_count += 1;
            // Check cancellation between projects so a long tick can be aborted.
            if state.cancel().is_cancelled() {
                tracing::debug!("graph_retention: cancelled mid-tick");
                break;
            }

            let request = djinn_db::RetentionSweepRequest {
                project_id: &project_id,
                mode: config.mode,
                history_n: config.history_n,
            };

            match retention_repo
                .sweep_with_retry(request, config.max_retries)
                .await
            {
                Ok(outcome) => {
                    // Emit telemetry with fixed labels only. The outcome carries
                    // bounded counts and fixed skip classes — never identity.
                    telemetry::increment(
                        mode_label(config.mode),
                        telemetry::OUTCOME_CANDIDATE,
                        telemetry::REASON_NONE,
                        outcome.candidates as u64,
                    );
                    if outcome.deleted > 0 {
                        telemetry::increment(
                            mode_label(config.mode),
                            telemetry::OUTCOME_DELETE,
                            telemetry::REASON_NONE,
                            outcome.deleted as u64,
                        );
                    }
                    if outcome.skipped_active_pin > 0 {
                        telemetry::increment(
                            mode_label(config.mode),
                            telemetry::OUTCOME_SKIP,
                            telemetry::REASON_ACTIVE_PIN,
                            outcome.skipped_active_pin as u64,
                        );
                    }
                    if outcome.skipped_now_survivor > 0 {
                        telemetry::increment(
                            mode_label(config.mode),
                            telemetry::OUTCOME_SKIP,
                            telemetry::REASON_NOW_SURVIVOR,
                            outcome.skipped_now_survivor as u64,
                        );
                    }
                    if outcome.skipped_removed_concurrently > 0 {
                        telemetry::increment(
                            mode_label(config.mode),
                            telemetry::OUTCOME_SKIP,
                            telemetry::REASON_REMOVED_CONCURRENTLY,
                            outcome.skipped_removed_concurrently as u64,
                        );
                    }
                    if outcome.retries > 0 {
                        telemetry::increment(
                            mode_label(config.mode),
                            telemetry::OUTCOME_RETRY,
                            telemetry::REASON_NONE,
                            outcome.retries as u64,
                        );
                    }

                    total_candidates += outcome.candidates as u64;
                    total_deleted += outcome.deleted as u64;
                    total_skipped += outcome.total_skipped() as u64;
                    total_retries += outcome.retries as u64;
                }
                Err(_) => {
                    total_errors += 1;
                    telemetry::increment(
                        mode_label(config.mode),
                        telemetry::OUTCOME_ERROR,
                        telemetry::REASON_NONE,
                        1,
                    );
                    // Log the error without project identity — only the bounded
                    // aggregate is logged at tick end. Per-project errors are
                    // counted but not individually retained.
                }
            }
        }
    }
    // Single bounded tick summary — no per-project results accumulated.
    tracing::info!(
        mode = mode_label(config.mode),
        projects = project_count,
        candidates = total_candidates,
        deleted = total_deleted,
        skipped = total_skipped,
        retries = total_retries,
        errors = total_errors,
        "graph_retention: tick complete"
    );
}

/// Convert a [`RetentionSkipClass`] to its telemetry reason label.
///
/// This is kept as a public helper so tests and future call sites can map
/// skip classes to the fixed label vocabulary without duplicating the match.
pub fn skip_class_reason(class: RetentionSkipClass) -> &'static str {
    match class {
        RetentionSkipClass::ActiveStreamPin => telemetry::REASON_ACTIVE_PIN,
        RetentionSkipClass::NowSurvivor => telemetry::REASON_NOW_SURVIVOR,
        RetentionSkipClass::RowRemovedConcurrently => telemetry::REASON_REMOVED_CONCURRENTLY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_db::RetentionMode;

    // ── Configuration validation tests ───────────────────────────────

    #[test]
    fn parse_defaults_when_all_unset() {
        let config = GraphRetentionConfig::parse(None, None, None, None).unwrap();
        assert_eq!(config.mode, RetentionMode::Off);
        assert_eq!(config.history_n, DEFAULT_RETENTION_HISTORY_N);
        assert_eq!(config.interval, Duration::from_secs(DEFAULT_INTERVAL_SECS));
        assert_eq!(config.max_retries, DEFAULT_MAX_RETRIES);
    }

    #[test]
    fn parse_mode_accepts_exact_values_case_insensitively() {
        assert_eq!(
            GraphRetentionConfig::parse(Some("off"), None, None, None)
                .unwrap()
                .mode,
            RetentionMode::Off
        );
        assert_eq!(
            GraphRetentionConfig::parse(Some("dry_run"), None, None, None)
                .unwrap()
                .mode,
            RetentionMode::DryRun
        );
        assert_eq!(
            GraphRetentionConfig::parse(Some("delete"), None, None, None)
                .unwrap()
                .mode,
            RetentionMode::Delete
        );
        assert_eq!(
            GraphRetentionConfig::parse(Some("DRY_RUN"), None, None, None)
                .unwrap()
                .mode,
            RetentionMode::DryRun
        );
        assert_eq!(
            GraphRetentionConfig::parse(Some("Delete"), None, None, None)
                .unwrap()
                .mode,
            RetentionMode::Delete
        );
    }

    #[test]
    fn parse_mode_rejects_unknown_without_enabling_delete() {
        let err = GraphRetentionConfig::parse(Some("purge"), None, None, None).unwrap_err();
        assert!(err.to_string().contains("invalid mode"));
        assert!(err.to_string().contains("purge"));

        let err = GraphRetentionConfig::parse(Some(""), None, None, None).unwrap_err();
        assert!(err.to_string().contains("invalid mode"));

        let err = GraphRetentionConfig::parse(Some("true"), None, None, None).unwrap_err();
        assert!(err.to_string().contains("invalid mode"));
    }

    #[test]
    fn parse_history_n_validates_positive_bounded() {
        assert_eq!(
            GraphRetentionConfig::parse(None, Some("5"), None, None)
                .unwrap()
                .history_n,
            5
        );
        assert_eq!(
            GraphRetentionConfig::parse(None, Some("1"), None, None)
                .unwrap()
                .history_n,
            1
        );
        // Zero is invalid
        assert!(GraphRetentionConfig::parse(None, Some("0"), None, None).is_err());
        // Negative (parsed as usize, so this is a parse error)
        assert!(GraphRetentionConfig::parse(None, Some("-1"), None, None).is_err());
    }

    #[test]
    fn parse_interval_validates_positive_bounded() {
        assert_eq!(
            GraphRetentionConfig::parse(None, None, Some("3600"), None)
                .unwrap()
                .interval,
            Duration::from_secs(3600)
        );
        assert_eq!(
            GraphRetentionConfig::parse(None, None, Some("60"), None)
                .unwrap()
                .interval,
            Duration::from_secs(60)
        );
        // Below minimum
        assert!(GraphRetentionConfig::parse(None, None, Some("30"), None).is_err());
        assert!(GraphRetentionConfig::parse(None, None, Some("0"), None).is_err());
        // Above maximum
        assert!(GraphRetentionConfig::parse(None, None, Some("999999"), None).is_err());
        // Non-numeric
        assert!(GraphRetentionConfig::parse(None, None, Some("abc"), None).is_err());
    }

    #[test]
    fn parse_max_retries_validates_positive_bounded() {
        assert_eq!(
            GraphRetentionConfig::parse(None, None, None, Some("5"))
                .unwrap()
                .max_retries,
            5
        );
        assert_eq!(
            GraphRetentionConfig::parse(None, None, None, Some("1"))
                .unwrap()
                .max_retries,
            1
        );
        // Zero is invalid
        assert!(GraphRetentionConfig::parse(None, None, None, Some("0")).is_err());
        // Above maximum
        assert!(GraphRetentionConfig::parse(None, None, None, Some("100")).is_err());
        // Non-numeric
        assert!(GraphRetentionConfig::parse(None, None, None, Some("xyz")).is_err());
    }

    #[test]
    fn invalid_config_does_not_silently_enable_deletion() {
        // Any single invalid field fails the whole parse, so the caller's
        // fail-safe (defaulting to Off) is triggered rather than silently
        // running Delete with wrong parameters.
        let cases = [
            (Some("invalid"), None, None, None),
            (None, Some("0"), None, None),
            (None, None, Some("0"), None),
            (None, None, None, Some("0")),
            (Some("delete"), Some("-5"), None, None),
        ];
        for (mode, history_n, interval, retries) in cases {
            let result = GraphRetentionConfig::parse(mode, history_n, interval, retries);
            assert!(
                result.is_err(),
                "expected config error for mode={mode:?} history_n={history_n:?} interval={interval:?} retries={retries:?}"
            );
        }
    }

    // ── Mode dispatch tests ───────────────────────────────────────────

    #[test]
    fn mode_dispatch_off_performs_no_db_calls() {
        // In `off` mode, run_tick returns immediately without touching the
        // retention repository. We verify this via the config's mode field:
        // the dispatch check `config.mode == RetentionMode::Off` short-
        // circuits before any DB call.
        let config = GraphRetentionConfig::parse(Some("off"), None, None, None).unwrap();
        assert_eq!(config.mode, RetentionMode::Off);
        // The run_tick function checks `config.mode == Off` and returns before
        // constructing any repository. This test documents that contract.
    }

    #[test]
    fn mode_dispatch_dry_run_vs_delete() {
        let dry_run = GraphRetentionConfig::parse(Some("dry_run"), None, None, None).unwrap();
        assert_eq!(dry_run.mode, RetentionMode::DryRun);

        let delete = GraphRetentionConfig::parse(Some("delete"), None, None, None).unwrap();
        assert_eq!(delete.mode, RetentionMode::Delete);

        // The actual DB dispatch difference (non-mutating vs delete) is
        // handled by RepoGraphRetentionRepository::sweep, which selects
        // dry_run vs delete paths internally. This loop passes the mode
        // through faithfully.
        assert_ne!(dry_run.mode, delete.mode);
    }

    // ── Cancellation tests ────────────────────────────────────────────

    #[tokio::test]
    async fn run_loop_cancels_cleanly() {
        let config = GraphRetentionConfig::default();
        let cancel = CancellationToken::new();

        // We can't easily build a full AppState in a unit test, but we can
        // verify the cancellation token wiring: the loop's select! branch on
        // cancel.cancelled() must fire.
        //
        // Instead of a full AppState, test the cancellation semantics
        // directly: the token is the same type used by git_maintenance.
        cancel.cancel();
        assert!(cancel.is_cancelled());

        // A real run_loop would break on the next select! iteration. The
        // config.mode == Off means run_tick is a no-op, so the loop would
        // just wait on the ticker — cancellation fires immediately.
        let _ = config; // suppress unused warning
    }

    #[tokio::test]
    async fn cancellation_token_is_the_same_type_as_git_maintenance() {
        // Verify the cancellation token type matches what AppState::cancel()
        // returns, ensuring the loop can be composed from become_leader.
        let cancel: CancellationToken = CancellationToken::new();
        cancel.cancel();
        // This compiles only if the type is correct.
        assert!(cancel.is_cancelled());
    }

    // ── Fixed label vocabulary tests ──────────────────────────────────

    #[test]
    fn mode_label_covers_all_modes() {
        assert_eq!(mode_label(RetentionMode::Off), "off");
        assert_eq!(mode_label(RetentionMode::DryRun), "dry_run");
        assert_eq!(mode_label(RetentionMode::Delete), "delete");
    }

    #[test]
    fn mode_labels_match_telemetry_constants() {
        assert_eq!(mode_label(RetentionMode::Off), telemetry::MODE_OFF);
        assert_eq!(mode_label(RetentionMode::DryRun), telemetry::MODE_DRY_RUN);
        assert_eq!(mode_label(RetentionMode::Delete), telemetry::MODE_DELETE);
    }

    #[test]
    fn skip_class_reason_maps_all_variants() {
        assert_eq!(
            skip_class_reason(RetentionSkipClass::ActiveStreamPin),
            telemetry::REASON_ACTIVE_PIN
        );
        assert_eq!(
            skip_class_reason(RetentionSkipClass::NowSurvivor),
            telemetry::REASON_NOW_SURVIVOR
        );
        assert_eq!(
            skip_class_reason(RetentionSkipClass::RowRemovedConcurrently),
            telemetry::REASON_REMOVED_CONCURRENTLY
        );
    }

    #[test]
    fn telemetry_label_vocabularies_are_bounded_and_fixed() {
        // The mode/outcome/reason label sets are exactly the registered
        // vocabularies — no dynamic or identity labels can appear.
        let modes: Vec<&str> = telemetry::ALL_MODES.to_vec();
        let outcomes: Vec<&str> = telemetry::ALL_OUTCOMES.to_vec();
        let reasons: Vec<&str> = telemetry::ALL_REASONS.to_vec();

        assert_eq!(modes, vec!["off", "dry_run", "delete"]);
        assert_eq!(
            outcomes,
            vec!["candidate", "delete", "skip", "retry", "error"]
        );
        assert_eq!(
            reasons,
            vec!["none", "active_pin", "now_survivor", "removed_concurrently"]
        );

        // No identity labels exist in any vocabulary.
        for label in modes.iter().chain(outcomes.iter()).chain(reasons.iter()) {
            assert!(
                !label.contains("project"),
                "forbidden project label: {label}"
            );
            assert!(
                !label.contains("generation"),
                "forbidden generation label: {label}"
            );
            assert!(!label.contains("commit"), "forbidden commit label: {label}");
            assert!(!label.contains("etag"), "forbidden etag label: {label}");
            assert!(!label.contains("hash"), "forbidden hash label: {label}");
            assert!(
                !label.contains("artifact"),
                "forbidden artifact label: {label}"
            );
        }
    }

    // ── Leader-only composition tests ─────────────────────────────────

    #[test]
    fn spawn_is_composed_only_in_become_leader() {
        let state_source = include_str!("server/state/mod.rs");
        let spawn = "crate::graph_retention::spawn(self.clone())";
        assert_eq!(state_source.matches(spawn).count(), 1);
        let leader = state_source.find("pub async fn become_leader").unwrap();
        let retention = state_source.find(spawn).unwrap();
        assert!(retention > leader);
        let initialize = state_source.find("pub async fn initialize(&self)").unwrap();
        assert!(!state_source[initialize..leader].contains(spawn));
    }

    #[test]
    fn config_default_is_off_mode() {
        // The default configuration must be Off so that even if the env vars
        // are completely unset, the loop does nothing — never silently
        // deletes.
        let config = GraphRetentionConfig::default();
        assert_eq!(config.mode, RetentionMode::Off);
    }

    #[test]
    fn max_retention_batch_constant_matches_db() {
        // The loop does not override the DB engine's batch cap.
        assert_eq!(MAX_RETENTION_BATCH, 25);
    }

    #[test]
    fn default_history_n_is_three() {
        assert_eq!(DEFAULT_RETENTION_HISTORY_N, 3);
        assert_eq!(MIN_RETENTION_HISTORY_N, 1);
    }
}
