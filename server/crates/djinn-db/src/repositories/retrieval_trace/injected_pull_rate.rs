//! `P(memory_read | Injected)` — the injected-pull-rate rollup (proposal u46i
//! AC6).
//!
//! # What this measures
//!
//! For every candidate a retrieval trace recorded as `injected` in a window,
//! did the same run subsequently issue an explicit `memory_read` for that note?
//!
//! # Why the join needs both new pieces
//!
//! `notes.last_accessed` / `notes.access_count` cannot answer this: they are
//! destructive last-write-wins scalars with no timestamp series, no run
//! identity, and no tool provenance. Migration 189's `note_access_events`
//! ledger supplies the row-per-read side of the join; this module supplies the
//! aggregate.
//!
//! # Join rules (each one is load-bearing)
//!
//! * Join on `note_id`, never `permalink`. Permalinks are mutable — `move_note`
//!   rewrites them — so a permalink join silently loses every note that was
//!   renamed between injection and read.
//! * Only `source = 'memory_read'` events count. `memory_search` also touches
//!   notes (ADR-054) but returning a note in a result set is not a pull.
//! * The event must be strictly newer than the trace (`>`), otherwise a read
//!   that happened *before* the injection would be counted as caused by it.
//! * Correlate on `task_run_id` when both sides have one, else on `session_id`.
//!   Candidates whose trace carries neither are **excluded from the
//!   denominator** and reported separately, rather than being silently counted
//!   as "not pulled".
//!
//! # Honesty
//!
//! This metric reads 0 until events accumulate, and an empty window is not
//! evidence of a zero pull rate. [`InjectedPullRateEvidence`] makes the
//! difference explicit and `pull_rate` is `None` whenever there is no
//! denominator to divide by.

use chrono::{DateTime, Duration, FixedOffset, Utc};
use serde::{Deserialize, Serialize};

use crate::Result as DbResult;
use crate::error::DbError;

use super::RetrievalTraceRepository;

/// Explicit timezone-aware half-open `[start, end)` interval for the operator
/// report, mirroring `task_run_outcome::TaskRunOutcomeReportRequest`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InjectedPullRateRequest {
    pub project_id: String,
    /// RFC-3339 timestamp with an explicit offset.
    pub start: String,
    /// RFC-3339 timestamp with an explicit offset.
    pub end: String,
    /// IANA timezone the caller framed the interval in. Retained on the
    /// response so the numbers are never reinterpreted in another zone.
    pub timezone: String,
}

/// Reject intervals the durable data cannot honestly answer.
///
/// Identical rules to `task_run_outcome::report_interval`: the window must be
/// non-empty, at most 30 days wide, entirely inside the protected retrieval
/// trace retention window ([`super::MINIMUM_RETRIEVAL_TRACE_RETENTION_WINDOW`]),
/// and must not extend into the future. Out-of-range requests are **rejected,
/// not silently clipped** — a clipped window would report a rate for an
/// interval the caller never asked about.
fn report_interval(
    request: &InjectedPullRateRequest,
) -> DbResult<(DateTime<FixedOffset>, DateTime<FixedOffset>)> {
    let start = DateTime::parse_from_rfc3339(&request.start)
        .map_err(|error| DbError::InvalidData(error.to_string()))?;
    let end = DateTime::parse_from_rfc3339(&request.end)
        .map_err(|error| DbError::InvalidData(error.to_string()))?;
    let now = Utc::now();
    if start >= end
        || end.signed_duration_since(start) > Duration::days(30)
        || start.with_timezone(&Utc) < now - Duration::days(30)
        || end.with_timezone(&Utc) > now
    {
        return Err(DbError::InvalidData("unsupported report interval".into()));
    }
    Ok((start, end))
}

/// What the numbers in an [`InjectedPullRateReport`] are actually evidence of.
///
/// An operator reading `pull_rate: 0` must be able to tell "we measured, and
/// nothing was pulled" apart from "there was nothing to measure". Every variant
/// except [`Measured`](Self::Measured) means the latter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InjectedPullRateEvidence {
    /// The window contains no injected candidates at all. Nothing is being
    /// measured; this is not a zero pull rate.
    NoInjectedCandidates,
    /// Injected candidates exist, but none of their traces carry a task run or
    /// session, so no candidate can be correlated with a read. This is an
    /// attribution-coverage gap, not a zero pull rate.
    NoAttributableCandidates,
    /// Attributable injected candidates exist, but the window contains zero
    /// `memory_read` ledger rows of any kind. Until the ledger accumulates
    /// (migration 189 is new), this is missing instrumentation coverage rather
    /// than proof that agents never pull injected notes.
    NoAccessLedgerCoverage,
    /// Attributable injected candidates and `memory_read` ledger rows both
    /// exist in the window. `pull_rate` is a real measurement.
    Measured,
}

/// Coverage diagnostics. These exist so the report can never quietly
/// under-count: every candidate and every read event that the rate could not
/// use is named here.
///
/// Follows the `unattributed_trace_count` / `unrecorded_run_count` convention
/// established by `task_run_outcome.rs`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InjectedPullRateDiagnostics {
    /// Injected candidates whose trace has neither `task_run_id` nor
    /// `session_id`. Excluded from the rate denominator.
    pub unattributable_candidate_count: u64,
    /// Injected candidates whose `note_id` is missing or empty in the stored
    /// candidate JSON. Excluded from every count; a candidate with no stable
    /// identity cannot be joined at all.
    pub identityless_candidate_count: u64,
    /// `memory_read` ledger rows in the window, regardless of whether any
    /// injected candidate matched them. Zero here with a non-zero
    /// `attributable_candidate_count` means the ledger has no coverage yet.
    pub memory_read_event_count: u64,
    /// `memory_read` ledger rows in the window carrying neither a task run nor
    /// a session. These can never match an injected candidate.
    pub unattributed_memory_read_event_count: u64,
    /// `memory_search` ledger rows in the window. Recorded for contrast only —
    /// these are deliberately NOT pulls and never enter the numerator.
    pub memory_search_event_count: u64,
}

/// `P(memory_read | Injected)` over a half-open `[from, until)` UTC window for
/// one project.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InjectedPullRateReport {
    pub project_id: String,
    pub window_start: String,
    pub window_end: String,
    /// Timezone the caller framed the window in. Echoed back so the counts are
    /// never reinterpreted in another zone.
    pub timezone: String,
    /// Every candidate recorded as `injected` in the window, including
    /// unattributable ones.
    pub injected_candidate_count: u64,
    /// Distinct notes among those candidates.
    pub injected_note_count: u64,
    /// Injected candidates whose trace carries a task run or a session — the
    /// denominator of `pull_rate`.
    pub attributable_candidate_count: u64,
    /// Attributable injected candidates followed by a `memory_read` of the same
    /// note by the same run/session — the numerator of `pull_rate`.
    pub pulled_candidate_count: u64,
    /// `pulled_candidate_count / attributable_candidate_count`, or `None` when
    /// the denominator is zero. Never report a bare `0.0` for an empty window.
    pub pull_rate: Option<f64>,
    /// What the numbers above are evidence of. Read this before `pull_rate`.
    pub evidence: InjectedPullRateEvidence,
    pub diagnostics: InjectedPullRateDiagnostics,
}

impl InjectedPullRateReport {
    /// `true` when `pull_rate` is a real measurement rather than an artifact of
    /// an empty window or missing attribution.
    pub fn is_measured(&self) -> bool {
        self.evidence == InjectedPullRateEvidence::Measured
    }
}

#[derive(sqlx::FromRow)]
pub(super) struct InjectedPullRateRow {
    injected_candidate_count: i64,
    injected_note_count: i64,
    attributable_candidate_count: i64,
    unattributable_candidate_count: i64,
    identityless_candidate_count: i64,
    pulled_candidate_count: i64,
    memory_read_event_count: i64,
    unattributed_memory_read_event_count: i64,
    memory_search_event_count: i64,
}

pub(super) fn build_report(
    project_id: &str,
    from: &str,
    until: &str,
    timezone: &str,
    row: InjectedPullRateRow,
) -> InjectedPullRateReport {
    let attributable = row.attributable_candidate_count.max(0) as u64;
    let pulled = row.pulled_candidate_count.max(0) as u64;
    let injected = row.injected_candidate_count.max(0) as u64;
    let memory_read_event_count = row.memory_read_event_count.max(0) as u64;

    let pull_rate = (attributable > 0).then(|| pulled as f64 / attributable as f64);

    let evidence = if injected == 0 {
        InjectedPullRateEvidence::NoInjectedCandidates
    } else if attributable == 0 {
        InjectedPullRateEvidence::NoAttributableCandidates
    } else if memory_read_event_count == 0 {
        InjectedPullRateEvidence::NoAccessLedgerCoverage
    } else {
        InjectedPullRateEvidence::Measured
    };

    InjectedPullRateReport {
        project_id: project_id.to_owned(),
        window_start: from.to_owned(),
        window_end: until.to_owned(),
        timezone: timezone.to_owned(),
        injected_candidate_count: injected,
        injected_note_count: row.injected_note_count.max(0) as u64,
        attributable_candidate_count: attributable,
        pulled_candidate_count: pulled,
        pull_rate,
        evidence,
        diagnostics: InjectedPullRateDiagnostics {
            unattributable_candidate_count: row.unattributable_candidate_count.max(0) as u64,
            identityless_candidate_count: row.identityless_candidate_count.max(0) as u64,
            memory_read_event_count,
            unattributed_memory_read_event_count: row.unattributed_memory_read_event_count.max(0)
                as u64,
            memory_search_event_count: row.memory_search_event_count.max(0) as u64,
        },
    }
}

impl RetrievalTraceRepository {
    /// Compute `P(memory_read | Injected)` for a project over a half-open
    /// `[from, until)` UTC window.
    ///
    /// `from` and `until` are ISO-8601 UTC timestamp strings (e.g.
    /// `2026-08-01T00:00:00.000Z`). Both `retrieval_traces.created_at` and
    /// `note_access_events.created_at` are stored as text, so the query
    /// compares parsed `timestamptz` values on both sides — the same technique
    /// the taxonomy-v1 health query uses — rather than comparing ISO spellings.
    pub async fn injected_pull_rate(
        &self,
        project_id: &str,
        from: &str,
        until: &str,
    ) -> DbResult<InjectedPullRateReport> {
        self.db.ensure_initialized().await?;

        let row: InjectedPullRateRow = sqlx::query_as(INJECTED_PULL_RATE_SQL)
            .bind(project_id)
            .bind(from)
            .bind(until)
            .fetch_one(self.db.pool())
            .await?;

        Ok(build_report(project_id, from, until, "UTC", row))
    }

    /// Operator entry point: validate the requested interval, then compute the
    /// rate.
    ///
    /// Unlike [`Self::injected_pull_rate`], this rejects intervals outside the
    /// protected 30-day retrieval-trace retention window instead of returning
    /// numbers for a window whose traces may already have been pruned. A pruned
    /// window would show injected candidates disappearing while their reads
    /// remain, which reads as a *falling* pull rate for a purely mechanical
    /// reason.
    pub async fn injected_pull_rate_report(
        &self,
        request: InjectedPullRateRequest,
    ) -> DbResult<InjectedPullRateReport> {
        let (start, end) = report_interval(&request)?;
        self.db.ensure_initialized().await?;

        let start = start.to_rfc3339();
        let end = end.to_rfc3339();
        let row: InjectedPullRateRow = sqlx::query_as(INJECTED_PULL_RATE_SQL)
            .bind(&request.project_id)
            .bind(&start)
            .bind(&end)
            .fetch_one(self.db.pool())
            .await?;

        Ok(build_report(
            &request.project_id,
            &start,
            &end,
            &request.timezone,
            row,
        ))
    }
}

pub(super) const INJECTED_PULL_RATE_SQL: &str = r#"
    WITH bounded_traces AS (
        SELECT id, session_id, task_run_id, created_at, candidates
        FROM retrieval_traces
        WHERE project_id = $1
          AND created_at::timestamptz >= $2::text::timestamptz
          AND created_at::timestamptz <  $3::text::timestamptz
    ),
    injected AS (
        SELECT
            t.session_id,
            t.task_run_id,
            t.created_at::timestamptz AS injected_at,
            nullif(btrim(coalesce(c.value->>'note_id', '')), '') AS note_id
        FROM bounded_traces t
        CROSS JOIN LATERAL jsonb_array_elements(
            CASE WHEN jsonb_typeof(t.candidates) = 'array' THEN t.candidates ELSE '[]'::jsonb END
        ) c
        WHERE c.value->>'outcome' = 'injected'
    ),
    scored AS (
        SELECT
            i.note_id,
            (i.task_run_id IS NOT NULL OR i.session_id IS NOT NULL) AS attributable,
            EXISTS (
                SELECT 1
                FROM note_access_events e
                WHERE e.project_id = $1
                  AND e.note_id = i.note_id
                  AND e.source = 'memory_read'
                  AND e.created_at::timestamptz > i.injected_at
                  AND (
                        (i.task_run_id IS NOT NULL AND e.task_run_id = i.task_run_id)
                     OR (i.session_id  IS NOT NULL AND e.session_id  = i.session_id)
                  )
            ) AS pulled
        FROM injected i
        WHERE i.note_id IS NOT NULL
    ),
    ledger AS (
        SELECT
            count(*) FILTER (WHERE source = 'memory_read')::bigint
                AS memory_read_event_count,
            count(*) FILTER (
                WHERE source = 'memory_read'
                  AND task_run_id IS NULL
                  AND session_id IS NULL
            )::bigint AS unattributed_memory_read_event_count,
            count(*) FILTER (WHERE source = 'memory_search')::bigint
                AS memory_search_event_count
        FROM note_access_events
        WHERE project_id = $1
          AND created_at::timestamptz >= $2::text::timestamptz
          AND created_at::timestamptz <  $3::text::timestamptz
    ),
    candidates AS (
        SELECT
            count(*)::bigint AS injected_candidate_count,
            count(DISTINCT note_id)::bigint AS injected_note_count,
            count(*) FILTER (WHERE attributable)::bigint AS attributable_candidate_count,
            count(*) FILTER (WHERE NOT attributable)::bigint AS unattributable_candidate_count,
            count(*) FILTER (WHERE attributable AND pulled)::bigint AS pulled_candidate_count
        FROM scored
    ),
    identityless AS (
        SELECT count(*)::bigint AS identityless_candidate_count
        FROM injected
        WHERE note_id IS NULL
    )
    SELECT
        c.injected_candidate_count,
        c.injected_note_count,
        c.attributable_candidate_count,
        c.unattributable_candidate_count,
        idl.identityless_candidate_count,
        c.pulled_candidate_count,
        l.memory_read_event_count,
        l.unattributed_memory_read_event_count,
        l.memory_search_event_count
    FROM candidates c
    CROSS JOIN identityless idl
    CROSS JOIN ledger l
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        injected: i64,
        attributable: i64,
        pulled: i64,
        memory_read_events: i64,
    ) -> InjectedPullRateRow {
        InjectedPullRateRow {
            injected_candidate_count: injected,
            injected_note_count: injected,
            attributable_candidate_count: attributable,
            unattributable_candidate_count: injected - attributable,
            identityless_candidate_count: 0,
            pulled_candidate_count: pulled,
            memory_read_event_count: memory_read_events,
            unattributed_memory_read_event_count: 0,
            memory_search_event_count: 0,
        }
    }

    #[test]
    fn empty_window_is_not_a_zero_pull_rate() {
        let report = build_report("p", "a", "b", "UTC", row(0, 0, 0, 0));
        assert_eq!(
            report.evidence,
            InjectedPullRateEvidence::NoInjectedCandidates
        );
        assert_eq!(report.pull_rate, None);
        assert!(!report.is_measured());
    }

    #[test]
    fn injected_but_unattributable_is_reported_as_a_coverage_gap() {
        let report = build_report("p", "a", "b", "UTC", row(4, 0, 0, 3));
        assert_eq!(
            report.evidence,
            InjectedPullRateEvidence::NoAttributableCandidates
        );
        assert_eq!(report.pull_rate, None);
        assert_eq!(report.diagnostics.unattributable_candidate_count, 4);
    }

    #[test]
    fn attributable_candidates_with_an_empty_ledger_are_not_a_proven_zero() {
        let report = build_report("p", "a", "b", "UTC", row(4, 4, 0, 0));
        assert_eq!(
            report.evidence,
            InjectedPullRateEvidence::NoAccessLedgerCoverage
        );
        // The rate is still computed, but `evidence` says not to trust it.
        assert_eq!(report.pull_rate, Some(0.0));
        assert!(!report.is_measured());
    }

    #[test]
    fn a_real_zero_is_distinguishable_from_an_empty_window() {
        let report = build_report("p", "a", "b", "UTC", row(4, 4, 0, 9));
        assert_eq!(report.evidence, InjectedPullRateEvidence::Measured);
        assert_eq!(report.pull_rate, Some(0.0));
        assert!(report.is_measured());
    }

    #[test]
    fn rate_is_pulled_over_attributable_not_over_injected() {
        // 10 injected, only 4 attributable, 2 of those pulled.
        let report = build_report("p", "a", "b", "UTC", row(10, 4, 2, 7));
        assert_eq!(report.pull_rate, Some(0.5));
        assert_eq!(report.injected_candidate_count, 10);
        assert_eq!(report.diagnostics.unattributable_candidate_count, 6);
        assert_eq!(report.evidence, InjectedPullRateEvidence::Measured);
    }
}

#[cfg(test)]
mod db_tests {
    //! Database-backed proof that the SQL actually joins the ledger to the
    //! trace. The unit tests above only cover the classification arithmetic;
    //! these exercise `INJECTED_PULL_RATE_SQL` against real rows.

    use super::*;

    use crate::NoteRepository;
    use crate::database::Database;
    use crate::repositories::note::{NoteAccessAttribution, NoteAccessSource};
    use crate::repositories::retrieval_trace::{
        CandidateOutcome, CreateRetrievalTraceParams, DEFAULT_CANDIDATE_CAP,
        RetrievalTraceEntryPoint, RetrievalTraceRepository, TraceCandidate,
    };
    use djinn_core::events::EventBus;

    const WINDOW_START: &str = "2000-01-01T00:00:00.000Z";
    const WINDOW_END: &str = "2999-01-01T00:00:00.000Z";
    /// Every trace in these tests is backdated here so an access event written
    /// with `now()` is unambiguously *after* the injection.
    const TRACE_AT: &str = "2026-01-01T00:00:00.000Z";

    async fn seed_project(db: &Database) -> String {
        db.ensure_initialized().await.unwrap();
        let project_id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO projects (id, name, github_owner, github_repo)
             VALUES ($1, $2, 'test-owner', $2)",
        )
        .bind(&project_id)
        .bind(format!("proj-{project_id}"))
        .execute(db.pool())
        .await
        .unwrap();
        project_id
    }

    /// Seed the epic → task → task_run chain a `sessions.task_run_id` FK needs.
    async fn seed_task_run(db: &Database, project_id: &str) -> String {
        let creator = crate::repositories::test_support::seed_test_user(db).await;
        let epic_id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO epics (id, project_id, short_id, title, description, emoji, color, owner, memory_refs)
             VALUES ($1, $2, $3, 'pull-rate epic', '', '', '', '', '[]'::jsonb)",
        )
        .bind(&epic_id)
        .bind(project_id)
        .bind(format!("ep-{}", &epic_id[epic_id.len() - 12..]))
        .execute(db.pool())
        .await
        .unwrap();

        let task_id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design, \
             issue_type, priority, owner, status, continuation_count, labels, acceptance_criteria, \
             memory_refs, created_by_user_id) \
             VALUES ($1, $2, $3, $4, 'pull-rate task', '', '', 'task', 0, '', 'open', 0, \
                     '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, $5)",
        )
        .bind(&task_id)
        .bind(project_id)
        .bind(format!("t-{}", &task_id[task_id.len() - 12..]))
        .bind(&epic_id)
        .bind(&creator)
        .execute(db.pool())
        .await
        .unwrap();

        let task_run_id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO task_runs (id, project_id, task_id, trigger_type, status)
             VALUES ($1, $2, $3, 'manual', 'running')",
        )
        .bind(&task_run_id)
        .bind(project_id)
        .bind(&task_id)
        .execute(db.pool())
        .await
        .unwrap();

        task_run_id
    }

    fn injected(note_id: &str) -> TraceCandidate {
        TraceCandidate {
            note_id: note_id.to_string(),
            permalink: None,
            title: None,
            outcome: CandidateOutcome::Injected,
            rank: Some(1),
            confidence: Some(0.9),
            skipped_reason: None,
            source: Some("scope_overlap".to_string()),
            scope: None,
        }
    }

    async fn insert_backdated_trace(
        traces: &RetrievalTraceRepository,
        project_id: &str,
        session_id: Option<&str>,
        task_run_id: Option<&str>,
        candidates: &[TraceCandidate],
    ) {
        let candidates = serde_json::to_value(candidates).unwrap();
        let row = traces
            .insert(CreateRetrievalTraceParams {
                project_id,
                session_id,
                task_run_id,
                task_id: None,
                entry_point: RetrievalTraceEntryPoint::Dispatch,
                trigger: None,
                candidates: &candidates,
                candidate_cap: DEFAULT_CANDIDATE_CAP,
                candidate_cap_exceeded: false,
                sampling_metadata: None,
                durations_ms: &serde_json::json!({}),
                estimated_injected_tokens: 10,
            })
            .await
            .unwrap();
        traces.update_created_at(&row.id, TRACE_AT).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_memory_read_by_the_same_run_counts_as_a_pull() {
        let db = Database::open_in_memory().unwrap();
        let project_id = seed_project(&db).await;
        let notes = NoteRepository::new(db.clone(), EventBus::noop());
        let traces = RetrievalTraceRepository::new(db.clone());

        let pulled = notes
            .create(&project_id, "Pulled", "body", "reference", "[]")
            .await
            .unwrap();
        let ignored = notes
            .create(&project_id, "Ignored", "body", "reference", "[]")
            .await
            .unwrap();

        let task_run_id = uuid::Uuid::now_v7().to_string();
        insert_backdated_trace(
            &traces,
            &project_id,
            None,
            Some(&task_run_id),
            &[injected(&pulled.id), injected(&ignored.id)],
        )
        .await;

        notes
            .touch_accessed(
                &pulled.id,
                NoteAccessSource::MemoryRead,
                &NoteAccessAttribution::for_run(None, Some(task_run_id.clone())),
            )
            .await
            .unwrap();

        let report = traces
            .injected_pull_rate(&project_id, WINDOW_START, WINDOW_END)
            .await
            .unwrap();

        assert_eq!(report.injected_candidate_count, 2);
        assert_eq!(report.attributable_candidate_count, 2);
        assert_eq!(report.pulled_candidate_count, 1);
        assert_eq!(report.pull_rate, Some(0.5));
        assert_eq!(report.evidence, InjectedPullRateEvidence::Measured);
        assert_eq!(report.diagnostics.memory_read_event_count, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_memory_search_touch_is_never_a_pull() {
        let db = Database::open_in_memory().unwrap();
        let project_id = seed_project(&db).await;
        let notes = NoteRepository::new(db.clone(), EventBus::noop());
        let traces = RetrievalTraceRepository::new(db.clone());

        let note = notes
            .create(&project_id, "Searched", "body", "reference", "[]")
            .await
            .unwrap();
        let task_run_id = uuid::Uuid::now_v7().to_string();
        insert_backdated_trace(
            &traces,
            &project_id,
            None,
            Some(&task_run_id),
            &[injected(&note.id)],
        )
        .await;

        // Same note, same run, but the access came from a search result set.
        notes
            .touch_accessed(
                &note.id,
                NoteAccessSource::MemorySearch,
                &NoteAccessAttribution::for_run(None, Some(task_run_id.clone())),
            )
            .await
            .unwrap();

        let report = traces
            .injected_pull_rate(&project_id, WINDOW_START, WINDOW_END)
            .await
            .unwrap();

        assert_eq!(report.pulled_candidate_count, 0);
        assert_eq!(report.diagnostics.memory_read_event_count, 0);
        assert_eq!(report.diagnostics.memory_search_event_count, 1);
        // No `memory_read` rows at all: this is missing coverage, not a proven
        // zero pull rate.
        assert_eq!(
            report.evidence,
            InjectedPullRateEvidence::NoAccessLedgerCoverage
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_read_by_a_different_run_is_not_credited() {
        let db = Database::open_in_memory().unwrap();
        let project_id = seed_project(&db).await;
        let notes = NoteRepository::new(db.clone(), EventBus::noop());
        let traces = RetrievalTraceRepository::new(db.clone());

        let note = notes
            .create(&project_id, "Cross Run", "body", "reference", "[]")
            .await
            .unwrap();
        insert_backdated_trace(
            &traces,
            &project_id,
            None,
            Some(&uuid::Uuid::now_v7().to_string()),
            &[injected(&note.id)],
        )
        .await;

        notes
            .touch_accessed(
                &note.id,
                NoteAccessSource::MemoryRead,
                &NoteAccessAttribution::for_run(None, Some(uuid::Uuid::now_v7().to_string())),
            )
            .await
            .unwrap();

        let report = traces
            .injected_pull_rate(&project_id, WINDOW_START, WINDOW_END)
            .await
            .unwrap();

        assert_eq!(report.attributable_candidate_count, 1);
        assert_eq!(report.pulled_candidate_count, 0);
        assert_eq!(report.pull_rate, Some(0.0));
        // A real measurement: the ledger has coverage, the read just belonged
        // to another run.
        assert_eq!(report.evidence, InjectedPullRateEvidence::Measured);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unattributable_candidates_leave_the_denominator_and_are_reported() {
        let db = Database::open_in_memory().unwrap();
        let project_id = seed_project(&db).await;
        let notes = NoteRepository::new(db.clone(), EventBus::noop());
        let traces = RetrievalTraceRepository::new(db.clone());

        let note = notes
            .create(&project_id, "Orphan Trace", "body", "reference", "[]")
            .await
            .unwrap();
        // No session, no run: nothing can ever be correlated with this.
        insert_backdated_trace(&traces, &project_id, None, None, &[injected(&note.id)]).await;

        let report = traces
            .injected_pull_rate(&project_id, WINDOW_START, WINDOW_END)
            .await
            .unwrap();

        assert_eq!(report.injected_candidate_count, 1);
        assert_eq!(report.attributable_candidate_count, 0);
        assert_eq!(report.diagnostics.unattributable_candidate_count, 1);
        assert_eq!(report.pull_rate, None);
        assert_eq!(
            report.evidence,
            InjectedPullRateEvidence::NoAttributableCandidates
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_only_attribution_still_correlates() {
        let db = Database::open_in_memory().unwrap();
        let project_id = seed_project(&db).await;
        let notes = NoteRepository::new(db.clone(), EventBus::noop());
        let traces = RetrievalTraceRepository::new(db.clone());

        let note = notes
            .create(&project_id, "Session Join", "body", "reference", "[]")
            .await
            .unwrap();
        let session_id = uuid::Uuid::now_v7().to_string();
        insert_backdated_trace(
            &traces,
            &project_id,
            Some(&session_id),
            None,
            &[injected(&note.id)],
        )
        .await;

        notes
            .touch_accessed(
                &note.id,
                NoteAccessSource::MemoryRead,
                &NoteAccessAttribution::for_session(session_id.clone()),
            )
            .await
            .unwrap();

        let report = traces
            .injected_pull_rate(&project_id, WINDOW_START, WINDOW_END)
            .await
            .unwrap();

        assert_eq!(report.pulled_candidate_count, 1);
        assert_eq!(report.pull_rate, Some(1.0));
        assert_eq!(report.evidence, InjectedPullRateEvidence::Measured);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_read_that_predates_the_injection_is_not_a_pull() {
        let db = Database::open_in_memory().unwrap();
        let project_id = seed_project(&db).await;
        let notes = NoteRepository::new(db.clone(), EventBus::noop());
        let traces = RetrievalTraceRepository::new(db.clone());

        let note = notes
            .create(&project_id, "Stale Read", "body", "reference", "[]")
            .await
            .unwrap();
        let task_run_id = uuid::Uuid::now_v7().to_string();

        // Read first (recorded at `now()`), then backdate the trace far into
        // the future relative to it.
        notes
            .touch_accessed(
                &note.id,
                NoteAccessSource::MemoryRead,
                &NoteAccessAttribution::for_run(None, Some(task_run_id.clone())),
            )
            .await
            .unwrap();

        let candidates = serde_json::to_value([injected(&note.id)]).unwrap();
        let row = traces
            .insert(CreateRetrievalTraceParams {
                project_id: &project_id,
                session_id: None,
                task_run_id: Some(&task_run_id),
                task_id: None,
                entry_point: RetrievalTraceEntryPoint::Dispatch,
                trigger: None,
                candidates: &candidates,
                candidate_cap: DEFAULT_CANDIDATE_CAP,
                candidate_cap_exceeded: false,
                sampling_metadata: None,
                durations_ms: &serde_json::json!({}),
                estimated_injected_tokens: 10,
            })
            .await
            .unwrap();
        traces
            .update_created_at(&row.id, "2100-01-01T00:00:00.000Z")
            .await
            .unwrap();

        let report = traces
            .injected_pull_rate(&project_id, WINDOW_START, WINDOW_END)
            .await
            .unwrap();

        assert_eq!(report.attributable_candidate_count, 1);
        assert_eq!(report.pulled_candidate_count, 0);
        assert_eq!(report.diagnostics.memory_read_event_count, 1);
        assert_eq!(report.evidence, InjectedPullRateEvidence::Measured);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn touch_accessed_writes_one_ledger_row_per_access() {
        let db = Database::open_in_memory().unwrap();
        let project_id = seed_project(&db).await;
        let notes = NoteRepository::new(db.clone(), EventBus::noop());

        let note = notes
            .create(&project_id, "Ledger Rows", "body", "reference", "[]")
            .await
            .unwrap();

        for _ in 0..3 {
            notes
                .touch_accessed(
                    &note.id,
                    NoteAccessSource::MemoryRead,
                    &NoteAccessAttribution::unattributed(),
                )
                .await
                .unwrap();
        }

        let events =
            crate::repositories::note::note_access_events_for_note(&db, &project_id, &note.id)
                .await
                .unwrap();
        assert_eq!(
            events.len(),
            3,
            "ledger is append-only, not last-write-wins"
        );
        assert!(events.iter().all(|event| event.source == "memory_read"));
        assert!(events.iter().all(|event| event.session_id.is_none()));

        // The destructive scalar still moves — the ledger is additive, not a
        // replacement.
        let updated = notes.get(&note.id).await.unwrap().unwrap();
        assert_eq!(updated.access_count, 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_attribution_resolves_the_task_run_from_the_sessions_row() {
        let db = Database::open_in_memory().unwrap();
        let project_id = seed_project(&db).await;
        let notes = NoteRepository::new(db.clone(), EventBus::noop());

        let note = notes
            .create(&project_id, "Run Resolution", "body", "reference", "[]")
            .await
            .unwrap();

        let session_id = uuid::Uuid::now_v7().to_string();
        let task_run_id = seed_task_run(&db, &project_id).await;
        sqlx::query(
            "INSERT INTO sessions (id, project_id, model_id, agent_type, status, task_run_id)
             VALUES ($1, $2, 'test-model', 'worker', 'running', $3)",
        )
        .bind(&session_id)
        .bind(&project_id)
        .bind(&task_run_id)
        .execute(db.pool())
        .await
        .unwrap();

        notes
            .touch_accessed(
                &note.id,
                NoteAccessSource::MemoryRead,
                &NoteAccessAttribution::for_session(session_id.clone()),
            )
            .await
            .unwrap();

        let events =
            crate::repositories::note::note_access_events_for_note(&db, &project_id, &note.id)
                .await
                .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].session_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(
            events[0].task_run_id.as_deref(),
            Some(task_run_id.as_str()),
            "a session-only attribution must resolve its run so the trace join \
             has the same correlation key on both sides"
        );
    }
}
