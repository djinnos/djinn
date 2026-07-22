//! Retrieval-health doctor checks.
//!
//! This module provides the database-independent, pure-synchronous core of the
//! retrieval-health detector. All database access and control-plane wiring live
//! outside `djinn-core`; this crate only defines the immutable config, snapshot
//! types, data-source abstraction, and the `DoctorCheck` implementation.
//!
//! The check emits [`memory.retrieval_zero_result`](RETRIEVAL_ZERO_RESULT_NAME)
//! per project only when the query count in the window is at least the
//! configured floor and the zero-result rate is strictly greater than the
//! configured threshold. Equality against the threshold is considered healthy.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;
use time::OffsetDateTime;

use crate::models::KnowledgeInjectionConfig;

use crate::doctor::{
    DoctorCheck, DoctorCheckCadence, DoctorResult, Finding, FindingSeverity, ResolverSnapshot,
};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Minimum supported retrieval-health window, in whole hours.
pub const MIN_WINDOW_HOURS: u64 = 1;

/// Maximum supported retrieval-health window, in whole hours (7 days).
pub const MAX_WINDOW_HOURS: u64 = 168;

/// Minimum supported zero-result threshold (inclusive).
pub const MIN_ZERO_RESULT_THRESHOLD: f64 = 0.0;

/// Maximum supported zero-result threshold (inclusive).
pub const MAX_ZERO_RESULT_THRESHOLD: f64 = 1.0;

/// Minimum supported query floor (inclusive).
pub const MIN_QUERY_FLOOR: u64 = 1;

/// Maximum supported query floor (inclusive).
pub const MAX_QUERY_FLOOR: u64 = 100_000;

/// Default retrieval-health window: 24 hours.
pub const DEFAULT_WINDOW_HOURS: u64 = 24;

/// Default zero-result threshold: 0.50.
pub const DEFAULT_ZERO_RESULT_THRESHOLD: f64 = 0.50;

/// Default query floor: 20 queries.
pub const DEFAULT_QUERY_FLOOR: u64 = 20;

/// Errors returned when constructing an invalid [`RetrievalHealthConfig`].
#[derive(Debug, Error, PartialEq)]
pub enum RetrievalHealthConfigError {
    /// `window_hours` was outside the documented bounds.
    #[error(
        "window_hours {0} is outside the supported range [{MIN_WINDOW_HOURS}, {MAX_WINDOW_HOURS}]"
    )]
    WindowHours(u64),

    /// `zero_result_threshold` was outside the documented `[0.0, 1.0]` bounds.
    #[error(
        "zero_result_threshold {0} is outside the supported range [{MIN_ZERO_RESULT_THRESHOLD}, {MAX_ZERO_RESULT_THRESHOLD}]"
    )]
    Threshold(f64),

    /// `query_floor` was outside the documented bounds.
    #[error(
        "query_floor {0} is outside the supported range [{MIN_QUERY_FLOOR}, {MAX_QUERY_FLOOR}]"
    )]
    QueryFloor(u64),

    /// An environment variable was set to a non-numeric value.
    #[error("environment variable {var} has invalid value {value}")]
    InvalidEnvValue { var: String, value: String },
}

/// Immutable configuration for retrieval-health checks.
///
/// All values are validated at construction time. Defaults are a 24-hour
/// window, 0.50 zero-result threshold, and a 20-query floor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetrievalHealthConfig {
    window_hours: u64,
    zero_result_threshold: f64,
    query_floor: u64,
}

impl RetrievalHealthConfig {
    /// Construct a config from validated values.
    ///
    /// Returns a typed error for any value outside the documented bounds:
    ///
    /// * `window_hours`: [`MIN_WINDOW_HOURS`] to [`MAX_WINDOW_HOURS`]
    /// * `zero_result_threshold`: [`MIN_ZERO_RESULT_THRESHOLD`] to
    ///   [`MAX_ZERO_RESULT_THRESHOLD`]
    /// * `query_floor`: [`MIN_QUERY_FLOOR`] to [`MAX_QUERY_FLOOR`]
    pub fn new(
        window_hours: u64,
        zero_result_threshold: f64,
        query_floor: u64,
    ) -> Result<Self, RetrievalHealthConfigError> {
        if !(MIN_WINDOW_HOURS..=MAX_WINDOW_HOURS).contains(&window_hours) {
            return Err(RetrievalHealthConfigError::WindowHours(window_hours));
        }
        if !(MIN_ZERO_RESULT_THRESHOLD..=MAX_ZERO_RESULT_THRESHOLD).contains(&zero_result_threshold)
        {
            return Err(RetrievalHealthConfigError::Threshold(zero_result_threshold));
        }
        if !(MIN_QUERY_FLOOR..=MAX_QUERY_FLOOR).contains(&query_floor) {
            return Err(RetrievalHealthConfigError::QueryFloor(query_floor));
        }
        Ok(Self {
            window_hours,
            zero_result_threshold,
            query_floor,
        })
    }

    /// The configured window, in whole hours.
    pub fn window_hours(&self) -> u64 {
        self.window_hours
    }

    /// The configured zero-result threshold.
    pub fn zero_result_threshold(&self) -> f64 {
        self.zero_result_threshold
    }

    /// The configured minimum query floor before a finding may be emitted.
    pub fn query_floor(&self) -> u64 {
        self.query_floor
    }

    /// Parse a config from the documented environment variables.
    ///
    /// Variables:
    /// * `DJINN_RETRIEVAL_HEALTH_WINDOW_HOURS` — defaults to 24.
    /// * `DJINN_RETRIEVAL_ZERO_RESULT_THRESHOLD` — defaults to 0.50.
    /// * `DJINN_RETRIEVAL_MINIMUM_QUERIES` — defaults to 20.
    ///
    /// `DJINN_RETRIEVAL_HEALTH_ZERO_RESULT_THRESHOLD` and
    /// `DJINN_RETRIEVAL_HEALTH_QUERY_FLOOR` remain supported as deprecated
    /// fallback aliases. When both a canonical variable and its alias are set,
    /// the canonical value is selected and the alias is ignored.
    ///
    /// Any selected value outside the documented bounds returns an error.
    pub fn from_env() -> Result<Self, RetrievalHealthConfigError> {
        let window_hours = parse_env_u64(
            "DJINN_RETRIEVAL_HEALTH_WINDOW_HOURS",
            None,
            DEFAULT_WINDOW_HOURS,
        )?;
        let zero_result_threshold = parse_env_f64(
            "DJINN_RETRIEVAL_ZERO_RESULT_THRESHOLD",
            Some("DJINN_RETRIEVAL_HEALTH_ZERO_RESULT_THRESHOLD"),
            DEFAULT_ZERO_RESULT_THRESHOLD,
        )?;
        let query_floor = parse_env_u64(
            "DJINN_RETRIEVAL_MINIMUM_QUERIES",
            Some("DJINN_RETRIEVAL_HEALTH_QUERY_FLOOR"),
            DEFAULT_QUERY_FLOOR,
        )?;
        Self::new(window_hours, zero_result_threshold, query_floor)
    }
}

impl Default for RetrievalHealthConfig {
    fn default() -> Self {
        Self::new(
            DEFAULT_WINDOW_HOURS,
            DEFAULT_ZERO_RESULT_THRESHOLD,
            DEFAULT_QUERY_FLOOR,
        )
        .expect("defaults are within documented bounds")
    }
}

fn parse_env_u64(
    canonical_var: &str,
    alias_var: Option<&str>,
    default: u64,
) -> Result<u64, RetrievalHealthConfigError> {
    match selected_env_value(canonical_var, alias_var) {
        Some((var, value)) => value
            .parse::<u64>()
            .map_err(|_| RetrievalHealthConfigError::InvalidEnvValue { var, value }),
        None => Ok(default),
    }
}

fn parse_env_f64(
    canonical_var: &str,
    alias_var: Option<&str>,
    default: f64,
) -> Result<f64, RetrievalHealthConfigError> {
    match selected_env_value(canonical_var, alias_var) {
        Some((var, value)) => value
            .parse::<f64>()
            .map_err(|_| RetrievalHealthConfigError::InvalidEnvValue { var, value }),
        None => Ok(default),
    }
}

/// Select the canonical environment variable when present, otherwise its
/// deprecated fallback alias. This deliberately avoids parsing an alias when a
/// canonical value was selected.
fn selected_env_value(canonical_var: &str, alias_var: Option<&str>) -> Option<(String, String)> {
    std::env::var(canonical_var)
        .ok()
        .map(|value| (canonical_var.to_string(), value))
        .or_else(|| {
            alias_var.and_then(|alias_var| {
                std::env::var(alias_var)
                    .ok()
                    .map(|value| (alias_var.to_string(), value))
            })
        })
}

// ---------------------------------------------------------------------------
// Snapshot types
// ---------------------------------------------------------------------------

/// Counts for a single retrieval entry point within a project window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryPointCounts {
    /// Total number of completed queries for this entry point.
    pub total_queries: u64,
    /// Number of completed queries that returned zero results.
    pub zero_result_queries: u64,
}

impl EntryPointCounts {
    /// Zero counts, useful when an entry point is present but had no traffic.
    pub const fn zero() -> Self {
        Self {
            total_queries: 0,
            zero_result_queries: 0,
        }
    }
}

/// Immutable snapshot of retrieval-health counts for a single project over an
/// exact window.
#[derive(Debug, Clone, PartialEq)]
pub struct RetrievalProjectWindowSnapshot {
    /// Project identifier the snapshot covers.
    pub project_id: String,
    /// Inclusive start of the half-open window.
    pub window_start: OffsetDateTime,
    /// Exclusive end of the half-open window.
    pub window_end: OffsetDateTime,
    /// Per-entry-point counts within the window.
    pub entry_point_counts: BTreeMap<String, EntryPointCounts>,
}

/// Immutable snapshot covering all projects for a single check invocation.
///
/// The control plane is responsible for prefetching the per-project rollups and
/// adapting them into this shape before invoking the pure core check.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RetrievalHealthSnapshot {
    /// Project snapshots keyed by project id.
    pub projects: BTreeMap<String, RetrievalProjectWindowSnapshot>,
}

// ---------------------------------------------------------------------------
// Data source
// ---------------------------------------------------------------------------

/// Synchronous, immutable data source for retrieval health.
///
/// Implementations are expected to return an already-prefetched snapshot; the
/// check itself performs no I/O. This keeps `djinn-core` free of database and
/// control-plane dependencies while still allowing a production adapter to feed
/// the check from `djinn-db` rollups and process metrics.
pub trait RetrievalHealthDataSource: Send + Sync {
    /// Return the immutable snapshot for the current check invocation.
    fn snapshot(&self) -> RetrievalHealthSnapshot;
}

// ---------------------------------------------------------------------------
// Check
// ---------------------------------------------------------------------------

/// Stable name for the zero-result retrieval finding.
pub const RETRIEVAL_ZERO_RESULT_NAME: &str = "memory.retrieval_zero_result";

/// Pure synchronous doctor check that flags projects whose zero-result rate is
/// strictly above the configured threshold over the configured window.
///
/// The check takes an immutable config and a synchronous data source. It is
/// deliberately on-demand (not cheap-periodic) because each invocation may
/// require a fresh prefetched snapshot from the control plane.
pub struct RetrievalZeroResultCheck {
    config: RetrievalHealthConfig,
    source: Arc<dyn RetrievalHealthDataSource>,
}

impl RetrievalZeroResultCheck {
    /// Construct the check.
    pub fn new(config: RetrievalHealthConfig, source: Arc<dyn RetrievalHealthDataSource>) -> Self {
        Self { config, source }
    }
}

impl DoctorCheck for RetrievalZeroResultCheck {
    fn name(&self) -> &'static str {
        RETRIEVAL_ZERO_RESULT_NAME
    }

    fn description(&self) -> &'static str {
        "Flags projects whose memory retrieval zero-result rate is strictly above the configured threshold"
    }

    fn run(&self) -> DoctorResult<Vec<Finding>> {
        let snapshot = self.source.snapshot();
        let mut findings = Vec::with_capacity(snapshot.projects.len());
        for project in snapshot.projects.values() {
            if let Some(finding) = resolve_retrieval_zero_result(project, &self.config) {
                findings.push(finding);
            }
        }
        Ok(findings)
    }

    fn cadence(&self) -> DoctorCheckCadence {
        DoctorCheckCadence::OnDemand
    }
}

// ---------------------------------------------------------------------------
// Resolver
// ---------------------------------------------------------------------------

fn resolve_retrieval_zero_result(
    project: &RetrievalProjectWindowSnapshot,
    config: &RetrievalHealthConfig,
) -> Option<Finding> {
    let mut total_queries: u64 = 0;
    let mut zero_result_queries: u64 = 0;
    for counts in project.entry_point_counts.values() {
        total_queries += counts.total_queries;
        zero_result_queries += counts.zero_result_queries;
    }

    let below_floor = total_queries < config.query_floor();
    let rate = if total_queries > 0 {
        zero_result_queries as f64 / total_queries as f64
    } else {
        0.0
    };
    // Strictly greater than threshold; equality passes.
    let above_threshold = rate > config.zero_result_threshold();

    let entry_point_counts_json = serde_json::to_value(&project.entry_point_counts)
        .expect("EntryPointCounts serializes to JSON");

    let inputs = json!({
        "project_id": project.project_id,
        "window_start": iso_format(project.window_start),
        "window_end": iso_format(project.window_end),
        "threshold": config.zero_result_threshold(),
        "floor": config.query_floor(),
        "entry_point_counts": entry_point_counts_json,
        "total_queries": total_queries,
        "zero_result_queries": zero_result_queries,
        "rate": rate,
        "below_floor": below_floor,
        "above_threshold": above_threshold,
    });

    let outputs = json!({
        "below_floor": below_floor,
        "above_threshold": above_threshold,
        "rate": rate,
        "total_queries": total_queries,
        "zero_result_queries": zero_result_queries,
    });

    if below_floor || !above_threshold {
        return None;
    }

    let evidence = json!({
        "project_id": project.project_id,
        "window": {
            "start": iso_format(project.window_start),
            "end": iso_format(project.window_end),
        },
        "threshold": config.zero_result_threshold(),
        "floor": config.query_floor(),
        "numerator": zero_result_queries,
        "denominator": total_queries,
        "rate": rate,
        "per_entry_point_counts": entry_point_counts_json,
    });

    Some(
        Finding::new(
            FindingSeverity::Warn,
            RETRIEVAL_ZERO_RESULT_NAME,
            ResolverSnapshot::new("resolve_retrieval_zero_result", inputs, outputs),
            format!(
                "project {} has a zero-result rate of {:.4} ({} / {}), above the {} threshold over the last {} hours",
                project.project_id,
                rate,
                zero_result_queries,
                total_queries,
                config.zero_result_threshold(),
                config.window_hours()
            ),
        )
        .with_entity_id("project_id", &project.project_id)
        .with_evidence(evidence),
    )
}

/// Format an [`OffsetDateTime`] as an ISO-8601 string for JSON snapshots.
fn iso_format(ts: OffsetDateTime) -> String {
    ts.format(&time::format_description::well_known::Iso8601::DEFAULT)
        .unwrap_or_else(|_| ts.unix_timestamp().to_string())
}

// ---------------------------------------------------------------------------
// Taxonomy-v1 immutable snapshots and pure alarms
// ---------------------------------------------------------------------------

pub const RETRIEVAL_TAXONOMY_V1: &str = "taxonomy-v1";
pub const LOAD_KNOWLEDGE_CONTEXT_ENTRY_POINT: &str = "load_knowledge_context";
pub const INJECTION_STARVATION_NAME: &str = "memory.injection_starvation";

/// Complete five-way terminal disposition histogram from a versioned rollup.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaxonomyV1DispositionHistogram {
    pub succeeded: u64, pub errored: u64, pub cancelled: u64, pub disabled: u64, pub unknown: u64,
}
/// Versioned counters which never include legacy or malformed taxonomy rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaxonomyV1QueryCounters {
    pub total_queries: u64, pub successful_queries: u64, pub errored_queries: u64,
    pub cancelled_queries: u64, pub zero_candidate_queries: u64, pub candidate_bearing_queries: u64,
    pub starved_queries: u64, pub injected_queries: u64,
}
/// One independently keyed valid project/entry-point rollup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaxonomyV1ValidGroupSnapshot {
    pub project_id: String, pub entry_point: String, pub taxonomy_version: String,
    pub window_start: OffsetDateTime, pub window_end: OffsetDateTime, pub refreshed_at: OffsetDateTime,
    pub counters: TaxonomyV1QueryCounters, pub candidate_total: u64, pub injected_total: u64,
    pub dispositions: TaxonomyV1DispositionHistogram, pub legacy_unclassified_queries: u64,
    pub invalid_taxonomy_queries: u64,
}
/// Identity and exclusions for a malformed group. Such a group is never evaluated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaxonomyV1InvalidGroupSnapshot {
    pub project_id: String, pub entry_point: String, pub taxonomy_version: String,
    pub window_start: OffsetDateTime, pub window_end: OffsetDateTime, pub refreshed_at: OffsetDateTime,
    pub legacy_unclassified_queries: u64, pub invalid_taxonomy_queries: u64, pub invalid_reason: String,
}
impl TaxonomyV1InvalidGroupSnapshot { pub fn finding_key(&self, check: &str) -> String { format!("{check}:{}:{}", self.project_id, self.entry_point) } }
/// An atomically-populated refresh; valid siblings remain evaluable beside invalid groups.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaxonomyV1RetrievalSnapshot { pub valid_groups: Vec<TaxonomyV1ValidGroupSnapshot>, pub invalid_groups: Vec<TaxonomyV1InvalidGroupSnapshot> }

/// Pure zero-candidate resolver. Invalid groups are intentionally absent.
pub fn resolve_retrieval_zero_result_v1(snapshot: &TaxonomyV1RetrievalSnapshot, config: KnowledgeInjectionConfig) -> Vec<Finding> {
    snapshot.valid_groups.iter().filter(|g| g.taxonomy_version == RETRIEVAL_TAXONOMY_V1).filter_map(|g| resolve_v1(g, config, RETRIEVAL_ZERO_RESULT_NAME, g.counters.zero_candidate_queries, g.counters.successful_queries, "resolve_retrieval_zero_result_v1")).collect()
}
/// Pure starvation resolver restricted to load_knowledge_context candidate-bearing queries.
pub fn resolve_injection_starvation_v1(snapshot: &TaxonomyV1RetrievalSnapshot, config: KnowledgeInjectionConfig) -> Vec<Finding> {
    snapshot.valid_groups.iter().filter(|g| g.taxonomy_version == RETRIEVAL_TAXONOMY_V1 && g.entry_point == LOAD_KNOWLEDGE_CONTEXT_ENTRY_POINT).filter_map(|g| resolve_v1(g, config, INJECTION_STARVATION_NAME, g.counters.starved_queries, g.counters.candidate_bearing_queries, "resolve_injection_starvation_v1")).collect()
}
fn resolve_v1(g: &TaxonomyV1ValidGroupSnapshot, config: KnowledgeInjectionConfig, name: &'static str, numerator: u64, denominator: u64, resolver: &'static str) -> Option<Finding> {
    let triggered = denominator >= u64::from(config.injection_starvation_query_floor) && numerator > 0 && u128::from(numerator) * 100 >= u128::from(denominator) * u128::from(config.injection_starvation_threshold_percent);
    if !triggered { return None; }
    let ratio = format!("{numerator}/{denominator}"); let key = format!("{name}:{}:{}", g.project_id, g.entry_point);
    let evidence = json!({"finding_key":key,"project_id":g.project_id,"entry_point":g.entry_point,"taxonomy_version":g.taxonomy_version,"window":{"start":iso_format(g.window_start),"end":iso_format(g.window_end)},"refreshed_at":iso_format(g.refreshed_at),"numerator":numerator,"denominator":denominator,"exact_ratio":ratio,"configured_threshold_percent":config.injection_starvation_threshold_percent,"configured_query_floor":config.injection_starvation_query_floor,"configured_window_minutes":config.retrieval_health_window_minutes,"query_counters":g.counters,"candidate_total":g.candidate_total,"injected_total":g.injected_total,"dispositions":g.dispositions,"legacy_unclassified_queries":g.legacy_unclassified_queries,"invalid_taxonomy_queries":g.invalid_taxonomy_queries});
    Some(Finding::new(FindingSeverity::Warn, name, ResolverSnapshot::new(resolver, evidence.clone(), json!({"triggered":true,"exact_ratio":ratio})), format!("{name} {key} is {ratio}")).with_entity_id("project_id", &g.project_id).with_entity_id("entry_point", &g.entry_point).with_entity_id("finding_key", key).with_evidence(evidence))
}
/// Cheap wrapper around the zero-candidate pure resolver.
pub struct TaxonomyV1RetrievalZeroResultCheck { config: KnowledgeInjectionConfig, snapshot: TaxonomyV1RetrievalSnapshot }
impl TaxonomyV1RetrievalZeroResultCheck { pub fn new(config: KnowledgeInjectionConfig, snapshot: TaxonomyV1RetrievalSnapshot) -> Self { Self { config, snapshot } } }
impl DoctorCheck for TaxonomyV1RetrievalZeroResultCheck { fn name(&self)->&'static str { RETRIEVAL_ZERO_RESULT_NAME } fn description(&self)->&'static str { "Flags taxonomy-v1 successful retrieval queries with zero candidates" } fn run(&self)->DoctorResult<Vec<Finding>> { Ok(resolve_retrieval_zero_result_v1(&self.snapshot,self.config)) } fn cadence(&self)->DoctorCheckCadence { DoctorCheckCadence::Cheap } }
/// Cheap wrapper around the load_knowledge_context starvation resolver.
pub struct InjectionStarvationCheck { config: KnowledgeInjectionConfig, snapshot: TaxonomyV1RetrievalSnapshot }
impl InjectionStarvationCheck { pub fn new(config: KnowledgeInjectionConfig, snapshot: TaxonomyV1RetrievalSnapshot) -> Self { Self { config, snapshot } } }
impl DoctorCheck for InjectionStarvationCheck { fn name(&self)->&'static str { INJECTION_STARVATION_NAME } fn description(&self)->&'static str { "Flags starved load_knowledge_context candidate-bearing queries" } fn run(&self)->DoctorResult<Vec<Finding>> { Ok(resolve_injection_starvation_v1(&self.snapshot,self.config)) } fn cadence(&self)->DoctorCheckCadence { DoctorCheckCadence::Cheap } }

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    // -----------------------------------------------------------------
    // In-memory test double
    // -----------------------------------------------------------------

    #[derive(Default)]
    struct MemoryDataSource {
        snapshot: RetrievalHealthSnapshot,
    }

    impl RetrievalHealthDataSource for MemoryDataSource {
        fn snapshot(&self) -> RetrievalHealthSnapshot {
            self.snapshot.clone()
        }
    }

    fn ts(hour: i64) -> OffsetDateTime {
        // Use a fixed base date so tests are deterministic.
        OffsetDateTime::from_unix_timestamp(1_704_000_000).unwrap() + time::Duration::hours(hour)
    }

    fn make_project(
        project_id: &str,
        total: u64,
        zero: u64,
        entry_points: Vec<(&str, u64, u64)>,
    ) -> RetrievalProjectWindowSnapshot {
        let mut counts = BTreeMap::new();
        if entry_points.is_empty() {
            counts.insert(
                "memory_search".to_string(),
                EntryPointCounts {
                    total_queries: total,
                    zero_result_queries: zero,
                },
            );
        } else {
            for (name, ep_total, ep_zero) in entry_points {
                counts.insert(
                    name.to_string(),
                    EntryPointCounts {
                        total_queries: ep_total,
                        zero_result_queries: ep_zero,
                    },
                );
            }
        }
        RetrievalProjectWindowSnapshot {
            project_id: project_id.to_string(),
            window_start: ts(0),
            window_end: ts(24),
            entry_point_counts: counts,
        }
    }

    fn make_project_with_total_zero(
        project_id: &str,
        total: u64,
        zero: u64,
    ) -> RetrievalProjectWindowSnapshot {
        make_project(project_id, total, zero, vec![])
    }

    fn run_check(
        config: RetrievalHealthConfig,
        projects: Vec<RetrievalProjectWindowSnapshot>,
    ) -> Vec<Finding> {
        let mut snapshot = RetrievalHealthSnapshot::default();
        for project in projects {
            snapshot
                .projects
                .insert(project.project_id.clone(), project);
        }
        let source: Arc<dyn RetrievalHealthDataSource> = Arc::new(MemoryDataSource { snapshot });
        let check = RetrievalZeroResultCheck::new(config, source);
        check.run().expect("run should succeed")
    }

    // -----------------------------------------------------------------
    // Config validation
    // -----------------------------------------------------------------

    #[test]
    fn defaults_match_spec() {
        let config = RetrievalHealthConfig::default();
        assert_eq!(config.window_hours(), 24);
        assert_eq!(config.zero_result_threshold(), 0.50);
        assert_eq!(config.query_floor(), 20);
    }

    #[test]
    fn valid_config_endpoints() {
        assert!(RetrievalHealthConfig::new(1, 0.0, 1).is_ok());
        assert!(RetrievalHealthConfig::new(168, 1.0, 100_000).is_ok());
    }

    #[test]
    fn invalid_window_hours_bounds() {
        assert_eq!(
            RetrievalHealthConfig::new(0, DEFAULT_ZERO_RESULT_THRESHOLD, DEFAULT_QUERY_FLOOR),
            Err(RetrievalHealthConfigError::WindowHours(0))
        );
        assert_eq!(
            RetrievalHealthConfig::new(169, DEFAULT_ZERO_RESULT_THRESHOLD, DEFAULT_QUERY_FLOOR),
            Err(RetrievalHealthConfigError::WindowHours(169))
        );
    }

    #[test]
    fn invalid_threshold_bounds() {
        assert_eq!(
            RetrievalHealthConfig::new(DEFAULT_WINDOW_HOURS, -0.1, DEFAULT_QUERY_FLOOR),
            Err(RetrievalHealthConfigError::Threshold(-0.1))
        );
        assert_eq!(
            RetrievalHealthConfig::new(DEFAULT_WINDOW_HOURS, 1.1, DEFAULT_QUERY_FLOOR),
            Err(RetrievalHealthConfigError::Threshold(1.1))
        );
    }

    #[test]
    fn invalid_query_floor_bounds() {
        assert_eq!(
            RetrievalHealthConfig::new(DEFAULT_WINDOW_HOURS, DEFAULT_ZERO_RESULT_THRESHOLD, 0),
            Err(RetrievalHealthConfigError::QueryFloor(0))
        );
        assert_eq!(
            RetrievalHealthConfig::new(
                DEFAULT_WINDOW_HOURS,
                DEFAULT_ZERO_RESULT_THRESHOLD,
                100_001
            ),
            Err(RetrievalHealthConfigError::QueryFloor(100_001))
        );
    }

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn clear_env_vars() {
        for var in [
            "DJINN_RETRIEVAL_HEALTH_WINDOW_HOURS",
            "DJINN_RETRIEVAL_ZERO_RESULT_THRESHOLD",
            "DJINN_RETRIEVAL_MINIMUM_QUERIES",
            "DJINN_RETRIEVAL_HEALTH_ZERO_RESULT_THRESHOLD",
            "DJINN_RETRIEVAL_HEALTH_QUERY_FLOOR",
        ] {
            unsafe {
                let _ = std::env::remove_var(var);
            }
        }
    }

    fn set_env_vars(window: &str, threshold: &str, floor: &str) {
        clear_env_vars();
        unsafe {
            std::env::set_var("DJINN_RETRIEVAL_HEALTH_WINDOW_HOURS", window);
            std::env::set_var("DJINN_RETRIEVAL_ZERO_RESULT_THRESHOLD", threshold);
            std::env::set_var("DJINN_RETRIEVAL_MINIMUM_QUERIES", floor);
        }
    }

    fn set_alias_env_vars(threshold: &str, floor: &str) {
        clear_env_vars();
        unsafe {
            std::env::set_var("DJINN_RETRIEVAL_HEALTH_ZERO_RESULT_THRESHOLD", threshold);
            std::env::set_var("DJINN_RETRIEVAL_HEALTH_QUERY_FLOOR", floor);
        }
    }

    #[test]
    fn from_env_uses_defaults() {
        let _guard = env_lock();
        clear_env_vars();
        let config = RetrievalHealthConfig::from_env().unwrap();
        assert_eq!(config.window_hours(), DEFAULT_WINDOW_HOURS);
        assert_eq!(
            config.zero_result_threshold(),
            DEFAULT_ZERO_RESULT_THRESHOLD
        );
        assert_eq!(config.query_floor(), DEFAULT_QUERY_FLOOR);
    }

    #[test]
    fn from_env_parses_custom_values() {
        let _guard = env_lock();
        set_env_vars("48", "0.75", "50");
        let config = RetrievalHealthConfig::from_env().unwrap();
        assert_eq!(config.window_hours(), 48);
        assert_eq!(config.zero_result_threshold(), 0.75);
        assert_eq!(config.query_floor(), 50);
    }

    #[test]
    fn from_env_uses_deprecated_aliases_as_fallback() {
        let _guard = env_lock();
        set_alias_env_vars("0.75", "50");
        let config = RetrievalHealthConfig::from_env().unwrap();
        assert_eq!(config.window_hours(), DEFAULT_WINDOW_HOURS);
        assert_eq!(config.zero_result_threshold(), 0.75);
        assert_eq!(config.query_floor(), 50);
    }

    #[test]
    fn from_env_canonical_values_win_over_invalid_aliases() {
        let _guard = env_lock();
        set_env_vars("48", "0.75", "50");
        unsafe {
            std::env::set_var("DJINN_RETRIEVAL_HEALTH_ZERO_RESULT_THRESHOLD", "invalid");
            std::env::set_var("DJINN_RETRIEVAL_HEALTH_QUERY_FLOOR", "0");
        }
        let config = RetrievalHealthConfig::from_env().unwrap();
        assert_eq!(config.window_hours(), 48);
        assert_eq!(config.zero_result_threshold(), 0.75);
        assert_eq!(config.query_floor(), 50);
    }

    #[test]
    fn from_env_rejects_invalid_window_hours() {
        let _guard = env_lock();
        set_env_vars("0", "0.5", "20");
        assert_eq!(
            RetrievalHealthConfig::from_env(),
            Err(RetrievalHealthConfigError::WindowHours(0))
        );
    }

    #[test]
    fn from_env_rejects_invalid_threshold() {
        let _guard = env_lock();
        set_env_vars("24", "1.1", "20");
        assert_eq!(
            RetrievalHealthConfig::from_env(),
            Err(RetrievalHealthConfigError::Threshold(1.1))
        );
    }

    #[test]
    fn from_env_rejects_invalid_query_floor() {
        let _guard = env_lock();
        set_env_vars("24", "0.5", "0");
        assert_eq!(
            RetrievalHealthConfig::from_env(),
            Err(RetrievalHealthConfigError::QueryFloor(0))
        );
    }

    #[test]
    fn from_env_rejects_query_floor_above_signed_off_limit() {
        let _guard = env_lock();
        set_env_vars("24", "0.5", "100001");
        assert_eq!(
            RetrievalHealthConfig::from_env(),
            Err(RetrievalHealthConfigError::QueryFloor(100_001))
        );
    }

    #[test]
    fn from_env_rejects_non_numeric() {
        let _guard = env_lock();
        set_env_vars("24", "not-a-number", "20");
        assert!(
            matches!(
                RetrievalHealthConfig::from_env(),
                Err(RetrievalHealthConfigError::InvalidEnvValue { .. })
            ),
            "expected InvalidEnvValue for non-numeric env var"
        );
    }

    // -----------------------------------------------------------------
    // Floor and threshold behavior
    // -----------------------------------------------------------------

    #[test]
    fn at_floor_with_above_threshold_rate_emits() {
        // floor = 20, total = 20, zero = 11 -> rate 0.55 > 0.5
        let config = RetrievalHealthConfig::new(24, 0.5, 20).unwrap();
        let findings = run_check(config, vec![make_project_with_total_zero("p1", 20, 11)]);
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn below_floor_does_not_emit() {
        // floor = 20, total = 19, zero = 19 -> rate 1.0 but below floor.
        let config = RetrievalHealthConfig::new(24, 0.5, 20).unwrap();
        let findings = run_check(config, vec![make_project_with_total_zero("p1", 19, 19)]);
        assert!(findings.is_empty());
    }

    #[test]
    fn threshold_equality_passes() {
        // floor = 20, total = 20, zero = 10 -> rate exactly 0.5, equality passes.
        let config = RetrievalHealthConfig::new(24, 0.5, 20).unwrap();
        let findings = run_check(config, vec![make_project_with_total_zero("p1", 20, 10)]);
        assert!(findings.is_empty());
    }

    #[test]
    fn above_threshold_emits() {
        // floor = 20, total = 20, zero = 11 -> rate 0.55 > 0.5.
        let config = RetrievalHealthConfig::new(24, 0.5, 20).unwrap();
        let findings = run_check(config, vec![make_project_with_total_zero("p1", 20, 11)]);
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn below_threshold_does_not_emit() {
        let config = RetrievalHealthConfig::new(24, 0.5, 20).unwrap();
        let findings = run_check(config, vec![make_project_with_total_zero("p1", 20, 9)]);
        assert!(findings.is_empty());
    }

    #[test]
    fn zero_queries_does_not_emit() {
        let config = RetrievalHealthConfig::new(24, 0.5, 1).unwrap();
        let findings = run_check(config, vec![make_project_with_total_zero("p1", 0, 0)]);
        assert!(findings.is_empty());
    }

    // -----------------------------------------------------------------
    // Multiple-project isolation
    // -----------------------------------------------------------------

    #[test]
    fn only_above_threshold_project_yields_finding() {
        let config = RetrievalHealthConfig::new(24, 0.5, 20).unwrap();
        let projects = vec![
            make_project_with_total_zero("healthy", 100, 10), // rate 0.10
            make_project_with_total_zero("sick", 100, 60),    // rate 0.60
        ];
        let findings = run_check(config, projects);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].entity_ids.get("project_id").map(String::as_str),
            Some("sick")
        );
    }

    // -----------------------------------------------------------------
    // Evidence shape
    // -----------------------------------------------------------------

    #[test]
    fn evidence_contains_all_required_fields() {
        let config = RetrievalHealthConfig::new(24, 0.5, 20).unwrap();
        let projects = vec![make_project(
            "p-evidence",
            0,
            0,
            vec![("memory_search", 30, 18), ("memory_build_context", 20, 8)],
        )];
        let findings = run_check(config, projects);
        assert_eq!(findings.len(), 1);

        let f = &findings[0];
        assert_eq!(f.severity, FindingSeverity::Warn);
        assert_eq!(f.check_name, RETRIEVAL_ZERO_RESULT_NAME);
        assert_eq!(
            f.entity_ids.get("project_id").map(String::as_str),
            Some("p-evidence")
        );

        let evidence = &f.evidence;
        assert_eq!(evidence["project_id"], "p-evidence");
        assert!(evidence["window"].is_object());
        assert_eq!(evidence["threshold"], 0.5);
        assert_eq!(evidence["floor"], 20);
        assert_eq!(evidence["numerator"], 26);
        assert_eq!(evidence["denominator"], 50);
        assert!(evidence["rate"].is_number());
        assert!(evidence["per_entry_point_counts"].is_object());
        assert_eq!(
            evidence["per_entry_point_counts"]["memory_search"]["total_queries"],
            30
        );
        assert_eq!(
            evidence["per_entry_point_counts"]["memory_search"]["zero_result_queries"],
            18
        );
        assert_eq!(
            evidence["per_entry_point_counts"]["memory_build_context"]["total_queries"],
            20
        );
        assert_eq!(
            evidence["per_entry_point_counts"]["memory_build_context"]["zero_result_queries"],
            8
        );

        // Rate should be 26/50 = 0.52.
        let rate = evidence["rate"].as_f64().expect("rate is a number");
        assert!((rate - 0.52).abs() < f64::EPSILON);
    }

    #[test]
    fn resolver_snapshot_contains_inputs_and_outputs() {
        let config = RetrievalHealthConfig::new(24, 0.5, 20).unwrap();
        let projects = vec![make_project_with_total_zero("p-snap", 40, 22)];
        let findings = run_check(config, projects);
        assert_eq!(findings.len(), 1);

        let snap = &findings[0].resolver_snapshot;
        assert_eq!(snap.resolver, "resolve_retrieval_zero_result");
        assert_eq!(snap.inputs["project_id"], "p-snap");
        assert_eq!(snap.inputs["total_queries"], 40);
        assert_eq!(snap.inputs["zero_result_queries"], 22);
        assert_eq!(snap.outputs["above_threshold"], true);
        assert_eq!(snap.outputs["below_floor"], false);
    }

    // -----------------------------------------------------------------
    // Cadence
    // -----------------------------------------------------------------

    #[test]
    fn check_is_on_demand() {
        let config = RetrievalHealthConfig::default();
        let source: Arc<dyn RetrievalHealthDataSource> = Arc::new(MemoryDataSource::default());
        let check = RetrievalZeroResultCheck::new(config, source);
        assert_eq!(check.cadence(), DoctorCheckCadence::OnDemand);
    }

    // -----------------------------------------------------------------
    // Entry point aggregation
    // -----------------------------------------------------------------

    #[test]
    fn counts_aggregate_across_entry_points() {
        // Even though neither entry point alone exceeds threshold, the
        // aggregate does, and the total meets the floor.
        let config = RetrievalHealthConfig::new(24, 0.5, 20).unwrap();
        let projects = vec![make_project(
            "p-aggregate",
            0,
            0,
            vec![("memory_search", 20, 6), ("memory_build_context", 20, 15)],
        )];
        let findings = run_check(config, projects);
        assert_eq!(findings.len(), 1);
        let evidence = &findings[0].evidence;
        assert_eq!(evidence["numerator"], 21);
        assert_eq!(evidence["denominator"], 40);
    }

    #[test]
    fn healthy_project_aggregates_to_no_finding() {
        let config = RetrievalHealthConfig::new(24, 0.5, 20).unwrap();
        let projects = vec![make_project(
            "p-healthy",
            0,
            0,
            vec![("memory_search", 20, 5), ("memory_build_context", 20, 5)],
        )];
        let findings = run_check(config, projects);
        assert!(findings.is_empty());
    }

    fn v1_group(project: &str, entry: &str, successful: u64, zero: u64, candidates: u64, starved: u64) -> TaxonomyV1ValidGroupSnapshot {
        TaxonomyV1ValidGroupSnapshot { project_id: project.into(), entry_point: entry.into(), taxonomy_version: RETRIEVAL_TAXONOMY_V1.into(), window_start: ts(0), window_end: ts(24), refreshed_at: ts(25), counters: TaxonomyV1QueryCounters { total_queries: successful + 3, successful_queries: successful, errored_queries: 2, cancelled_queries: 1, zero_candidate_queries: zero, candidate_bearing_queries: candidates, starved_queries: starved, injected_queries: candidates - starved }, candidate_total: candidates * 2, injected_total: candidates - starved, dispositions: TaxonomyV1DispositionHistogram { succeeded: successful, errored: 2, cancelled: 1, disabled: 0, unknown: 0 }, legacy_unclassified_queries: 5, invalid_taxonomy_queries: 0 }
    }
    fn v1_config() -> KnowledgeInjectionConfig { KnowledgeInjectionConfig { injection_starvation_threshold_percent: 50, injection_starvation_query_floor: 2, retrieval_health_window_minutes: 60, ..KnowledgeInjectionConfig::default() } }

    #[test]
    fn v1_resolvers_are_inclusive_and_independent() {
        let snapshot = TaxonomyV1RetrievalSnapshot { valid_groups: vec![v1_group("p1", "memory_search", 2, 1, 0, 0), v1_group("p2", "memory_search", 2, 0, 0, 0), v1_group("p1", LOAD_KNOWLEDGE_CONTEXT_ENTRY_POINT, 2, 0, 2, 1)], invalid_groups: vec![] };
        let zero = resolve_retrieval_zero_result_v1(&snapshot, v1_config());
        assert_eq!(zero.len(), 1, "50 percent equality triggers");
        assert_eq!(zero[0].entity_ids["finding_key"], "memory.retrieval_zero_result:p1:memory_search");
        let starvation = resolve_injection_starvation_v1(&snapshot, v1_config());
        assert_eq!(starvation.len(), 1);
        assert_eq!(starvation[0].entity_ids["finding_key"], "memory.injection_starvation:p1:load_knowledge_context");
    }

    #[test]
    fn v1_floor_and_complete_evidence() {
        let below = TaxonomyV1RetrievalSnapshot { valid_groups: vec![v1_group("p", "memory_search", 1, 1, 0, 0)], invalid_groups: vec![] };
        assert!(resolve_retrieval_zero_result_v1(&below, v1_config()).is_empty());
        let snapshot = TaxonomyV1RetrievalSnapshot { valid_groups: vec![v1_group("p", "memory_search", 2, 2, 0, 0)], invalid_groups: vec![] };
        let finding = resolve_retrieval_zero_result_v1(&snapshot, v1_config()).pop().unwrap();
        assert_eq!(finding.evidence["exact_ratio"], "2/2");
        assert_eq!(finding.evidence["configured_window_minutes"], 60);
        assert!(finding.evidence["refreshed_at"].is_string());
        assert_eq!(finding.evidence["query_counters"]["errored_queries"], 2);
        assert_eq!(finding.evidence["dispositions"]["cancelled"], 1);
        assert_eq!(finding.evidence["legacy_unclassified_queries"], 5);
    }

}
