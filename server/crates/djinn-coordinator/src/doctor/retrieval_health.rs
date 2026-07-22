//! Shared DB-backed taxonomy-v1 retrieval-health source.

use std::sync::{Arc, Mutex};

use djinn_core::doctor::checks::retrieval::{
    InjectionStarvationCheck, RETRIEVAL_TAXONOMY_V1, TaxonomyV1DispositionHistogram,
    TaxonomyV1InvalidGroupSnapshot, TaxonomyV1QueryCounters, TaxonomyV1RetrievalSnapshot,
    TaxonomyV1RetrievalZeroResultCheck, TaxonomyV1ValidGroupSnapshot,
};
use djinn_core::doctor::{DoctorCheck, DoctorCheckCadence, DoctorResult, Finding};
use djinn_core::models::KnowledgeInjectionConfig;
use djinn_db::Database;
use djinn_db::repositories::retrieval_trace::{
    RetrievalTraceRepository, TaxonomyV1RetrievalHealthGroup,
};
use time::{Duration, OffsetDateTime, format_description::well_known::Iso8601};

pub const RETRIEVAL_HEALTH_REFRESH_NAME: &str = "memory.retrieval_health_refresh";

/// One shared source; successful refreshes replace the complete immutable snapshot.
pub struct RetrievalHealthSource {
    repository: RetrievalTraceRepository,
    config: KnowledgeInjectionConfig,
    snapshot: Mutex<Arc<TaxonomyV1RetrievalSnapshot>>,
    last_error: Mutex<Option<String>>,
}

impl RetrievalHealthSource {
    pub fn new(db: Database, config: KnowledgeInjectionConfig) -> Self {
        Self {
            repository: RetrievalTraceRepository::new(db),
            config,
            snapshot: Mutex::new(Arc::new(TaxonomyV1RetrievalSnapshot::new())),
            last_error: Mutex::new(None),
        }
    }

    pub fn snapshot(&self) -> Arc<TaxonomyV1RetrievalSnapshot> {
        Arc::clone(&self.snapshot.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub async fn refresh(&self) -> Result<(), String> {
        let until = OffsetDateTime::now_utc();
        let from =
            until - Duration::minutes(i64::from(self.config.retrieval_health_window_minutes));
        let format = |timestamp: OffsetDateTime| {
            timestamp
                .format(&Iso8601::DEFAULT)
                .map_err(|error| error.to_string())
        };
        let groups = match (format(from), format(until)) {
            (Ok(from), Ok(until)) => self
                .repository
                .taxonomy_v1_health_rollup(&from, &until, &until)
                .await
                .map_err(|e| e.to_string()),
            (Err(error), _) | (_, Err(error)) => Err(error),
        };
        let next = groups.and_then(map_groups);
        match next {
            Ok(snapshot) => {
                *self.snapshot.lock().unwrap_or_else(|e| e.into_inner()) = Arc::new(snapshot);
                *self.last_error.lock().unwrap_or_else(|e| e.into_inner()) = None;
                Ok(())
            }
            Err(error) => {
                *self.last_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(error.clone());
                Err(error)
            }
        }
    }

    fn refresh_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

fn checked(value: i64, field: &str) -> Result<u64, String> {
    u64::try_from(value).map_err(|_| format!("invalid negative {field}: {value}"))
}
fn timestamp(value: &str, field: &str) -> Result<OffsetDateTime, String> {
    OffsetDateTime::parse(value, &Iso8601::DEFAULT)
        .map_err(|e| format!("invalid {field} timestamp {value:?}: {e}"))
}
fn entry_point(group: &TaxonomyV1RetrievalHealthGroup) -> String {
    serde_json::to_value(group.entry_point)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}
fn map_groups(
    groups: Vec<TaxonomyV1RetrievalHealthGroup>,
) -> Result<TaxonomyV1RetrievalSnapshot, String> {
    let mut valid = Vec::new();
    let mut invalid = Vec::new();
    for group in groups {
        let entry_point = entry_point(&group);
        let project_id = group.project_id;
        let window_start = timestamp(&group.window_start, "window_start")?;
        let window_end = timestamp(&group.window_end, "window_end")?;
        let refreshed_at = timestamp(&group.refreshed_at, "refreshed_at")?;
        let taxonomy_version = if group.taxonomy_version == 1 {
            RETRIEVAL_TAXONOMY_V1.to_owned()
        } else {
            format!("taxonomy-v{}", group.taxonomy_version)
        };
        if group.invalid {
            let mut reasons: Vec<String> = group
                .validation_errors
                .into_iter()
                .map(|e| format!("{}:{}", e.trace_id, e.reason))
                .collect();
            reasons.sort();
            invalid.push(TaxonomyV1InvalidGroupSnapshot {
                project_id,
                entry_point,
                taxonomy_version,
                window_start,
                window_end,
                refreshed_at,
                legacy_unclassified_queries: checked(
                    group.counts.legacy_unclassified_queries,
                    "legacy_unclassified_queries",
                )?,
                invalid_taxonomy_queries: checked(
                    group.counts.invalid_taxonomy_queries,
                    "invalid_taxonomy_queries",
                )?,
                invalid_reason: reasons.join("; "),
            });
            continue;
        }
        let c = group.counts;
        valid.push(TaxonomyV1ValidGroupSnapshot {
            project_id,
            entry_point,
            taxonomy_version,
            window_start,
            window_end,
            refreshed_at,
            counters: TaxonomyV1QueryCounters {
                total_queries: checked(c.total_queries, "total_queries")?,
                successful_queries: checked(c.successful_queries, "successful_queries")?,
                errored_queries: checked(c.errored_queries, "errored_queries")?,
                cancelled_queries: 0,
                zero_candidate_queries: checked(
                    c.zero_candidate_queries,
                    "zero_candidate_queries",
                )?,
                candidate_bearing_queries: checked(
                    c.candidate_bearing_queries,
                    "candidate_bearing_queries",
                )?,
                starved_queries: checked(c.starved_queries, "starved_queries")?,
                injected_queries: checked(c.injected_queries, "injected_queries")?,
            },
            candidate_total: checked(c.candidate_total, "candidate_total")?,
            injected_total: checked(c.injected_total, "injected_total")?,
            dispositions: TaxonomyV1DispositionHistogram {
                confidence_filtered: checked(
                    c.confidence_filtered_total,
                    "confidence_filtered_total",
                )?,
                not_top_k: checked(c.not_top_k_total, "not_top_k_total")?,
                oversized_skipped: checked(c.oversized_skipped_total, "oversized_skipped_total")?,
                injected: checked(c.injected_disposition_total, "injected_disposition_total")?,
                budget_pruned: checked(c.budget_pruned_total, "budget_pruned_total")?,
            },
            legacy_unclassified_queries: checked(
                c.legacy_unclassified_queries,
                "legacy_unclassified_queries",
            )?,
            invalid_taxonomy_queries: checked(
                c.invalid_taxonomy_queries,
                "invalid_taxonomy_queries",
            )?,
        });
    }
    Ok(TaxonomyV1RetrievalSnapshot::from_groups(valid, invalid))
}

pub struct SourceZeroResultCheck {
    source: Arc<RetrievalHealthSource>,
}
impl SourceZeroResultCheck {
    pub fn new(source: Arc<RetrievalHealthSource>) -> Self {
        Self { source }
    }
}
impl DoctorCheck for SourceZeroResultCheck {
    fn name(&self) -> &'static str {
        djinn_core::doctor::checks::retrieval::RETRIEVAL_ZERO_RESULT_NAME
    }
    fn description(&self) -> &'static str {
        "Flags taxonomy-v1 successful retrieval queries with zero candidates"
    }
    fn run(&self) -> DoctorResult<Vec<Finding>> {
        TaxonomyV1RetrievalZeroResultCheck::new(
            self.source.config,
            (*self.source.snapshot()).clone(),
        )
        .run()
    }
    fn cadence(&self) -> DoctorCheckCadence {
        DoctorCheckCadence::Cheap
    }
}
pub struct SourceInjectionStarvationCheck {
    source: Arc<RetrievalHealthSource>,
}
impl SourceInjectionStarvationCheck {
    pub fn new(source: Arc<RetrievalHealthSource>) -> Self {
        Self { source }
    }
}
impl DoctorCheck for SourceInjectionStarvationCheck {
    fn name(&self) -> &'static str {
        djinn_core::doctor::checks::retrieval::INJECTION_STARVATION_NAME
    }
    fn description(&self) -> &'static str {
        "Flags starved load_knowledge_context candidate-bearing queries"
    }
    fn run(&self) -> DoctorResult<Vec<Finding>> {
        InjectionStarvationCheck::new(self.source.config, (*self.source.snapshot()).clone()).run()
    }
    fn cadence(&self) -> DoctorCheckCadence {
        DoctorCheckCadence::Cheap
    }
}
pub struct RetrievalHealthRefreshCheck {
    source: Arc<RetrievalHealthSource>,
}
impl RetrievalHealthRefreshCheck {
    pub fn new(source: Arc<RetrievalHealthSource>) -> Self {
        Self { source }
    }
}
impl DoctorCheck for RetrievalHealthRefreshCheck {
    fn name(&self) -> &'static str {
        RETRIEVAL_HEALTH_REFRESH_NAME
    }
    fn description(&self) -> &'static str {
        "Reports the most recent taxonomy-v1 retrieval health refresh failure"
    }
    fn run(&self) -> DoctorResult<Vec<Finding>> {
        Ok(self
            .source
            .refresh_error()
            .into_iter()
            .map(|error| {
                Finding::new(
                    djinn_core::doctor::FindingSeverity::Error,
                    RETRIEVAL_HEALTH_REFRESH_NAME,
                    djinn_core::doctor::ResolverSnapshot::new(
                        "retrieval_health_refresh",
                        serde_json::json!({"error": error}),
                        serde_json::json!({"healthy": false}),
                    ),
                    error,
                )
            })
            .collect())
    }
    fn cadence(&self) -> DoctorCheckCadence {
        DoctorCheckCadence::Cheap
    }
}
