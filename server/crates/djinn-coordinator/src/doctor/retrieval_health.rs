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

/// Return the two stable alarm identities for every malformed snapshot group.
///
/// Malformed groups are excluded from both pure resolvers. Reconciliation uses
/// these identities to retain prior rows without parsing display text or
/// requiring a currently-active row for each possible alarm.
pub fn malformed_retrieval_alarm_keys(snapshot: &TaxonomyV1RetrievalSnapshot) -> Vec<String> {
    let mut keys: Vec<_> = snapshot
        .invalid_groups()
        .flat_map(|group| {
            [
                group
                    .finding_key(djinn_core::doctor::checks::retrieval::RETRIEVAL_ZERO_RESULT_NAME),
                group.finding_key(djinn_core::doctor::checks::retrieval::INJECTION_STARVATION_NAME),
            ]
        })
        .collect();
    keys.sort();
    keys
}

/// One shared source; successful refreshes replace the complete immutable snapshot.
pub struct RetrievalHealthSource {
    // `None` is only constructed by the isolated registration test seam below;
    // production construction always supplies the coordinator repository.
    repository: Option<RetrievalTraceRepository>,
    config: KnowledgeInjectionConfig,
    publication: Arc<RetrievalHealthPublication>,
}

/// The synchronous state shared by the source and all of its Cheap checks.
/// A new `Arc` is installed only after the complete repository response maps.
struct RetrievalHealthPublication {
    snapshot: Mutex<Arc<TaxonomyV1RetrievalSnapshot>>,
    last_error: Mutex<Option<String>>,
    last_attempt: Mutex<Option<OffsetDateTime>>,
    last_success: Mutex<Option<OffsetDateTime>>,
}

impl RetrievalHealthPublication {
    fn new() -> Self {
        Self {
            snapshot: Mutex::new(Arc::new(TaxonomyV1RetrievalSnapshot::new())),
            last_error: Mutex::new(None),
            last_attempt: Mutex::new(None),
            last_success: Mutex::new(None),
        }
    }

    fn snapshot(&self) -> Arc<TaxonomyV1RetrievalSnapshot> {
        Arc::clone(&self.snapshot.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn finish(&self, next: Result<TaxonomyV1RetrievalSnapshot, String>) -> Result<(), String> {
        match next {
            Ok(snapshot) => {
                *self.snapshot.lock().unwrap_or_else(|e| e.into_inner()) = Arc::new(snapshot);
                *self.last_error.lock().unwrap_or_else(|e| e.into_inner()) = None;
                *self.last_success.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(OffsetDateTime::now_utc());
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

    fn refresh_failure_evidence(&self) -> Option<serde_json::Value> {
        let error = self.refresh_error()?;
        let attempted_at = (*self.last_attempt.lock().unwrap_or_else(|e| e.into_inner()))?;
        let last_success = *self.last_success.lock().unwrap_or_else(|e| e.into_inner());
        Some(
            serde_json::json!({"error_class":"retrieval_health_refresh_failed","attempted_at":attempted_at.format(&Iso8601::DEFAULT).ok(),"last_success_at":last_success.and_then(|at| at.format(&Iso8601::DEFAULT).ok()),"last_success_age_seconds":last_success.map(|at| (attempted_at-at).whole_seconds()),"detail":error.chars().take(512).collect::<String>()}),
        )
    }
}

impl RetrievalHealthSource {
    pub fn new(db: Database, config: KnowledgeInjectionConfig) -> Self {
        Self {
            repository: Some(RetrievalTraceRepository::new(db)),
            config,
            publication: Arc::new(RetrievalHealthPublication::new()),
        }
    }

    /// Build a source whose first refresh fails without opening a database.
    ///
    /// This keeps startup-registration tests isolated from database setup while
    /// exercising the same failure-preserving publication path as a repository
    /// refresh error.
    #[cfg(test)]
    pub(crate) fn failing_initial_refresh_for_test(config: KnowledgeInjectionConfig) -> Self {
        Self {
            repository: None,
            config,
            publication: Arc::new(RetrievalHealthPublication::new()),
        }
    }

    pub fn snapshot(&self) -> Arc<TaxonomyV1RetrievalSnapshot> {
        self.publication.snapshot()
    }

    pub async fn refresh(&self) -> Result<(), String> {
        *self
            .publication
            .last_attempt
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(OffsetDateTime::now_utc());
        let until = OffsetDateTime::now_utc();
        let from =
            until - Duration::minutes(i64::from(self.config.retrieval_health_window_minutes));
        let format = |timestamp: OffsetDateTime| {
            timestamp
                .format(&Iso8601::DEFAULT)
                .map_err(|error| error.to_string())
        };
        let groups = match (format(from), format(until), self.repository.as_ref()) {
            (Ok(from), Ok(until), Some(repository)) => repository
                .taxonomy_v1_health_rollup(&from, &until, &until)
                .await
                .map_err(|e| e.to_string()),
            (Err(error), _, _) | (_, Err(error), _) => Err(error),
            (_, _, None) => Err("test retrieval-health initial refresh failure".to_owned()),
        };
        self.publication.finish(groups.and_then(map_groups))
    }

    fn refresh_error(&self) -> Option<String> {
        self.publication.refresh_error()
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
    config: KnowledgeInjectionConfig,
    publication: Arc<RetrievalHealthPublication>,
}
impl SourceZeroResultCheck {
    pub fn new(source: Arc<RetrievalHealthSource>) -> Self {
        Self {
            config: source.config,
            publication: Arc::clone(&source.publication),
        }
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
        if self.publication.refresh_error().is_some() {
            return Ok(Vec::new());
        }
        TaxonomyV1RetrievalZeroResultCheck::new(self.config, (*self.publication.snapshot()).clone())
            .run()
    }
    fn cadence(&self) -> DoctorCheckCadence {
        DoctorCheckCadence::Cheap
    }
}
pub struct SourceInjectionStarvationCheck {
    config: KnowledgeInjectionConfig,
    publication: Arc<RetrievalHealthPublication>,
}
impl SourceInjectionStarvationCheck {
    pub fn new(source: Arc<RetrievalHealthSource>) -> Self {
        Self {
            config: source.config,
            publication: Arc::clone(&source.publication),
        }
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
        if self.publication.refresh_error().is_some() {
            return Ok(Vec::new());
        }
        InjectionStarvationCheck::new(self.config, (*self.publication.snapshot()).clone()).run()
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
            .publication
            .refresh_failure_evidence()
            .into_iter()
            .map(|evidence| {
                let detail = evidence["detail"]
                    .as_str()
                    .unwrap_or("retrieval refresh failed")
                    .to_owned();
                Finding::new(
                    djinn_core::doctor::FindingSeverity::Error,
                    RETRIEVAL_HEALTH_REFRESH_NAME,
                    djinn_core::doctor::ResolverSnapshot::new(
                        "retrieval_health_refresh",
                        evidence.clone(),
                        serde_json::json!({"healthy": false}),
                    ),
                    detail,
                )
                .with_evidence(evidence)
            })
            .collect())
    }
    fn cadence(&self) -> DoctorCheckCadence {
        DoctorCheckCadence::Cheap
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_db::repositories::retrieval_trace::{
        RetrievalTaxonomyValidationError, RetrievalTraceEntryPoint, TaxonomyV1RetrievalHealthCounts,
    };

    fn group(entry_point: RetrievalTraceEntryPoint) -> TaxonomyV1RetrievalHealthGroup {
        TaxonomyV1RetrievalHealthGroup {
            project_id: "project".into(),
            entry_point,
            taxonomy_version: 1,
            window_start: "2026-01-01T00:00:00Z".into(),
            window_end: "2026-01-01T01:00:00Z".into(),
            refreshed_at: "2026-01-01T01:00:01Z".into(),
            invalid: false,
            counts: TaxonomyV1RetrievalHealthCounts {
                total_queries: 10,
                successful_queries: 10,
                zero_candidate_queries: 10,
                candidate_bearing_queries: 10,
                starved_queries: 10,
                ..Default::default()
            },
            validation_errors: vec![],
        }
    }

    #[test]
    fn maps_healthy_and_malformed_siblings() {
        let healthy = group(RetrievalTraceEntryPoint::LoadKnowledgeContext);
        let mut malformed = group(RetrievalTraceEntryPoint::Dispatch);
        malformed.invalid = true;
        malformed.validation_errors = vec![
            RetrievalTaxonomyValidationError {
                trace_id: "b".into(),
                reason: "second".into(),
            },
            RetrievalTaxonomyValidationError {
                trace_id: "a".into(),
                reason: "first".into(),
            },
        ];
        let snapshot = map_groups(vec![healthy, malformed]).unwrap();
        assert_eq!(snapshot.valid_groups().count(), 1);
        let invalid: Vec<_> = snapshot.invalid_groups().collect();
        assert_eq!(invalid.len(), 1);
        assert_eq!(invalid[0].invalid_reason, "a:first; b:second");
        assert_eq!(
            malformed_retrieval_alarm_keys(&snapshot),
            vec![
                "memory.injection_starvation:project:dispatch",
                "memory.retrieval_zero_result:project:dispatch",
            ],
        );
    }

    #[test]
    fn rejected_values_preserve_prior_snapshot_without_partial_publication() {
        let publication = RetrievalHealthPublication::new();
        publication
            .finish(map_groups(vec![group(
                RetrievalTraceEntryPoint::LoadKnowledgeContext,
            )]))
            .unwrap();
        let prior = publication.snapshot();
        let count_fields = [
            "total_queries",
            "successful_queries",
            "errored_queries",
            "zero_candidate_queries",
            "candidate_bearing_queries",
            "starved_queries",
            "injected_queries",
            "candidate_total",
            "injected_total",
            "confidence_filtered_total",
            "not_top_k_total",
            "oversized_skipped_total",
            "injected_disposition_total",
            "budget_pruned_total",
            "legacy_unclassified_queries",
            "invalid_taxonomy_queries",
        ];
        for field in count_fields {
            let mut bad = group(RetrievalTraceEntryPoint::LoadKnowledgeContext);
            match field {
                "total_queries" => bad.counts.total_queries = -1,
                "successful_queries" => bad.counts.successful_queries = -1,
                "errored_queries" => bad.counts.errored_queries = -1,
                "zero_candidate_queries" => bad.counts.zero_candidate_queries = -1,
                "candidate_bearing_queries" => bad.counts.candidate_bearing_queries = -1,
                "starved_queries" => bad.counts.starved_queries = -1,
                "injected_queries" => bad.counts.injected_queries = -1,
                "candidate_total" => bad.counts.candidate_total = -1,
                "injected_total" => bad.counts.injected_total = -1,
                "confidence_filtered_total" => bad.counts.confidence_filtered_total = -1,
                "not_top_k_total" => bad.counts.not_top_k_total = -1,
                "oversized_skipped_total" => bad.counts.oversized_skipped_total = -1,
                "injected_disposition_total" => bad.counts.injected_disposition_total = -1,
                "budget_pruned_total" => bad.counts.budget_pruned_total = -1,
                "legacy_unclassified_queries" => bad.counts.legacy_unclassified_queries = -1,
                "invalid_taxonomy_queries" => bad.counts.invalid_taxonomy_queries = -1,
                _ => unreachable!(),
            }
            assert!(
                publication.finish(map_groups(vec![bad])).is_err(),
                "{field}"
            );
            assert!(Arc::ptr_eq(&prior, &publication.snapshot()), "{field}");
        }
        for field in ["window_start", "window_end", "refreshed_at"] {
            let mut bad = group(RetrievalTraceEntryPoint::LoadKnowledgeContext);
            match field {
                "window_start" => bad.window_start = "invalid".into(),
                "window_end" => bad.window_end = "invalid".into(),
                "refreshed_at" => bad.refreshed_at = "invalid".into(),
                _ => unreachable!(),
            }
            assert!(
                publication.finish(map_groups(vec![bad])).is_err(),
                "{field}"
            );
            assert!(Arc::ptr_eq(&prior, &publication.snapshot()), "{field}");
        }
    }

    #[test]
    fn source_backed_resolvers_share_one_atomic_publication() {
        let publication = Arc::new(RetrievalHealthPublication::new());
        publication
            .finish(map_groups(vec![group(
                RetrievalTraceEntryPoint::LoadKnowledgeContext,
            )]))
            .unwrap();
        let config = KnowledgeInjectionConfig {
            injection_starvation_query_floor: 1,
            injection_starvation_threshold_percent: 50,
            ..Default::default()
        };
        let zero = SourceZeroResultCheck {
            config,
            publication: Arc::clone(&publication),
        };
        let starvation = SourceInjectionStarvationCheck {
            config,
            publication: Arc::clone(&publication),
        };
        assert!(Arc::ptr_eq(
            &zero.publication.snapshot(),
            &starvation.publication.snapshot()
        ));
        assert_eq!(zero.run().unwrap().len(), 1);
        assert_eq!(starvation.run().unwrap().len(), 1);
    }
}
