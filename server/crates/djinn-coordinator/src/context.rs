//! Coordinator-side application context.
//!
//! Mirrors the subset of `djinn_agent::context::AgentContext` that the
//! coordinator, doctor, and coordinator-owned supervisor modules need.
//! This avoids a circular dependency on `djinn-agent`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use crate::cargo_warm_base_gc::WarmJobGuard;
use crate::roles::RoleRegistry;
use djinn_control_plane::bridge;
use djinn_core::clock::{Clock, SystemClock};
use djinn_git::{GitActorHandle, GitError};
use djinn_lsp::LspManager;
use djinn_orchestration_types::coordinator::BackgroundWorkTracker;
use djinn_provider::catalog::{CatalogService, HealthTracker};
use djinn_runtime::GraphWarmerService;
use djinn_supervisor::ConnectionRegistry;
use djinn_workspace::MirrorManager;
use tokio::sync::Mutex;

/// Shared tracker for per-task last-activity timestamps (unix seconds).
/// Used by stall detection to kill sessions that stop producing tokens.
pub type ActivityTracker = Arc<std::sync::Mutex<HashMap<String, Arc<AtomicU64>>>>;

/// Configuration for the periodic reconciliation sweep that reaps stale PRs,
/// branches, and orphan worker sessions.
///
/// Read from environment variables at construction time.  Defaults are
/// conservative: the sweep is **disabled** and in **dry-run** mode so that
/// first runs are safe.  Operators opt in by setting
/// `DJINN_RECONCILIATION_SWEEP_ENABLED=true`.
#[derive(Debug, Clone)]
pub struct ReconciliationSweepConfig {
    /// Whether the reconciliation sweep is active.
    /// Env: `DJINN_RECONCILIATION_SWEEP_ENABLED` (default `false`).
    pub enabled: bool,
    /// When `true`, the sweep logs what it *would* do without calling GitHub.
    /// Env: `DJINN_RECONCILIATION_SWEEP_DRY_RUN` (default `true`).
    pub dry_run: bool,
    /// Seconds after a task closes before its PR/branch becomes eligible for
    /// sweep reaping.
    /// Env: `DJINN_RECONCILIATION_SWEEP_GRACE_PERIOD_SECS` (default `600`).
    pub grace_period: Duration,
}

/// Rollout gate for the coordinator-owned retroactive proposal lint sweep.
/// The sweep is disabled by default because it reads retained proposal bodies.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProposalSpecIntegritySweepConfig {
    /// Env: `DJINN_PROPOSAL_SPEC_INTEGRITY_SWEEP_V1` (default `false`).
    pub enabled: bool,
}

impl ProposalSpecIntegritySweepConfig {
    pub fn from_env() -> Self {
        Self {
            enabled: std::env::var("DJINN_PROPOSAL_SPEC_INTEGRITY_SWEEP_V1")
                .map(|value| parse_bool_env(&value))
                .unwrap_or(false),
        }
    }
}

impl Default for ReconciliationSweepConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dry_run: true,
            grace_period: Duration::from_secs(600),
        }
    }
}

impl ReconciliationSweepConfig {
    /// Build a config from environment variables, falling back to safe defaults.
    pub fn from_env() -> Self {
        let enabled = std::env::var("DJINN_RECONCILIATION_SWEEP_ENABLED")
            .map(|v| parse_bool_env(&v))
            .unwrap_or(false);

        let dry_run = std::env::var("DJINN_RECONCILIATION_SWEEP_DRY_RUN")
            .map(|v| parse_bool_env(&v))
            .unwrap_or(true);

        let grace_period_secs = std::env::var("DJINN_RECONCILIATION_SWEEP_GRACE_PERIOD_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(600);

        Self {
            enabled,
            dry_run,
            grace_period: Duration::from_secs(grace_period_secs),
        }
    }
}

/// Parse a boolean environment variable value.  Accepts `1`, `true`, `yes`
/// (case-insensitive) as truthy; everything else is falsy.
fn parse_bool_env(val: &str) -> bool {
    matches!(
        val.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes"
    )
}

/// Global cache cleanup mode controlling whether cleanup passes actually
/// delete entries or only log what they *would* delete.
///
/// Defaults to `DryRun` so that first runs are safe.  Operators opt in to
/// destructive deletion by setting `DJINN_CACHE_CLEANUP_MODE=delete`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheCleanupMode {
    /// Log cleanup candidates without deleting anything.
    DryRun,
    /// Actually delete entries that exceed the configured retention thresholds.
    Delete,
}

impl CacheCleanupMode {
    /// Parse a mode string from an environment variable value.
    ///
    /// Accepts `delete` (case-insensitive) as the destructive mode;
    /// everything else falls back to `DryRun`.
    pub fn from_env_value(val: &str) -> Self {
        if val.trim().eq_ignore_ascii_case("delete") {
            Self::Delete
        } else {
            Self::DryRun
        }
    }

    /// Returns `true` when the mode is `Delete` (destructive cleanup enabled).
    pub fn is_delete(self) -> bool {
        matches!(self, Self::Delete)
    }

    /// Returns a Prometheus-friendly label string for metric emission.
    pub fn as_metric_label(self) -> &'static str {
        match self {
            Self::DryRun => "dry_run",
            Self::Delete => "delete",
        }
    }
}

/// Configuration for coordinator-owned cache cleanup passes.
///
/// Read from environment variables at construction time.  Defaults are
/// conservative: cleanup is **enabled** (so size-growth monitoring runs) but
/// the global mode is **dry_run** so no entries are actually deleted unless
/// operators explicitly opt in.
///
/// Per-path enable flags and retention thresholds let operators tune each
/// cache path independently.  The `mode` flag is the global kill-switch for
/// destructive deletion.
#[derive(Debug, Clone)]
pub struct CacheCleanupConfig {
    /// Global cleanup mode.
    /// Env: `DJINN_CACHE_CLEANUP_MODE` (default `"dry_run"`).
    /// Accepted values: `"dry_run"`, `"delete"`.
    pub mode: CacheCleanupMode,

    /// Whether sccache cleanup is enabled.
    /// Env: `DJINN_CACHE_CLEANUP_SCCACHE_ENABLED` (default `true`).
    pub sccache_enabled: bool,

    /// sccache max age in hours before files become cleanup candidates.
    /// Env: `DJINN_CACHE_CLEANUP_SCCACHE_MAX_AGE_HOURS` (default `24`).
    pub sccache_max_age_hours: u64,

    /// Whether cargo-target-runs debris cleanup is enabled.
    /// Env: `DJINN_CACHE_CLEANUP_CARGO_DEBRIS_ENABLED` (default `true`).
    pub cargo_debris_enabled: bool,

    /// cargo-target-runs debris max age in days before entries become cleanup
    /// candidates.
    /// Env: `DJINN_CACHE_CLEANUP_CARGO_DEBRIS_MAX_AGE_DAYS` (default `7`).
    pub cargo_debris_max_age_days: u64,

    /// Warm-base idle retention in days before a `/cache/cargo-target/<project_id>`
    /// directory is eligible for whole-base eviction.
    /// Env: `DJINN_CACHE_CLEANUP_WARM_BASE_IDLE_RETENTION_DAYS` (default `14`).
    pub warm_base_idle_retention_days: u64,

    /// Project-idle time before an individual Cargo profile bundle is eligible
    /// for pressure reclamation. Artifact mtimes do not establish staleness.
    /// Env: `DJINN_CACHE_CLEANUP_WARM_PROFILE_MIN_IDLE_HOURS` (default `24`).
    /// Zero is valid and makes a profile immediately eligible.
    pub warm_profile_min_idle_hours: u64,

    /// Grace period before a newly-created or freshly-active warm base can be
    /// considered idle, even if the recorded DB activity falls outside the
    /// retention window. Protects warm bases that are mid-warm or about to be
    /// seeded by a dispatched task run.
    /// Env: `DJINN_CACHE_CLEANUP_WARM_BASE_GRACE_PERIOD_SECS` (default `300`).
    pub warm_base_grace_period: Duration,

    /// Disk free-space low watermark below which pressure-driven whole-base
    /// eviction starts. Expressed as a fraction of total available bytes.
    /// Env: `DJINN_CACHE_CLEANUP_WARM_BASE_LOW_FREE_RATIO` (default `0.15`).
    pub warm_base_low_free_ratio: f64,

    /// Disk free-space high watermark at which pressure-driven whole-base
    /// eviction stops. Expressed as a fraction of total available bytes.
    /// Env: `DJINN_CACHE_CLEANUP_WARM_BASE_HIGH_FREE_RATIO` (default `0.25`).
    pub warm_base_high_free_ratio: f64,
}

impl Default for CacheCleanupConfig {
    fn default() -> Self {
        Self {
            mode: CacheCleanupMode::DryRun,
            sccache_enabled: true,
            sccache_max_age_hours: 24,
            cargo_debris_enabled: true,
            cargo_debris_max_age_days: 7,
            warm_base_idle_retention_days: 14,
            warm_profile_min_idle_hours: 24,
            warm_base_grace_period: Duration::from_secs(300),
            warm_base_low_free_ratio: 0.15,
            warm_base_high_free_ratio: 0.25,
        }
    }
}

impl CacheCleanupConfig {
    /// Build a config from environment variables, falling back to safe defaults.
    pub fn from_env() -> Self {
        let mode = std::env::var("DJINN_CACHE_CLEANUP_MODE")
            .map(|v| CacheCleanupMode::from_env_value(&v))
            .unwrap_or(CacheCleanupMode::DryRun);

        let sccache_enabled = std::env::var("DJINN_CACHE_CLEANUP_SCCACHE_ENABLED")
            .map(|v| parse_bool_env(&v))
            .unwrap_or(true);

        let sccache_max_age_hours = std::env::var("DJINN_CACHE_CLEANUP_SCCACHE_MAX_AGE_HOURS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(24);

        let cargo_debris_enabled = std::env::var("DJINN_CACHE_CLEANUP_CARGO_DEBRIS_ENABLED")
            .map(|v| parse_bool_env(&v))
            .unwrap_or(true);

        let cargo_debris_max_age_days =
            std::env::var("DJINN_CACHE_CLEANUP_CARGO_DEBRIS_MAX_AGE_DAYS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(7);

        let warm_base_idle_retention_days =
            std::env::var("DJINN_CACHE_CLEANUP_WARM_BASE_IDLE_RETENTION_DAYS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(14);

        let warm_profile_min_idle_hours =
            std::env::var("DJINN_CACHE_CLEANUP_WARM_PROFILE_MIN_IDLE_HOURS")
                .ok()
                .and_then(|value| parse_unsigned_decimal(&value))
                .unwrap_or(24);

        let warm_base_grace_period_secs =
            std::env::var("DJINN_CACHE_CLEANUP_WARM_BASE_GRACE_PERIOD_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(300);

        let warm_base_low_free_ratio =
            std::env::var("DJINN_CACHE_CLEANUP_WARM_BASE_LOW_FREE_RATIO")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|v| (0.0..1.0).contains(v))
                .unwrap_or(0.15);

        let warm_base_high_free_ratio =
            std::env::var("DJINN_CACHE_CLEANUP_WARM_BASE_HIGH_FREE_RATIO")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|v| (0.0..1.0).contains(v))
                .unwrap_or(0.25);

        let warm_base_low_free_ratio = warm_base_low_free_ratio.min(warm_base_high_free_ratio);

        Self {
            mode,
            sccache_enabled,
            sccache_max_age_hours,
            cargo_debris_enabled,
            cargo_debris_max_age_days,
            warm_base_idle_retention_days,
            warm_profile_min_idle_hours,
            warm_base_grace_period: Duration::from_secs(warm_base_grace_period_secs),
            warm_base_low_free_ratio,
            warm_base_high_free_ratio,
        }
    }

    /// Validate warm-base watermark sanity: low must be <= high and both must
    /// lie strictly inside [0, 1].
    pub fn validate_warm_base_watermarks(&self) -> Result<(), String> {
        if !(0.0..1.0).contains(&self.warm_base_low_free_ratio) {
            return Err(format!(
                "low_free_ratio must be in [0, 1), got {}",
                self.warm_base_low_free_ratio
            ));
        }
        if !(0.0..1.0).contains(&self.warm_base_high_free_ratio) {
            return Err(format!(
                "high_free_ratio must be in [0, 1), got {}",
                self.warm_base_high_free_ratio
            ));
        }
        if self.warm_base_low_free_ratio > self.warm_base_high_free_ratio {
            return Err(format!(
                "low_free_ratio ({}) must be <= high_free_ratio ({})",
                self.warm_base_low_free_ratio, self.warm_base_high_free_ratio
            ));
        }
        Ok(())
    }
}

/// Parse only an ASCII unsigned decimal. In particular, `+1`, whitespace,
/// suffixes, and overflowing values are invalid rather than silently accepted.
fn parse_unsigned_decimal(value: &str) -> Option<u64> {
    (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
}

/// Subset of application state required by the coordinator actor, doctor
/// checks, and coordinator-owned supervisor dispatch.
///
/// This mirrors `djinn_agent::context::AgentContext` without the
/// `coordinator: Arc<Mutex<Option<CoordinatorHandle>>>` field, which would
/// create a circular crate dependency.  Cheaply cloneable — all fields
/// are either `Clone` or wrapped in `Arc`.
#[derive(Clone)]
pub struct CoordinatorContext {
    pub db: djinn_db::Database,
    pub event_bus: djinn_core::events::EventBus,
    pub git_actors: Arc<Mutex<HashMap<PathBuf, GitActorHandle>>>,
    pub background_work_tasks: BackgroundWorkTracker,
    pub role_registry: Arc<RoleRegistry>,
    pub health_tracker: HealthTracker,
    pub file_time: Arc<crate::file_time::FileTime>,
    pub lsp: LspManager,
    pub catalog: CatalogService,
    pub active_tasks: ActivityTracker,
    pub task_ops_project_path_override: Option<PathBuf>,
    pub working_root: Option<PathBuf>,
    pub graph_warmer: Option<Arc<dyn GraphWarmerService>>,
    pub warm_job_guard: Option<Arc<dyn WarmJobGuard>>,
    pub repo_graph_ops: Option<Arc<dyn bridge::RepoGraphOps>>,
    pub runtime_ops: Option<Arc<dyn bridge::RuntimeOps>>,
    pub cargo_target_runs_root: Option<PathBuf>,
    pub mirror: Option<Arc<MirrorManager>>,
    pub rpc_registry: Option<Arc<ConnectionRegistry>>,
    pub default_project_id: Option<String>,
    pub reconciliation_sweep: ReconciliationSweepConfig,
    pub cache_cleanup: CacheCleanupConfig,
}

impl CoordinatorContext {
    /// Get or spawn a `GitActorHandle` for the given project path.
    pub async fn git_actor(&self, path: &Path) -> Result<GitActorHandle, GitError> {
        let mut map = self.git_actors.lock().await;
        djinn_git::get_or_spawn(&mut map, path)
    }

    /// Register a task as having in-flight post-session background work.
    pub fn register_background_work(&self, task_id: &str) {
        recover_lock(&self.background_work_tasks, "background_work_tasks")
            .insert(task_id.to_string());
    }

    /// Deregister a task's post-session background work (completed or crashed).
    pub fn deregister_background_work(&self, task_id: &str) {
        recover_lock(&self.background_work_tasks, "background_work_tasks").remove(task_id);
    }

    /// Register a task as active and return the shared timestamp atomic.
    /// The atomic is initialized to the current unix timestamp.
    pub fn register_activity(&self, task_id: &str) -> Arc<AtomicU64> {
        let now = SystemClock::new()
            .now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let ts = Arc::new(AtomicU64::new(now));
        recover_lock(&self.active_tasks, "active_tasks").insert(task_id.to_string(), ts.clone());
        ts
    }

    /// Remove a task from the active-tasks tracker.
    pub fn deregister_activity(&self, task_id: &str) {
        recover_lock(&self.active_tasks, "active_tasks").remove(task_id);
    }

    /// Record a stall-check timestamp for a task (overwrites the existing entry).
    pub fn record_heartbeat(&self, task_id: &str) {
        let now = SystemClock::new()
            .now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if let Some(ts) = recover_lock(&self.active_tasks, "active_tasks").get(task_id) {
            ts.store(now, Ordering::Relaxed);
        }
    }
}

/// Acquire a `std::sync::Mutex` guard, recovering from poison with a warning.
///
/// Mutex poisoning only occurs when a previous holder panicked — a programming
/// invariant violation.  The guarded data remains structurally valid, so we log
/// the anomaly and continue operating rather than cascading the panic.
fn recover_lock<'a, T>(
    mutex: &'a std::sync::Mutex<T>,
    label: &'static str,
) -> std::sync::MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::warn!(
                mutex = label,
                "std::sync::Mutex poisoned by prior panic; recovering with data"
            );
            poisoned.into_inner()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_cleanup_config_default_is_safe() {
        let config = CacheCleanupConfig::default();
        assert_eq!(config.mode, CacheCleanupMode::DryRun);
        assert!(config.sccache_enabled);
        assert_eq!(config.sccache_max_age_hours, 24);
        assert!(config.cargo_debris_enabled);
        assert_eq!(config.cargo_debris_max_age_days, 7);
        assert_eq!(config.warm_base_idle_retention_days, 14);
        assert_eq!(config.warm_profile_min_idle_hours, 24);
        assert_eq!(config.warm_base_grace_period, Duration::from_secs(300));
        assert!((config.warm_base_low_free_ratio - 0.15).abs() < f64::EPSILON);
        assert!((config.warm_base_high_free_ratio - 0.25).abs() < f64::EPSILON);
    }

    #[test]
    fn cache_cleanup_config_from_env_ignores_invalid_numbers() {
        // Set invalid values and ensure we fall back to defaults, not crash.
        unsafe {
            std::env::set_var("DJINN_CACHE_CLEANUP_WARM_BASE_IDLE_RETENTION_DAYS", "abc");
            std::env::set_var("DJINN_CACHE_CLEANUP_WARM_BASE_GRACE_PERIOD_SECS", "-1");
            std::env::set_var("DJINN_CACHE_CLEANUP_WARM_BASE_LOW_FREE_RATIO", "1.5");
            std::env::set_var("DJINN_CACHE_CLEANUP_WARM_BASE_HIGH_FREE_RATIO", "nope");
        }

        let config = CacheCleanupConfig::from_env();
        assert_eq!(config.warm_base_idle_retention_days, 14);
        assert_eq!(config.warm_base_grace_period, Duration::from_secs(300));
        assert!((config.warm_base_low_free_ratio - 0.15).abs() < f64::EPSILON);
        assert!((config.warm_base_high_free_ratio - 0.25).abs() < f64::EPSILON);

        unsafe {
            std::env::remove_var("DJINN_CACHE_CLEANUP_WARM_BASE_IDLE_RETENTION_DAYS");
            std::env::remove_var("DJINN_CACHE_CLEANUP_WARM_BASE_GRACE_PERIOD_SECS");
            std::env::remove_var("DJINN_CACHE_CLEANUP_WARM_BASE_LOW_FREE_RATIO");
            std::env::remove_var("DJINN_CACHE_CLEANUP_WARM_BASE_HIGH_FREE_RATIO");
        }
    }

    #[test]
    fn cache_cleanup_config_from_env_low_is_capped_to_high() {
        unsafe {
            std::env::set_var("DJINN_CACHE_CLEANUP_WARM_BASE_LOW_FREE_RATIO", "0.9");
            std::env::set_var("DJINN_CACHE_CLEANUP_WARM_BASE_HIGH_FREE_RATIO", "0.2");
        }

        let config = CacheCleanupConfig::from_env();
        // Low should be clamped to high so the stop-at-high-watermark semantics
        // are always defined.
        assert!((config.warm_base_low_free_ratio - 0.2).abs() < f64::EPSILON);
        assert!((config.warm_base_high_free_ratio - 0.2).abs() < f64::EPSILON);

        unsafe {
            std::env::remove_var("DJINN_CACHE_CLEANUP_WARM_BASE_LOW_FREE_RATIO");
            std::env::remove_var("DJINN_CACHE_CLEANUP_WARM_BASE_HIGH_FREE_RATIO");
        }
    }

    #[test]
    fn cache_cleanup_config_validates_watermarks() {
        let valid = CacheCleanupConfig::default();
        assert!(valid.validate_warm_base_watermarks().is_ok());

        let mut low_invalid = valid.clone();
        low_invalid.warm_base_low_free_ratio = -0.1;
        assert!(low_invalid.validate_warm_base_watermarks().is_err());

        let mut high_invalid = valid.clone();
        high_invalid.warm_base_high_free_ratio = 1.0;
        assert!(high_invalid.validate_warm_base_watermarks().is_err());

        let mut inverted = valid.clone();
        inverted.warm_base_low_free_ratio = 0.5;
        inverted.warm_base_high_free_ratio = 0.3;
        assert!(inverted.validate_warm_base_watermarks().is_err());
    }

    #[test]
    fn cache_cleanup_mode_from_env_value_delete() {
        assert_eq!(
            CacheCleanupMode::from_env_value("delete"),
            CacheCleanupMode::Delete
        );
        assert_eq!(
            CacheCleanupMode::from_env_value("DELETE"),
            CacheCleanupMode::Delete
        );
        assert_eq!(
            CacheCleanupMode::from_env_value("Delete"),
            CacheCleanupMode::Delete
        );
        assert_eq!(
            CacheCleanupMode::from_env_value(" delete "),
            CacheCleanupMode::Delete
        );
    }

    #[test]
    fn cache_cleanup_mode_from_env_value_dry_run_fallback() {
        assert_eq!(
            CacheCleanupMode::from_env_value("dry_run"),
            CacheCleanupMode::DryRun
        );
        assert_eq!(
            CacheCleanupMode::from_env_value(""),
            CacheCleanupMode::DryRun
        );
        assert_eq!(
            CacheCleanupMode::from_env_value("yes"),
            CacheCleanupMode::DryRun
        );
        assert_eq!(
            CacheCleanupMode::from_env_value("true"),
            CacheCleanupMode::DryRun
        );
        assert_eq!(
            CacheCleanupMode::from_env_value("random"),
            CacheCleanupMode::DryRun
        );
    }

    #[test]
    fn cache_cleanup_mode_is_delete() {
        assert!(CacheCleanupMode::Delete.is_delete());
        assert!(!CacheCleanupMode::DryRun.is_delete());
    }

    #[test]
    fn cache_cleanup_mode_as_metric_label() {
        assert_eq!(CacheCleanupMode::DryRun.as_metric_label(), "dry_run");
        assert_eq!(CacheCleanupMode::Delete.as_metric_label(), "delete");
    }

    #[test]
    fn parse_bool_env_recognizes_truthy_values() {
        assert!(parse_bool_env("true"));
        assert!(parse_bool_env("TRUE"));
        assert!(parse_bool_env("1"));
        assert!(parse_bool_env("yes"));
        assert!(parse_bool_env("YES"));
        assert!(parse_bool_env(" true "));
        assert!(!parse_bool_env("false"));
        assert!(!parse_bool_env("0"));
        assert!(!parse_bool_env("no"));
        assert!(!parse_bool_env(""));
        assert!(!parse_bool_env("random"));
    }

    #[test]
    fn proposal_spec_integrity_sweep_defaults_to_disabled() {
        unsafe { std::env::remove_var("DJINN_PROPOSAL_SPEC_INTEGRITY_SWEEP_V1") };
        assert!(!ProposalSpecIntegritySweepConfig::default().enabled);
        assert!(!ProposalSpecIntegritySweepConfig::from_env().enabled);
        unsafe { std::env::set_var("DJINN_PROPOSAL_SPEC_INTEGRITY_SWEEP_V1", "true") };
        assert!(ProposalSpecIntegritySweepConfig::from_env().enabled);
        unsafe { std::env::remove_var("DJINN_PROPOSAL_SPEC_INTEGRITY_SWEEP_V1") };
    }
}

#[cfg(test)]
mod warm_profile_idle_config_tests {
    use super::*;

    #[test]
    fn warm_profile_idle_accepts_zero_and_rejects_non_decimal_values() {
        let variable = "DJINN_CACHE_CLEANUP_WARM_PROFILE_MIN_IDLE_HOURS";
        for (value, expected) in [
            ("0", 0),
            ("24", 24),
            ("-1", 24),
            ("24h", 24),
            (" 24", 24),
            ("+24", 24),
            ("18446744073709551616", 24),
        ] {
            unsafe { std::env::set_var(variable, value) };
            assert_eq!(
                CacheCleanupConfig::from_env().warm_profile_min_idle_hours,
                expected
            );
        }
        unsafe { std::env::remove_var(variable) };
    }
}
