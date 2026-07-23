//! Leader-tick integration for the cheap doctor subset (epic 4q1t).
//!
//! The Doctor framework ([`djinn_core::doctor`]) exposes a pluggable registry of
//! [`DoctorCheck`]s and a [`DoctorCheckCadence`] tag that separates "safe to run
//! every coordinator tick" from "explicit on-demand". This module wires the
//! cheap subset into the coordinator's existing 30s tick without going through
//! the MCP `doctor_run` tool: it uses the framework's
//! [`run_cheap_subset`] helper, persists every returned [`Finding`] through
//! [`djinn_db::DoctorFindingRepository`], emits bounded-cardinality
//! `djinn_doctor_findings{check}` and `djinn_doctor_run_duration_seconds{check}`
//! metrics via [`djinn_telemetry::doctor`], and writes a board-visible activity
//! entry for each `Critical` finding via [`djinn_db::TaskRepository::log_activity`].
//!
//! All failures (one bad check, one bad DB write, one bad activity entry) are
//! logged and surfaced as tracing; they MUST NOT take the rest of the
//! coordinator tick down — see the per-step `match`/`if let Err` blocks below.
//!
//! ## Tick placement
//!
//! The cheap-doctor tick is called from [`super::super::coordinator::actor`]
//! immediately after the existing cheap reapers/backstops and before dispatch
//! so a board-visible critical finding can race the dispatch decision for the
//! same 30s window. It is intentionally a non-blocking observation: no
//! dispatch, no fix, no state mutation outside of inserts.

use djinn_core::clock::{Clock, SystemClock};
use djinn_core::doctor::{
    DoctorCheckRun, DoctorRegistry, Finding, FindingSeverity, run_cheap_subset,
};
use djinn_db::repositories::doctor_finding::{KeyedDoctorFinding, severity};
use djinn_db::{
    DoctorFindingRepository, LintMaterializationOutcome, NewDoctorFinding,
    ProposalIntegrityHeadPage, ProposalIntegrityRepository, ProposalRepository,
};
use serde_json::json;
use tracing::{error, info, warn};

/// Activity-event type written for board-visible critical doctor findings.
///
/// Matches the existing `task_outcome_confidence` precedent: stable
/// snake_case identifier used by UI/feed consumers to filter the activity log.
pub const DOCTOR_CRITICAL_FINDING_ACTIVITY: &str = "doctor_critical_finding";

/// Activity-event type written for board-visible non-critical doctor findings.
///
/// Surfaces Warn/Info findings in the activity feed when the leader-tick path
/// wants to make a divergence visible without bumping the Critical-flagged
/// lane. Written with the same payload shape as the critical variant.
pub const DOCTOR_FINDING_ACTIVITY: &str = "doctor_finding";

/// Stable name for the retroactive immutable proposal lint finding.
pub const PROPOSAL_SPEC_INTEGRITY_CHECK_NAME: &str = "proposal_spec_integrity_v1";

/// Number of ascending-id pages a single leader tick may consume.
const PROPOSAL_SPEC_INTEGRITY_MAX_PAGES_PER_TICK: usize = 10;

/// Run the bounded, conflict-safe retroactive proposal lint materialization.
///
/// The early return precedes construction of every proposal source: disabled
/// operation performs neither a proposal scan nor a retained-body load.
pub async fn run_proposal_spec_integrity_sweep(
    enabled: bool,
    db: &djinn_db::Database,
    run_id: Option<&str>,
) {
    if !enabled {
        return;
    }
    let heads = ProposalIntegrityRepository::new(db.clone());
    let proposals = ProposalRepository::new(db.clone(), djinn_core::events::EventBus::noop());
    let findings = DoctorFindingRepository::new(db.clone());
    let mut after_proposal_id = None;
    for _ in 0..PROPOSAL_SPEC_INTEGRITY_MAX_PAGES_PER_TICK {
        let page = match heads
            .list_current_heads(ProposalIntegrityHeadPage {
                after_proposal_id: after_proposal_id.clone(),
                limit: djinn_db::MAX_PROPOSAL_INTEGRITY_PAGE_SIZE,
            })
            .await
        {
            Ok(page) => page,
            Err(error) => {
                warn!(error = %error, "proposal spec integrity sweep: head scan failed");
                return;
            }
        };
        if page.is_empty() {
            return;
        }
        let page_len = page.len();
        after_proposal_id = page.last().map(|head| head.proposal_id.clone());
        for head in page {
            let result = match proposals.lint_for_revision(&head.revision).await {
                Ok(result) => result,
                Err(error) => {
                    warn!(proposal_id = %head.proposal_id, revision_seq = head.revision.seq, error = %error, "proposal spec integrity sweep: lint failed; continuing");
                    continue;
                }
            };
            match heads.materialize_if_current(&head, &result).await {
                Ok(LintMaterializationOutcome::Stale) => continue,
                Ok(
                    LintMaterializationOutcome::Materialized
                    | LintMaterializationOutcome::AlreadyPresent,
                ) => {}
                Err(error) => {
                    warn!(proposal_id = %head.proposal_id, revision_seq = head.revision.seq, error = %error, "proposal spec integrity sweep: guarded materialization failed; continuing");
                    continue;
                }
            }
            if result.errors.is_empty() {
                continue;
            }
            let key = format!(
                "{PROPOSAL_SPEC_INTEGRITY_CHECK_NAME}:{}:{}:{}",
                head.proposal_id, head.revision.seq, result.linter_version
            );
            let finding = NewDoctorFinding {
                run_id: run_id.map(str::to_owned),
                check_name: PROPOSAL_SPEC_INTEGRITY_CHECK_NAME.to_owned(),
                severity: severity::ERROR.to_owned(),
                entity_ids: json!([head.proposal_id]),
                evidence: json!({ "revision_seq": head.revision.seq, "body_sha256": head.body_sha256, "linter_version": result.linter_version, "violations": result.errors }),
                resolver_snapshot: None,
                detail: Some("proposal specification integrity lint errors".to_owned()),
            };
            if let Err(error) = findings.insert_ignore_duplicate(finding, &key).await {
                warn!(proposal_id = %head.proposal_id, revision_seq = head.revision.seq, error = %error, "proposal spec integrity sweep: finding insert failed; continuing");
            }
        }
        if page_len < djinn_db::MAX_PROPOSAL_INTEGRITY_PAGE_SIZE as usize {
            return;
        }
    }
}

/// Convert a single [`Finding`] into the repository insert DTO.
///
/// Mirrors the canonical shape used by the MCP `doctor_run` tool in
/// `djinn-control-plane` so a row inserted from the leader tick is structurally
/// indistinguishable from one inserted by an MCP caller (check name, severity,
/// entity ids, evidence, resolver snapshot, detail all preserved).
pub fn finding_to_new_row(finding: &Finding, run_id: Option<&str>) -> NewDoctorFinding {
    let entity_ids = match serde_json::to_value(&finding.entity_ids) {
        Ok(value) => value,
        Err(e) => {
            warn!(
                check_name = %finding.check_name,
                error = %e,
                "doctor leader tick: failed to serialize finding entity ids; storing empty array"
            );
            json!([])
        }
    };
    let resolver_snapshot = serde_json::to_value(&finding.resolver_snapshot).ok();

    NewDoctorFinding {
        run_id: run_id.map(str::to_owned),
        check_name: finding.check_name.clone(),
        severity: finding.severity.as_str().to_owned(),
        entity_ids,
        evidence: finding.evidence.clone(),
        resolver_snapshot,
        detail: Some(finding.detail.clone()),
    }
}

fn is_retrieval_check(name: &str) -> bool {
    matches!(
        name,
        "memory.retrieval_zero_result"
            | "memory.injection_starvation"
            | "memory.retrieval_health_refresh"
    )
}

fn retrieval_active_key(finding: &Finding) -> String {
    if finding.check_name == "memory.retrieval_health_refresh" {
        return "refresh".to_owned();
    }
    finding
        .entity_ids
        .get("finding_key")
        .cloned()
        .unwrap_or_else(|| {
            format!(
                "{}:{}",
                finding
                    .entity_ids
                    .get("project_id")
                    .map(String::as_str)
                    .unwrap_or("unknown"),
                finding
                    .entity_ids
                    .get("entry_point")
                    .map(String::as_str)
                    .unwrap_or("all")
            )
        })
}

/// Best-effort task-id lookup for a finding, used to attach a board activity
/// entry to the right task when one is known. Returns `None` for findings with
/// no entity context (board-wide observation), in which case the activity
/// entry is written with `task_id = NULL` (the activity_log column is
/// nullable per the existing schema).
pub fn finding_task_id(finding: &Finding) -> Option<String> {
    // The canonical "task id" key the leader-tick path looks for. Checks are
    // free to populate other keys (workspace, session, run) — those do not
    // attach the activity entry to a specific task.
    finding
        .entity_ids
        .get("task_id")
        .cloned()
        .or_else(|| finding.entity_ids.get("task").cloned())
}

/// Persist every finding returned by the cheap subset, in order. A per-finding
/// failure is logged and the remaining findings are still attempted so one bad
/// row cannot block the rest of the tick.
async fn persist_findings(
    repo: &DoctorFindingRepository,
    run_id: Option<&str>,
    findings: &[Finding],
) {
    for finding in findings {
        let new = finding_to_new_row(finding, run_id);
        match repo.insert(new).await {
            Ok(persisted) => {
                info!(
                    finding_id = %persisted.id,
                    check = %persisted.check_name,
                    severity = %persisted.severity,
                    "CoordinatorActor: persisted cheap doctor finding"
                );
            }
            Err(error) => {
                // Log + continue. We do NOT propagate so a single DB hiccup
                // cannot silence the rest of the tick or its metrics.
                warn!(
                    check = %finding.check_name,
                    severity = %finding.severity.as_str(),
                    error = %error,
                    "CoordinatorActor: failed to persist cheap doctor finding; continuing"
                );
            }
        }
    }
}

/// Write one board-visible activity entry for a [`Finding`].
///
/// The actor on the activity entry is fixed to `coordinator` / `system` (same
/// convention as the existing stall-timeout entries) so operators can filter
/// the activity log by actor. The `task_id` is `None` when the finding has no
/// entity context — this is the documented "board-wide observation" case and
/// matches `activity_log.task_id` being nullable.
///
/// A failure to write the activity entry is logged but never propagated; the
/// tick must keep moving.
async fn record_activity_for_finding(repo: &djinn_db::TaskRepository, finding: &Finding) {
    let task_id = finding_task_id(finding);
    let (event_type, kind_label) = if finding.severity == FindingSeverity::Critical {
        (DOCTOR_CRITICAL_FINDING_ACTIVITY, "critical")
    } else {
        (DOCTOR_FINDING_ACTIVITY, finding.severity.as_str())
    };

    let payload = json!({
        "check": finding.check_name,
        "check_name": finding.check_name,
        "severity": finding.severity.as_str(),
        "kind": kind_label,
        "detail": finding.detail,
        "entity_ids": finding.entity_ids,
    })
    .to_string();

    if let Err(error) = repo
        .log_activity(
            task_id.as_deref(),
            "coordinator",
            "system",
            event_type,
            &payload,
        )
        .await
    {
        warn!(
            check = %finding.check_name,
            severity = %finding.severity.as_str(),
            error = %error,
            "CoordinatorActor: failed to write doctor finding activity entry; continuing"
        );
    }
}

/// Run the cheap doctor subset once, persisting findings and emitting metrics.
///
/// Exposed for tests via [`super::super::coordinator::actor`]; the actor's
/// `run_tick` invokes this directly so the leader-tick integration stays
/// focused and unit-testable. Each step is best-effort: a failure in one
/// check or one DB write is logged and the rest of the tick continues.
///
/// `registry` is the [`DoctorRegistry`] to source cheap checks from. The
/// production path passes [`registry()`]; tests pass a local
/// [`DoctorRegistry`] they control.
///
/// `run_id` is stamped on every persisted finding so the operator can scope a
/// subsequent `doctor_list_findings` query back to one leader-tick invocation.
/// Pass `None` for ad-hoc invocations (tests).
///
/// Returns the cheap-check runs after persistence/metric/activity side effects
/// so hermetic coordinator tests can assert the same findings the tick handled.
pub async fn run_cheap_doctor_checks(
    registry: &DoctorRegistry,
    db: &djinn_db::Database,
    events_tx: &tokio::sync::broadcast::Sender<djinn_core::events::DjinnEventEnvelope>,
    run_id: Option<&str>,
) -> Vec<DoctorCheckRun> {
    run_cheap_doctor_checks_with_preserved_retrieval_keys(registry, db, events_tx, run_id, &[])
        .await
}

/// Run the cheap subset while retaining prior rows for malformed groups from
/// an otherwise successful retrieval snapshot. The supplied keys must be
/// derived from taxonomy-v1 group helpers and include both potential alarms.
pub async fn run_cheap_doctor_checks_with_preserved_retrieval_keys(
    registry: &DoctorRegistry,
    db: &djinn_db::Database,
    events_tx: &tokio::sync::broadcast::Sender<djinn_core::events::DjinnEventEnvelope>,
    run_id: Option<&str>,
    malformed_retrieval_keys: &[String],
) -> Vec<DoctorCheckRun> {
    let started = SystemClock::new().now_instant();
    let runs = match run_cheap_subset(registry) {
        Ok(runs) => runs,
        Err(error) => {
            // A registry-level error means every check failed to run; log
            // once and exit so we don't spam the activity log.
            error!(
                error = %error,
                "CoordinatorActor: cheap doctor subset failed to enumerate; skipping tick"
            );
            return Vec::new();
        }
    };

    if runs.is_empty() {
        // Nothing cheap registered. Still record a tick stamp so the metric
        // surface does not silently go stale.
        info!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            "CoordinatorActor: cheap doctor subset had no registered checks"
        );
        return Vec::new();
    }

    let finding_repo = DoctorFindingRepository::new(db.clone());
    let task_repo =
        djinn_db::TaskRepository::new(db.clone(), crate::events::event_bus_for(events_tx));

    let retrieval: Vec<KeyedDoctorFinding> = runs
        .iter()
        .flat_map(|run| run.findings.iter())
        .filter(|finding| is_retrieval_check(&finding.check_name))
        .map(|finding| KeyedDoctorFinding {
            active_key: retrieval_active_key(finding),
            finding: finding_to_new_row(finding, run_id),
        })
        .collect();
    if runs.iter().any(|run| is_retrieval_check(run.check_name)) {
        // A refresh failure is indeterminate, not healthy absence: preserve all
        // previously active alarms while reconciling the refresh-error key.
        let preserve_all_alarms = runs.iter().any(|run| {
            run.check_name == "memory.retrieval_health_refresh" && !run.findings.is_empty()
        });
        let preserve_keys = if preserve_all_alarms {
            match finding_repo.active_retrieval_alarm_keys().await {
                Ok(keys) => keys,
                Err(error) => {
                    warn!(error = %error, "CoordinatorActor: failed to load retrieval preserve keys");
                    return runs;
                }
            }
        } else {
            malformed_retrieval_keys.to_vec()
        };
        if let Err(error) = finding_repo
            .reconcile_retrieval_findings(retrieval, &preserve_keys)
            .await
        {
            warn!(error = %error, "CoordinatorActor: failed to reconcile retrieval doctor findings");
        }
    }
    for run in &runs {
        let check_name = run.check_name;
        let findings_count = run.findings.len();
        let run_started = SystemClock::new().now_instant();

        if !is_retrieval_check(check_name) {
            persist_findings(&finding_repo, run_id, &run.findings).await;
        }

        for finding in &run.findings {
            // This check is a dry-run: durable doctor evidence is allowed, but
            // observing an orphan must not create task activity.
            if finding.check_name == crate::doctor::CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME {
                continue;
            }
            // Every finding gets a board activity entry — Critical surfaces
            // in the dedicated lane; Warn/Info still surface in the
            // generic doctor-finding lane for observability.
            record_activity_for_finding(&task_repo, finding).await;
        }

        let elapsed = run_started.elapsed();
        // Metrics facade enforces the `check`-only label discipline.
        djinn_telemetry::doctor::set_findings(check_name, findings_count);
        djinn_telemetry::doctor::record_run_duration(check_name, elapsed);

        info!(
            check = check_name,
            findings = findings_count,
            elapsed_ms = elapsed.as_millis() as u64,
            "CoordinatorActor: cheap doctor check complete"
        );
    }

    info!(
        checks = runs.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "CoordinatorActor: cheap doctor tick complete"
    );

    runs
}

#[cfg(test)]
#[path = "leader_tick_mixed_snapshot_tests.rs"]
mod leader_tick_mixed_snapshot_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::stranded_ready::{MemoryStrandedReadySource, StrandedReadyCheck};
    use djinn_core::doctor::{
        DoctorCheck, DoctorCheckCadence, DoctorRegistry, DoctorResult, Finding, FindingSeverity,
        ResolverSnapshot,
    };
    use djinn_db::{Database, DoctorFindingRepository};
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::broadcast;

    const TEST_CHECK_NAME: &str = "leader_tick.test";

    struct FixtureCheck;

    impl DoctorCheck for FixtureCheck {
        fn name(&self) -> &'static str {
            TEST_CHECK_NAME
        }

        fn description(&self) -> &'static str {
            "Test check that emits one critical and one warn finding"
        }

        fn cadence(&self) -> DoctorCheckCadence {
            DoctorCheckCadence::Cheap
        }

        fn run(&self) -> DoctorResult<Vec<Finding>> {
            let mut critical = Finding::new(
                FindingSeverity::Critical,
                self.name(),
                ResolverSnapshot::new(
                    "resolve_fixture",
                    json!({"observed": 0}),
                    json!({"expected": 1, "should_fix": true}),
                ),
                "fixture critical divergence",
            );
            critical = critical
                .with_entity_id("task_id", "task-fixture-1")
                .with_entity_id("workspace_id", "ws-fixture")
                .with_evidence(json!({"rows": 1}));

            let warn = Finding::new(
                FindingSeverity::Warn,
                self.name(),
                ResolverSnapshot::new(
                    "resolve_fixture",
                    json!({"observed": 2}),
                    json!({"expected": 2, "should_fix": false}),
                ),
                "fixture warn observation",
            )
            .with_entity_id("task_id", "task-fixture-2")
            .with_evidence(json!({"rows": 1}));

            Ok(vec![critical, warn])
        }
    }

    struct BoardWideCheck;

    impl DoctorCheck for BoardWideCheck {
        fn name(&self) -> &'static str {
            "leader_tick.board_wide"
        }

        fn description(&self) -> &'static str {
            "Check that emits a critical finding with no task entity"
        }

        fn cadence(&self) -> DoctorCheckCadence {
            DoctorCheckCadence::Cheap
        }

        fn run(&self) -> DoctorResult<Vec<Finding>> {
            Ok(vec![
                Finding::new(
                    FindingSeverity::Critical,
                    self.name(),
                    ResolverSnapshot::new("resolve_board", json!({}), json!({"should_fix": true})),
                    "board-wide critical divergence",
                )
                .with_evidence(json!({"sample": 1})),
            ])
        }
    }

    /// Deterministic source/repository seam for the retrieval lifecycle
    /// regression. Phase 0 alarms, phase 1 is a whole-source failure, and
    /// phase 2 is a healthy non-alarming refresh.
    struct LifecycleRetrievalCheck {
        name: &'static str,
        phase: Arc<AtomicUsize>,
        resolver_invocations: Arc<AtomicUsize>,
    }

    impl LifecycleRetrievalCheck {
        fn new(
            name: &'static str,
            phase: Arc<AtomicUsize>,
            resolver_invocations: Arc<AtomicUsize>,
        ) -> Self {
            Self {
                name,
                phase,
                resolver_invocations,
            }
        }
    }

    impl DoctorCheck for LifecycleRetrievalCheck {
        fn name(&self) -> &'static str {
            self.name
        }

        fn description(&self) -> &'static str {
            "Injected retrieval source lifecycle test check"
        }

        fn cadence(&self) -> DoctorCheckCadence {
            DoctorCheckCadence::Cheap
        }

        fn run(&self) -> DoctorResult<Vec<Finding>> {
            let phase = self.phase.load(Ordering::SeqCst);
            if self.name == "memory.retrieval_health_refresh" {
                return if phase == 1 {
                    let evidence = json!({
                        "error_class": "retrieval_health_refresh_failed",
                        "attempted_at": "2026-01-01T01:02:00Z",
                        "last_success_at": "2026-01-01T01:00:00Z",
                        "last_success_age_seconds": 120,
                        "detail": "injected repository refresh failure",
                    });
                    Ok(vec![
                        Finding::new(
                            FindingSeverity::Error,
                            self.name,
                            ResolverSnapshot::new(
                                "retrieval_health_refresh",
                                evidence.clone(),
                                json!({"healthy": false}),
                            ),
                            "injected repository refresh failure",
                        )
                        .with_evidence(evidence),
                    ])
                } else {
                    Ok(Vec::new())
                };
            }

            // A whole-source failure skips both retrieval resolvers.
            if phase == 1 {
                return Ok(Vec::new());
            }
            self.resolver_invocations.fetch_add(1, Ordering::SeqCst);
            if phase == 2 {
                return Ok(Vec::new());
            }

            let (finding_key, entry_point) = if self.name == "memory.retrieval_zero_result" {
                ("project-a:dispatch", "dispatch")
            } else {
                ("project-a:load_knowledge_context", "load_knowledge_context")
            };
            Ok(vec![
                Finding::new(
                    FindingSeverity::Error,
                    self.name,
                    ResolverSnapshot::new(
                        "retrieval_alarm",
                        json!({"refresh_timestamp": "2026-01-01T01:00:00Z"}),
                        json!({"alarming": true}),
                    ),
                    format!("{entry_point} retrieval alarm"),
                )
                .with_entity_id("finding_key", finding_key)
                .with_entity_id("project_id", "project-a")
                .with_entity_id("entry_point", entry_point)
                .with_evidence(json!({
                    "refresh_timestamp": "2026-01-01T01:00:00Z",
                    "entry_point": entry_point,
                })),
            ])
        }
    }

    struct ExpensiveCheck;

    impl DoctorCheck for ExpensiveCheck {
        fn name(&self) -> &'static str {
            "leader_tick.expensive"
        }

        fn description(&self) -> &'static str {
            "Check that is explicitly on-demand and must NOT run on the cheap tick"
        }

        fn run(&self) -> DoctorResult<Vec<Finding>> {
            panic!("expensive check must not be invoked from the cheap leader-tick path");
        }
    }

    /// Variant of [`ExpensiveCheck`] that increments a shared counter instead
    /// of panicking when invoked. The end-to-end regression test uses this so
    /// it can distinguish "never invoked" (counter == 0) from "invoked but
    /// silently no-op'd" (counter > 0) — a panic would also surface that
    /// condition, but the counter provides a stronger positive assertion and
    /// does not depend on panic unwinding.
    struct CountingExpensiveCheck {
        invocations: Arc<AtomicUsize>,
    }

    impl CountingExpensiveCheck {
        fn new(invocations: Arc<AtomicUsize>) -> Self {
            Self { invocations }
        }
    }

    impl DoctorCheck for CountingExpensiveCheck {
        fn name(&self) -> &'static str {
            "leader_tick.expensive.counter"
        }

        fn description(&self) -> &'static str {
            "On-demand check that bumps a shared counter when invoked"
        }

        fn run(&self) -> DoctorResult<Vec<Finding>> {
            self.invocations.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        }
    }

    fn fresh_db() -> Database {
        Database::open_in_memory().expect("in-memory db")
    }

    fn fresh_registry() -> DoctorRegistry {
        let registry = DoctorRegistry::new();
        djinn_core::doctor::register(&registry, FixtureCheck);
        djinn_core::doctor::register(&registry, BoardWideCheck);
        djinn_core::doctor::register(&registry, ExpensiveCheck);
        djinn_core::doctor::register(
            &registry,
            StrandedReadyCheck::new(Arc::new(MemoryStrandedReadySource::new(json!({
                "total": 0,
                "threshold_minutes": 30,
                "findings": [],
            })))),
        );
        registry
    }

    fn entity_ids_of(finding: &Finding) -> BTreeMap<String, String> {
        finding.entity_ids.clone()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_cheap_doctor_checks_persists_findings_and_emits_metrics() {
        let _ = djinn_telemetry::init();
        let db = fresh_db();
        let (events_tx, _events_rx) = broadcast::channel(16);
        let registry = fresh_registry();

        run_cheap_doctor_checks(&registry, &db, &events_tx, Some("leader-tick-test")).await;

        let repo = DoctorFindingRepository::new(db.clone());
        let rows = repo
            .list_recent(djinn_db::RecentDoctorFindings {
                run_id: Some("leader-tick-test".to_owned()),
                ..Default::default()
            })
            .await
            .expect("list recent");

        // Both cheap checks wrote findings; the expensive check did not run.
        let check_names: std::collections::BTreeSet<&str> =
            rows.iter().map(|row| row.check_name.as_str()).collect();
        assert!(check_names.contains(TEST_CHECK_NAME));
        assert!(check_names.contains("leader_tick.board_wide"));
        assert!(
            !check_names.contains("leader_tick.expensive"),
            "expensive on-demand check must not run on the cheap tick"
        );

        // Three findings total: one critical + one warn (fixture), one critical (board).
        assert_eq!(rows.len(), 3);

        // The fixture check's run-duration metric was stamped with the stable
        // check name label and no other labels.
        let rendered = djinn_telemetry::render().expect("render");
        let findings_line = rendered
            .lines()
            .find(|line| {
                line.starts_with("djinn_doctor_findings{") && line.contains(TEST_CHECK_NAME)
            })
            .expect("findings metric");
        assert!(findings_line.ends_with(" 2"));
        let duration_line = rendered
            .lines()
            .find(|line| {
                line.starts_with("djinn_doctor_run_duration_seconds{")
                    && line.contains(TEST_CHECK_NAME)
            })
            .expect("duration metric");
        // Stable-name-only label discipline.
        assert_eq!(findings_line.matches('=').count(), 1);
        assert_eq!(duration_line.matches('=').count(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_cheap_doctor_checks_writes_activity_entries_for_critical_findings() {
        let _ = djinn_telemetry::init();
        let db = fresh_db();
        let (events_tx, _events_rx) = broadcast::channel(16);
        let registry = fresh_registry();

        run_cheap_doctor_checks(&registry, &db, &events_tx, Some("leader-tick-activity")).await;

        let task_repo =
            djinn_db::TaskRepository::new(db.clone(), crate::events::event_bus_for(&events_tx));

        // The fixture's critical finding is tied to task-fixture-1, so the
        // activity entry must be attached to that task with the
        // `doctor_critical_finding` event type and the canonical payload
        // shape (check/check_name / severity / detail / entity_ids).
        let task_activity = task_repo
            .list_activity("task-fixture-1")
            .await
            .expect("list task activity");
        let critical = task_activity
            .iter()
            .find(|entry| entry.event_type == DOCTOR_CRITICAL_FINDING_ACTIVITY)
            .expect("critical activity entry written");
        let payload: serde_json::Value =
            serde_json::from_str(&critical.payload).expect("payload is json");
        assert_eq!(payload["check"], TEST_CHECK_NAME);
        assert_eq!(payload["check_name"], TEST_CHECK_NAME);
        assert_eq!(payload["severity"], "critical");
        assert_eq!(payload["kind"], "critical");
        assert_eq!(payload["detail"], "fixture critical divergence");
        assert_eq!(payload["entity_ids"]["task_id"], "task-fixture-1");
        assert_eq!(payload["entity_ids"]["workspace_id"], "ws-fixture");
        assert_eq!(critical.actor_id, "coordinator");
        assert_eq!(critical.actor_role, "system");

        // The fixture's warn finding is tied to task-fixture-2 and must use
        // the generic `doctor_finding` event type with `kind = warn`.
        let warn_activity = task_repo
            .list_activity("task-fixture-2")
            .await
            .expect("list task activity");
        let warn_entry = warn_activity
            .iter()
            .find(|entry| entry.event_type == DOCTOR_FINDING_ACTIVITY)
            .expect("warn activity entry written");
        let warn_payload: serde_json::Value =
            serde_json::from_str(&warn_entry.payload).expect("payload is json");
        assert_eq!(warn_payload["check"], TEST_CHECK_NAME);
        assert_eq!(warn_payload["check_name"], TEST_CHECK_NAME);
        assert_eq!(warn_payload["severity"], "warn");
        assert_eq!(warn_payload["kind"], "warn");

        // The board-wide critical finding has no task entity, so its
        // activity entry MUST have task_id = NULL and event_type =
        // `doctor_critical_finding` (operator-visible critical lane).
        let board_activity = task_repo
            .query_activity(djinn_db::ActivityQuery {
                event_type: Some(DOCTOR_CRITICAL_FINDING_ACTIVITY.to_owned()),
                ..Default::default()
            })
            .await
            .expect("query activity");
        let board_entry = board_activity
            .iter()
            .find(|entry| entry.payload.contains("leader_tick.board_wide"))
            .expect("board activity entry");
        assert!(board_entry.task_id.is_none());
        let board_payload: serde_json::Value =
            serde_json::from_str(&board_entry.payload).expect("payload is json");
        assert_eq!(board_payload["check"], "leader_tick.board_wide");
        assert_eq!(board_payload["check_name"], "leader_tick.board_wide");
        assert_eq!(board_payload["severity"], "critical");
    }

    #[test]
    fn finding_to_new_row_carries_structured_fields() {
        let finding = Finding::new(
            FindingSeverity::Warn,
            "demo.test",
            ResolverSnapshot::new(
                "resolve",
                json!({"observed": 1}),
                json!({"expected": 2, "should_fix": true}),
            ),
            "demo detail",
        )
        .with_entity_id("task_id", "task-demo")
        .with_evidence(json!({"rows": 3}));

        let row = finding_to_new_row(&finding, Some("run-x"));
        assert_eq!(row.check_name, "demo.test");
        assert_eq!(row.severity, "warn");
        assert_eq!(row.run_id.as_deref(), Some("run-x"));
        assert_eq!(row.entity_ids, json!({"task_id": "task-demo"}));
        assert_eq!(row.evidence, json!({"rows": 3}));
        assert!(row.resolver_snapshot.is_some());
        assert_eq!(row.detail.as_deref(), Some("demo detail"));
    }

    #[test]
    fn finding_task_id_prefers_task_id_entity_key() {
        let finding = Finding::new(
            FindingSeverity::Info,
            "demo.entity",
            ResolverSnapshot::new("resolve", json!({}), json!({})),
            "detail",
        )
        .with_entity_id("workspace_id", "ws-1")
        .with_entity_id("task_id", "task-99");

        assert_eq!(finding_task_id(&finding).as_deref(), Some("task-99"));
    }

    #[test]
    fn finding_task_id_returns_none_for_board_wide_finding() {
        let finding = Finding::new(
            FindingSeverity::Critical,
            "demo.board",
            ResolverSnapshot::new("resolve", json!({}), json!({})),
            "detail",
        );
        assert!(finding_task_id(&finding).is_none());

        let mut with_workspace = finding.clone();
        with_workspace = with_workspace.with_entity_id("workspace_id", "ws-1");
        assert!(
            finding_task_id(&with_workspace).is_none(),
            "only the canonical task_id / task keys map to an activity row"
        );

        // Silence the unused-variable warning that comes from the entity_ids
        // helper above being only used in tests.
        let _ = entity_ids_of(&finding);
    }

    #[test]
    fn expensive_check_is_excluded_from_cheap_subset() {
        let registry = fresh_registry();
        let cheap = registry.cheap_checks();
        let names: std::collections::BTreeSet<&str> =
            cheap.iter().map(|check| check.name()).collect();
        assert!(names.contains(TEST_CHECK_NAME));
        assert!(names.contains("leader_tick.board_wide"));
        assert!(
            !names.contains("leader_tick.expensive"),
            "expensive on-demand check must be excluded from the cheap subset"
        );
    }

    #[test]
    fn cheap_doctor_tick_helper_handles_empty_registry_gracefully() {
        let registry = DoctorRegistry::new();
        let result = djinn_core::doctor::run_cheap_subset(&registry);
        // An empty cheap subset is a valid run; the helper returns an empty
        // vec of check results.
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    /// End-to-end regression for acceptance criterion 3 of task `yl2p`:
    /// a registered on-demand (expensive) check MUST NOT be executed by the
    /// cheap leader-tick path and MUST NOT leave any finding row or metric
    /// sample behind.
    ///
    /// The test asserts all three side-effect channels together so a future
    /// regression in any one of them is caught by a single focused failure:
    ///
    /// 1. `run()` is never invoked — proved via the shared `AtomicUsize`
    ///    counter handed to [`CountingExpensiveCheck`].
    /// 2. No `doctor_findings` row is persisted for the expensive check.
    /// 3. No `djinn_doctor_findings{check="<expensive>"}` or
    ///    `djinn_doctor_run_duration_seconds{check="<expensive>"}` sample
    ///    appears in the rendered Prometheus output.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn leader_tick_excludes_expensive_check_end_to_end() {
        let _ = djinn_telemetry::init();
        let db = fresh_db();
        let (events_tx, _events_rx) = broadcast::channel(16);

        // Build a registry that pairs the cheap fixture check (so the leader
        // tick has at least one cheap run to do) with a counting expensive
        // check that records every invocation.
        let invocations = Arc::new(AtomicUsize::new(0));
        let registry = DoctorRegistry::new();
        djinn_core::doctor::register(&registry, FixtureCheck);
        djinn_core::doctor::register(
            &registry,
            CountingExpensiveCheck::new(Arc::clone(&invocations)),
        );

        run_cheap_doctor_checks(
            &registry,
            &db,
            &events_tx,
            Some("leader-tick-expensive-exclusion"),
        )
        .await;

        // (1) The expensive check's `run()` was never invoked.
        assert_eq!(
            invocations.load(Ordering::SeqCst),
            0,
            "expensive on-demand check must not be invoked from the cheap leader-tick path",
        );

        // (2) No `doctor_findings` row was written for the expensive check.
        let finding_repo = DoctorFindingRepository::new(db.clone());
        let rows = finding_repo
            .list_recent(djinn_db::RecentDoctorFindings {
                run_id: Some("leader-tick-expensive-exclusion".to_owned()),
                ..Default::default()
            })
            .await
            .expect("list recent");
        let check_names: std::collections::BTreeSet<&str> =
            rows.iter().map(|row| row.check_name.as_str()).collect();
        assert!(check_names.contains(TEST_CHECK_NAME));
        assert!(
            !check_names.contains("leader_tick.expensive.counter"),
            "expensive check must not produce any doctor_findings row, got check names {check_names:?}",
        );

        // (3) The rendered Prometheus output contains no sample line for the
        // expensive check on either metric. The doctor gauges are not
        // initialized to zero in `register_metrics`, so an absent label
        // combination proves neither `set_findings` nor
        // `set_run_duration_seconds` was called for it.
        let rendered = djinn_telemetry::render().expect("render");
        let expensive_findings_line = rendered.lines().find(|line| {
            line.starts_with("djinn_doctor_findings{")
                && line.contains("leader_tick.expensive.counter")
        });
        assert!(
            expensive_findings_line.is_none(),
            "expensive check must not produce a djinn_doctor_findings sample, got line {:?}",
            expensive_findings_line,
        );
        let expensive_duration_line = rendered.lines().find(|line| {
            line.starts_with("djinn_doctor_run_duration_seconds{")
                && line.contains("leader_tick.expensive.counter")
        });
        assert!(
            expensive_duration_line.is_none(),
            "expensive check must not produce a djinn_doctor_run_duration_seconds sample, got line {:?}",
            expensive_duration_line,
        );
    }

    /// Focused assertion for the rendered Prometheus surface (acceptance
    /// criterion 2 of task `yl2p`): the cheap-check `djinn_doctor_findings`
    /// and `djinn_doctor_run_duration_seconds` samples carry ONLY the stable
    /// `check` label and are not contaminated with severity, entity, or
    /// histogram bucket labels.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn leader_tick_rendered_metrics_carry_only_check_label() {
        let _ = djinn_telemetry::init();
        let db = fresh_db();
        let (events_tx, _events_rx) = broadcast::channel(16);
        let registry = fresh_registry();

        run_cheap_doctor_checks(
            &registry,
            &db,
            &events_tx,
            Some("leader-tick-label-discipline"),
        )
        .await;

        let rendered = djinn_telemetry::render().expect("render");

        let findings_line = rendered
            .lines()
            .find(|line| {
                line.starts_with("djinn_doctor_findings{") && line.contains(TEST_CHECK_NAME)
            })
            .expect("findings metric must be rendered for the cheap check");
        assert!(
            findings_line.contains(&format!("check=\"{TEST_CHECK_NAME}\"")),
            "findings line must carry the stable check label exactly: {findings_line}",
        );
        assert_eq!(
            findings_line.matches('=').count(),
            1,
            "findings line must carry only the check label (one '=' per Prometheus label): {findings_line}",
        );

        let duration_line = rendered
            .lines()
            .find(|line| {
                line.starts_with("djinn_doctor_run_duration_seconds{")
                    && line.contains(TEST_CHECK_NAME)
            })
            .expect("run-duration metric must be rendered for the cheap check");
        assert!(
            duration_line.contains(&format!("check=\"{TEST_CHECK_NAME}\"")),
            "duration line must carry the stable check label exactly: {duration_line}",
        );
        assert_eq!(
            duration_line.matches('=').count(),
            1,
            "duration line must carry only the check label (one '=' per Prometheus label): {duration_line}",
        );

        // No severity / entity / workspace / histogram bucket labels.
        for forbidden in ["severity=", "entity=", "workspace=", "le=", "bucket="] {
            assert!(
                !findings_line.contains(forbidden),
                "findings line must not carry '{forbidden}' label: {findings_line}",
            );
            assert!(
                !duration_line.contains(forbidden),
                "duration line must not carry '{forbidden}' label: {duration_line}",
            );
        }

        // The duration sample must be a non-negative finite f64; the
        // `findings` gauge must be a non-negative finite number.
        let duration_value: f64 = duration_line
            .rsplit(' ')
            .next()
            .expect("duration sample has a value")
            .parse()
            .expect("duration sample is numeric");
        assert!(
            duration_value.is_finite() && duration_value >= 0.0,
            "duration sample must be a non-negative finite number, got {duration_value}",
        );

        let findings_value: f64 = findings_line
            .rsplit(' ')
            .next()
            .expect("findings sample has a value")
            .parse()
            .expect("findings sample is numeric");
        assert!(
            findings_value.is_finite() && findings_value >= 0.0,
            "findings sample must be a non-negative finite number, got {findings_value}",
        );
    }

    /// Cross-channel invariant for acceptance criteria 1 and 2: the
    /// `djinn_doctor_findings{check}` gauge must match the actual number of
    /// `doctor_findings` rows persisted for that check in the same tick run.
    /// A drift between the metric and the persistence layer would silently
    /// mislead operators querying the Prometheus surface.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn leader_tick_findings_metric_matches_persisted_row_count() {
        let _ = djinn_telemetry::init();
        let db = fresh_db();
        let (events_tx, _events_rx) = broadcast::channel(16);
        let registry = fresh_registry();

        let run_id = "leader-tick-metric-vs-persistence";
        run_cheap_doctor_checks(&registry, &db, &events_tx, Some(run_id)).await;

        let finding_repo = DoctorFindingRepository::new(db.clone());
        let rows = finding_repo
            .list_recent(djinn_db::RecentDoctorFindings {
                run_id: Some(run_id.to_owned()),
                check_name: Some(TEST_CHECK_NAME.to_owned()),
                ..Default::default()
            })
            .await
            .expect("list recent");

        let persisted_count = rows.len();
        assert!(
            persisted_count > 0,
            "fixture check must have emitted findings"
        );

        let rendered = djinn_telemetry::render().expect("render");
        let findings_line = rendered
            .lines()
            .find(|line| {
                line.starts_with("djinn_doctor_findings{")
                    && line.contains(&format!("check=\"{TEST_CHECK_NAME}\""))
            })
            .expect("findings metric must be rendered for the cheap check");
        let metric_value: f64 = findings_line
            .rsplit(' ')
            .next()
            .expect("findings sample has a value")
            .parse()
            .expect("findings sample is numeric");
        assert_eq!(
            metric_value as usize, persisted_count,
            "djinn_doctor_findings{{check=\"{TEST_CHECK_NAME}\"}} ({metric_value}) must equal \
             the number of doctor_findings rows persisted for that check in the same \
             leader-tick run ({persisted_count})",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn whole_refresh_failure_preserves_keyed_alarms_and_recovers() {
        let _ = djinn_telemetry::init();
        let db = fresh_db();
        let (events_tx, _events_rx) = broadcast::channel(16);
        let phase = Arc::new(AtomicUsize::new(0));
        let zero_resolver_invocations = Arc::new(AtomicUsize::new(0));
        let starvation_resolver_invocations = Arc::new(AtomicUsize::new(0));
        let registry = DoctorRegistry::new();
        djinn_core::doctor::register(
            &registry,
            LifecycleRetrievalCheck::new(
                "memory.retrieval_zero_result",
                Arc::clone(&phase),
                Arc::clone(&zero_resolver_invocations),
            ),
        );
        djinn_core::doctor::register(
            &registry,
            LifecycleRetrievalCheck::new(
                "memory.injection_starvation",
                Arc::clone(&phase),
                Arc::clone(&starvation_resolver_invocations),
            ),
        );
        djinn_core::doctor::register(
            &registry,
            LifecycleRetrievalCheck::new(
                "memory.retrieval_health_refresh",
                Arc::clone(&phase),
                Arc::new(AtomicUsize::new(0)),
            ),
        );

        // A healthy snapshot automatically creates one keyed alarm per
        // resolver. Keep their complete persisted forms for byte-for-byte
        // comparison after the injected whole-refresh failure.
        run_cheap_doctor_checks(&registry, &db, &events_tx, Some("healthy-alarming")).await;
        let repo = DoctorFindingRepository::new(db.clone());
        let zero_before = repo
            .latest_for_check("memory.retrieval_zero_result")
            .await
            .expect("zero-result row")
            .expect("zero-result created");
        let starvation_before = repo
            .latest_for_check("memory.injection_starvation")
            .await
            .expect("starvation row")
            .expect("starvation created");
        assert_eq!(zero_before.status, "active");
        assert_eq!(starvation_before.status, "active");
        assert_eq!(zero_resolver_invocations.load(Ordering::SeqCst), 1);
        assert_eq!(starvation_resolver_invocations.load(Ordering::SeqCst), 1);

        // The injected repository/source failure emits exactly the structured
        // refresh error, without invoking either resolver or mutating either
        // existing active alarm (including observed_at and refresh evidence).
        phase.store(1, Ordering::SeqCst);
        run_cheap_doctor_checks(&registry, &db, &events_tx, Some("refresh-failure")).await;
        assert_eq!(zero_resolver_invocations.load(Ordering::SeqCst), 1);
        assert_eq!(starvation_resolver_invocations.load(Ordering::SeqCst), 1);
        assert_eq!(
            repo.get(&zero_before.id).await.expect("reload zero"),
            Some(zero_before.clone()),
            "whole refresh failure must preserve zero-result finding byte-for-byte",
        );
        assert_eq!(
            repo.get(&starvation_before.id)
                .await
                .expect("reload starvation"),
            Some(starvation_before.clone()),
            "whole refresh failure must preserve starvation finding byte-for-byte",
        );
        let refresh_rows = repo
            .list_recent(djinn_db::RecentDoctorFindings {
                check_name: Some("memory.retrieval_health_refresh".to_owned()),
                ..Default::default()
            })
            .await
            .expect("refresh rows");
        assert_eq!(refresh_rows.len(), 1);
        assert_eq!(refresh_rows[0].status, "active");
        assert_eq!(refresh_rows[0].severity, "error");
        assert_eq!(
            refresh_rows[0].evidence["error_class"],
            "retrieval_health_refresh_failed"
        );
        assert_eq!(
            refresh_rows[0].evidence["detail"],
            "injected repository refresh failure"
        );

        // A healthy non-alarming refresh evaluates both resolvers again and
        // reconciles all three active keys to resolved, including refresh.
        phase.store(2, Ordering::SeqCst);
        run_cheap_doctor_checks(&registry, &db, &events_tx, Some("healthy-recovery")).await;
        assert_eq!(zero_resolver_invocations.load(Ordering::SeqCst), 2);
        assert_eq!(starvation_resolver_invocations.load(Ordering::SeqCst), 2);
        assert_eq!(
            repo.get(&zero_before.id)
                .await
                .expect("reloaded resolved zero")
                .expect("zero row remains")
                .status,
            "resolved"
        );
        assert_eq!(
            repo.get(&starvation_before.id)
                .await
                .expect("reloaded resolved starvation")
                .expect("starvation row remains")
                .status,
            "resolved"
        );
        assert_eq!(
            repo.get(&refresh_rows[0].id)
                .await
                .expect("reloaded resolved refresh")
                .expect("refresh row remains")
                .status,
            "resolved"
        );
    }
}
