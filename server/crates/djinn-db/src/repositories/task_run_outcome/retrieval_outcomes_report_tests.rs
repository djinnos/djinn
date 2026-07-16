use chrono::{DateTime, Duration, FixedOffset, Utc};
use djinn_core::events::EventBus;
use djinn_core::models::TaskRunTrigger;
use serde_json::{Value, json};

use super::*;
use crate::repositories::epic::EpicRepository;
use crate::repositories::retrieval_trace::{
    CreateRetrievalTraceParams, CreateRetrievalTraceWithSemanticsParams, RetrievalTraceEntryPoint,
    RetrievalTraceOutcome, RetrievalTraceRepository,
};
use crate::repositories::session::{CreateSessionParams, SessionRepository};
use crate::repositories::task_attempt::{CreateTaskAttemptParams, TaskAttemptRepository};
use crate::repositories::task_run::CreateTaskRunParams;

struct ReportFixture {
    db: Database,
    project_id: String,
    task_id: String,
    next_attempt: i32,
}

impl ReportFixture {
    async fn new() -> Self {
        let db = Database::ephemeral().await.unwrap();
        let epic = EpicRepository::new(db.clone(), EventBus::noop())
            .create("retrieval report", "", "", "", "", None)
            .await
            .unwrap();
        let task_id = uuid::Uuid::now_v7().to_string();
        let short_id = format!("r{}", &task_id[..12]);
        sqlx::query(
            "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design, \
             issue_type, priority, owner, status, continuation_count, labels, acceptance_criteria, memory_refs) \
             VALUES ($1, $2, $3, $4, 'Report fixture', '', '', 'task', 0, '', 'open', 0, \
                     '[]'::jsonb, '[]'::jsonb, '[]'::jsonb)",
        )
        .bind(&task_id)
        .bind(&epic.project_id)
        .bind(&short_id)
        .bind(&epic.id)
        .execute(db.pool())
        .await
        .unwrap();
        Self {
            db,
            project_id: epic.project_id,
            task_id,
            next_attempt: 0,
        }
    }

    async fn run_at(&mut self, started_at: DateTime<Utc>) -> String {
        self.next_attempt += 1;
        let attempt_id = uuid::Uuid::now_v7().to_string();
        let dispatch_key = format!("report-attempt-{}", self.next_attempt);
        let attempt = TaskAttemptRepository::new(self.db.clone())
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &attempt_id,
                task_id: &self.task_id,
                role: "worker",
                dispatch_key: &dispatch_key,
                session_id: None,
                attempt_seq: Some(self.next_attempt),
            })
            .await
            .unwrap();
        let run_id = uuid::Uuid::now_v7().to_string();
        TaskRunOutcomeRepository::new(self.db.clone())
            .create_run_for_attempt(
                CreateTaskRunParams {
                    id: &run_id,
                    project_id: &self.project_id,
                    task_id: &self.task_id,
                    trigger_type: TaskRunTrigger::NewTask.as_str(),
                    status: None,
                    workspace_path: None,
                    mirror_ref: None,
                },
                &attempt.id,
            )
            .await
            .unwrap();
        sqlx::query("UPDATE task_runs SET started_at = $2 WHERE id = $1")
            .bind(&run_id)
            .bind(started_at.to_rfc3339())
            .execute(self.db.pool())
            .await
            .unwrap();
        run_id
    }

    async fn session(&self, run_id: &str) -> String {
        SessionRepository::new(self.db.clone(), EventBus::noop())
            .create(CreateSessionParams {
                project_id: &self.project_id,
                task_id: Some(&self.task_id),
                model: "test-model",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: Some(run_id),
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap()
            .id
    }

    async fn trace(
        &self,
        run_id: Option<&str>,
        session_id: Option<&str>,
        entry_point: RetrievalTraceEntryPoint,
        rollout_label: &str,
        outcome: RetrievalTraceOutcome,
    ) -> String {
        let (candidates, tokens) = match outcome {
            RetrievalTraceOutcome::Injected => (injected_candidates(), 12),
            _ => (skipped_candidates(), 0),
        };
        RetrievalTraceRepository::new(self.db.clone())
            .insert_with_semantics(CreateRetrievalTraceWithSemanticsParams {
                trace: CreateRetrievalTraceParams {
                    project_id: &self.project_id,
                    session_id,
                    task_run_id: run_id,
                    task_id: Some(&self.task_id),
                    entry_point,
                    trigger: None,
                    candidates: &candidates,
                    candidate_cap: 50,
                    candidate_cap_exceeded: false,
                    sampling_metadata: None,
                    durations_ms: &json!({}),
                    estimated_injected_tokens: tokens,
                },
                rollout_label,
                outcome,
            })
            .await
            .unwrap()
            .id
    }

    async fn report(
        &self,
        start: String,
        end: String,
        timezone: &str,
    ) -> Result<TaskRunOutcomeReport> {
        TaskRunOutcomeRepository::new(self.db.clone())
            .retrieval_outcomes_report(TaskRunOutcomeReportRequest {
                project_id: self.project_id.clone(),
                start,
                end,
                timezone: timezone.to_owned(),
            })
            .await
    }
}

fn injected_candidates() -> Value {
    json!([{
        "note_id": "injected-note", "outcome": "injected", "rank": 1,
        "confidence": 0.9, "skipped_reason": null, "source": null, "scope": null
    }])
}

fn skipped_candidates() -> Value {
    json!([{
        "note_id": "skipped-note", "outcome": "skipped", "rank": 1,
        "confidence": 0.2, "skipped_reason": "not_top_k", "source": null, "scope": null
    }])
}

fn recent_interval() -> (DateTime<Utc>, DateTime<Utc>) {
    let end = Utc::now() - Duration::minutes(1);
    (end - Duration::hours(2), end)
}

fn cell<'a>(
    report: &'a TaskRunOutcomeReport,
    rollout: &str,
    outcome: &str,
) -> &'a TaskRunOutcomeReportCell {
    report
        .cells
        .iter()
        .find(|cell| cell.rollout_label == rollout && cell.outcome == outcome)
        .unwrap()
}

#[tokio::test]
async fn report_deduplicates_trace_and_session_fanout_but_keeps_attempts_and_overlapping_cells() {
    let mut fixture = ReportFixture::new().await;
    let (start, end) = recent_interval();
    let first = fixture.run_at(start + Duration::minutes(10)).await;
    let second = fixture.run_at(start + Duration::minutes(20)).await;
    let session_one = fixture.session(&first).await;
    let session_two = fixture.session(&first).await;

    for session in [&session_one, &session_two] {
        fixture
            .trace(
                Some(&first),
                Some(session),
                RetrievalTraceEntryPoint::Dispatch,
                "cohort:a",
                RetrievalTraceOutcome::Injected,
            )
            .await;
    }
    fixture
        .trace(
            Some(&first),
            Some(&session_one),
            RetrievalTraceEntryPoint::Dispatch,
            "cohort:a",
            RetrievalTraceOutcome::Injected,
        )
        .await;
    fixture
        .trace(
            Some(&second),
            None,
            RetrievalTraceEntryPoint::Dispatch,
            "cohort:a",
            RetrievalTraceOutcome::Injected,
        )
        .await;
    fixture
        .trace(
            Some(&first),
            None,
            RetrievalTraceEntryPoint::LoadKnowledgeContext,
            "cohort:b",
            RetrievalTraceOutcome::Empty,
        )
        .await;

    let report = fixture
        .report(start.to_rfc3339(), end.to_rfc3339(), "UTC")
        .await
        .unwrap();
    assert!(report.cells_are_non_additive);
    let injected = cell(&report, "cohort:a", "injected");
    assert_eq!(
        injected.denominator, 2,
        "one run is counted once despite three traces across two sessions"
    );
    assert_eq!(
        injected
            .attempts
            .iter()
            .map(|item| (item.attempt_seq, item.count))
            .collect::<Vec<_>>(),
        vec![(Some(1), 1), (Some(2), 1)]
    );
    assert_eq!(cell(&report, "cohort:b", "empty").denominator, 1);
    assert_eq!(
        report
            .cells
            .iter()
            .map(|cell| cell.denominator)
            .sum::<u64>(),
        3
    );
}

#[tokio::test]
async fn report_keeps_diagnostics_and_conservatively_classifies_historical_evidence() {
    let mut fixture = ReportFixture::new().await;
    let (start, end) = recent_interval();
    let disabled = fixture.run_at(start + Duration::minutes(10)).await;
    let historical = fixture.run_at(start + Duration::minutes(20)).await;
    let _unrecorded = fixture.run_at(start + Duration::minutes(30)).await;
    fixture
        .trace(
            Some(&disabled),
            None,
            RetrievalTraceEntryPoint::LoadKnowledgeContext,
            "off",
            RetrievalTraceOutcome::DisabledOff,
        )
        .await;
    let historical_trace = fixture
        .trace(
            Some(&historical),
            None,
            RetrievalTraceEntryPoint::Dispatch,
            "temporary",
            RetrievalTraceOutcome::Empty,
        )
        .await;
    // Simulate a pre-invariant row whose persisted claim contradicts malformed evidence.
    sqlx::query(
        "UPDATE retrieval_traces SET rollout_label = 'legacy', outcome = 'injected', \
         candidates = $2, estimated_injected_tokens = 50 WHERE id = $1",
    )
    .bind(&historical_trace)
    .bind(json!({"malformed": true}))
    .execute(fixture.db.pool())
    .await
    .unwrap();
    let unattributed_trace = fixture
        .trace(
            None,
            None,
            RetrievalTraceEntryPoint::Dispatch,
            "cohort:unattributed",
            RetrievalTraceOutcome::Empty,
        )
        .await;
    // Diagnostics use trace time rather than run time, so place it inside the request.
    sqlx::query("UPDATE retrieval_traces SET created_at = $2 WHERE id = $1")
        .bind(&unattributed_trace)
        .bind((start + Duration::minutes(40)).to_rfc3339())
        .execute(fixture.db.pool())
        .await
        .unwrap();

    let report = fixture
        .report(start.to_rfc3339(), end.to_rfc3339(), "UTC")
        .await
        .unwrap();
    assert_eq!(report.diagnostics.unattributed_trace_count, 1);
    assert_eq!(report.diagnostics.unrecorded_run_count, 1);
    assert_eq!(cell(&report, "off", "disabled_off").denominator, 1);
    assert_eq!(cell(&report, "legacy", "legacy_unknown").denominator, 1);
    assert!(!report.cells.iter().any(|cell| cell.outcome == "injected"));
    assert_eq!(
        report
            .cells
            .iter()
            .map(|cell| cell.denominator)
            .sum::<u64>(),
        2,
        "diagnostics must not enter report denominators"
    );
}

#[tokio::test]
async fn report_applies_half_open_timezone_aware_interval_and_echoes_request() {
    let mut fixture = ReportFixture::new().await;
    let offset = FixedOffset::west_opt(7 * 60 * 60).unwrap();
    let end_utc = Utc::now() - Duration::minutes(2);
    let start_utc = end_utc - Duration::hours(1);
    let included = fixture.run_at(start_utc).await;
    let excluded = fixture.run_at(end_utc).await;
    for run in [&included, &excluded] {
        fixture
            .trace(
                Some(run),
                None,
                RetrievalTraceEntryPoint::Dispatch,
                "boundary",
                RetrievalTraceOutcome::Injected,
            )
            .await;
    }
    let start = start_utc.with_timezone(&offset).to_rfc3339();
    let end = end_utc.with_timezone(&offset).to_rfc3339();
    let report = fixture
        .report(start.clone(), end.clone(), "America/Los_Angeles")
        .await
        .unwrap();
    assert_eq!(report.start, start);
    assert_eq!(report.end, end);
    assert_eq!(report.timezone, "America/Los_Angeles");
    assert_eq!(cell(&report, "boundary", "injected").denominator, 1);
}

#[tokio::test]
async fn report_rejects_overlong_and_pre_retention_intervals_without_clipping() {
    let fixture = ReportFixture::new().await;
    let now = Utc::now() - Duration::minutes(1);
    let overlong = fixture
        .report(
            (now - Duration::days(31)).to_rfc3339(),
            now.to_rfc3339(),
            "UTC",
        )
        .await;
    assert!(overlong.is_err());

    let old_start = now - Duration::days(30) - Duration::hours(1);
    let before_retained_window = fixture
        .report(
            old_start.to_rfc3339(),
            (old_start + Duration::minutes(30)).to_rfc3339(),
            "UTC",
        )
        .await;
    assert!(before_retained_window.is_err());
}
