//! Doctor seed checks: disposition orphan + defer-forever.
//!
//! - [`DispositionOrphanCheck`] — flags any task row in `in_progress` or
//!   `pr_review` status where there is no live session, no queued/remembered
//!   dispatch, and no open PR. The "deferred forever" class.
//! - [`DeferForeverCheck`] — flags any task row whose `deferred_until` is in
//!   the past by more than N hours (default 6) and whose dispatch gate is
//!   currently satisfied (image ready, no unresolved blockers, capacity free
//!   for the task's role/model).
//!
//! Both checks are read-only detectors. They do not mutate state and do not
//! import `supervisor_impl::pr` (per the
//! `pitfalls/coupling-non-pr-diagnostics-to-pr-open-disposition-code`
//! guardrail — the disposition-orphan check is about *task* disposition, not
//! *PR-open* disposition, and is safe in `djinn-core`).
//!
//! # Shared-resolver invariant
//!
//! Each check defines a private `resolve()` helper. `run()` calls `resolve()`
//! and embeds both inputs and outputs in a [`ResolverSnapshot`] on the
//! returned [`Finding`]. A future `fix()` implementation MUST re-run the same
//! `resolve()` with `finding.resolver_snapshot.inputs` to derive expected
//! state — never a hard-coded value.

use std::sync::Arc;

use time::OffsetDateTime;

use super::super::{DoctorCheck, DoctorResult, Finding, FindingSeverity, ResolverSnapshot};

// ---------------------------------------------------------------------------
// Data access trait
// ---------------------------------------------------------------------------

/// A single task row observed by the disposition checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskDispositionRow {
    /// Stable task id (e.g. `019ed083-...`).
    pub task_id: String,
    /// Current task status (`in_progress`, `pr_review`, `deferred`, etc.).
    pub status: String,
    /// Whether there is a `sessions` row with `status = 'running'` for this
    /// task.
    pub has_running_session: bool,
    /// Whether there is a queued or remembered dispatch marker
    /// (`inflight_dispatches` / `last_dispatched`) for this task.
    pub has_inflight_dispatch: bool,
    /// Whether the task has an open PR (`pr_url` is set and the PR is not
    /// closed).
    pub has_open_pr: bool,
    /// Optional `deferred_until` timestamp; `None` means the task is not
    /// deferred.
    pub deferred_until: Option<OffsetDateTime>,
    /// Dispatch-gate snapshot used by `defer_forever`:
    /// whether the image for the task is ready.
    pub image_ready: bool,
    /// Whether the task has unresolved blockers.
    pub no_blockers: bool,
    /// Whether there is free capacity for the task's role/model.
    pub capacity_free: bool,
}

/// Read-only data source for the disposition checks.
///
/// The checks take an `Arc<dyn DispositionDb>` so that tests can supply an
/// in-memory implementation without opening a real database. A future adapter
/// over `djinn-db` can implement this trait for live use.
pub trait DispositionDb: Send + Sync {
    /// Return all task rows relevant to the disposition checks. Each check
    /// filters the rows itself.
    fn disposition_candidates(&self) -> Vec<TaskDispositionRow>;
}

// ---------------------------------------------------------------------------
// Disposition orphan check
// ---------------------------------------------------------------------------

/// Check name constant.
pub const DISPOSITION_ORPHAN_NAME: &str = "disposition_orphan";

/// Flags any `task` row in `in_progress` or `pr_review` status where there is
/// no live session, no queued/remembered dispatch, and no open PR.
///
/// Severity: **Critical** — the task is effectively stuck with no mechanism
/// advancing it toward completion.
pub struct DispositionOrphanCheck {
    db: Arc<dyn DispositionDb>,
}

impl DispositionOrphanCheck {
    /// Construct the check with its read-only data source.
    pub fn new(db: Arc<dyn DispositionDb>) -> Self {
        Self { db }
    }
}

impl DoctorCheck for DispositionOrphanCheck {
    fn name(&self) -> &'static str {
        DISPOSITION_ORPHAN_NAME
    }

    fn description(&self) -> &'static str {
        "Flags tasks in_progress/pr_review with no running session, no \
         inflight dispatch, and no open PR — the deferred-forever class"
    }

    fn run(&self) -> DoctorResult<Vec<Finding>> {
        let rows = self.db.disposition_candidates();
        let mut findings = Vec::new();
        for row in &rows {
            if let Some(finding) = resolve_orphan(row) {
                findings.push(finding);
            }
        }
        Ok(findings)
    }
}

/// Resolver for [`DispositionOrphanCheck`].
///
/// Returns `Some(Finding)` when the task is an orphan: its status is
/// `in_progress` or `pr_review` AND it has no running session, no inflight
/// dispatch, and no open PR. Returns `None` otherwise (healthy or
/// non-applicable task).
fn resolve_orphan(row: &TaskDispositionRow) -> Option<Finding> {
    let active = matches!(row.status.as_str(), "in_progress" | "pr_review");
    if !active {
        return None;
    }

    let is_orphan = !row.has_running_session && !row.has_inflight_dispatch && !row.has_open_pr;

    let inputs = serde_json::json!({
        "task_id": row.task_id,
        "status": row.status,
        "has_running_session": row.has_running_session,
        "has_inflight_dispatch": row.has_inflight_dispatch,
        "has_open_pr": row.has_open_pr,
    });

    let outputs = serde_json::json!({
        "is_orphan": is_orphan,
    });

    if !is_orphan {
        return None;
    }

    let finding = Finding::new(
        FindingSeverity::Critical,
        DISPOSITION_ORPHAN_NAME,
        ResolverSnapshot::new("resolve_orphan", inputs.clone(), outputs.clone()),
        format!(
            "task {} in status '{}' has no running session, no inflight \
             dispatch, and no open PR — it is an orphan with nothing \
             advancing it",
            row.task_id, row.status
        ),
    )
    .with_entity_id("task_id", &row.task_id)
    .with_evidence(serde_json::json!({
        "status": row.status,
        "has_running_session": row.has_running_session,
        "has_inflight_dispatch": row.has_inflight_dispatch,
        "has_open_pr": row.has_open_pr,
    }));

    Some(finding)
}

// ---------------------------------------------------------------------------
// Defer-forever check
// ---------------------------------------------------------------------------

/// Check name constant.
pub const DEFER_FOREVER_NAME: &str = "defer_forever";

/// Default threshold: a task deferred more than this many hours past its
/// `deferred_until` is a candidate for the defer-forever finding.
pub const DEFER_FOREVER_DEFAULT_THRESHOLD_HOURS: i64 = 6;

/// Flags any `task` row whose `deferred_until` is in the past by more than N
/// hours (default 6) and whose dispatch gate is currently satisfied (image
/// ready, no unresolved blockers, capacity free).
///
/// Severity: **Warn** — the deferral may be stale; the task *could* be
/// dispatched now, suggesting the deferral was forgotten rather than
/// intentional.
pub struct DeferForeverCheck {
    db: Arc<dyn DispositionDb>,
    threshold_hours: i64,
}

impl DeferForeverCheck {
    /// Construct the check with the default 6-hour threshold.
    pub fn new(db: Arc<dyn DispositionDb>) -> Self {
        Self {
            db,
            threshold_hours: Self::default_threshold_hours(),
        }
    }

    /// Construct the check with a custom threshold (in hours).
    pub fn with_threshold_hours(db: Arc<dyn DispositionDb>, hours: i64) -> Self {
        Self {
            db,
            threshold_hours: hours,
        }
    }

    /// The default deferral threshold in hours (6).
    ///
    /// A future iteration can plumb a config source, but a constant default
    /// is acceptable for Wave 1.
    pub fn default_threshold_hours() -> i64 {
        DEFER_FOREVER_DEFAULT_THRESHOLD_HOURS
    }
}

impl DoctorCheck for DeferForeverCheck {
    fn name(&self) -> &'static str {
        DEFER_FOREVER_NAME
    }

    fn description(&self) -> &'static str {
        "Flags tasks deferred past their deferred_until by more than the \
         threshold whose dispatch gate is satisfied — the defer-forever class"
    }

    fn run(&self) -> DoctorResult<Vec<Finding>> {
        let rows = self.db.disposition_candidates();
        let now = OffsetDateTime::now_utc();
        let mut findings = Vec::new();
        for row in &rows {
            if let Some(finding) = resolve_defer_forever(row, now, self.threshold_hours) {
                findings.push(finding);
            }
        }
        Ok(findings)
    }
}

/// Resolver for [`DeferForeverCheck`].
///
/// Returns `Some(Finding)` when the task has a `deferred_until` that is in the
/// past by more than `threshold_hours` AND the dispatch gate is satisfied
/// (image ready, no blockers, capacity free). Returns `None` otherwise —
/// including when the gate is unsatisfied (the deferral is justified).
fn resolve_defer_forever(
    row: &TaskDispositionRow,
    now: OffsetDateTime,
    threshold_hours: i64,
) -> Option<Finding> {
    let deferred_until = row.deferred_until?;
    if deferred_until > now {
        // Still in the future — deferral is active, not stale.
        return None;
    }

    let elapsed = now - deferred_until;
    let elapsed_hours = elapsed.whole_hours();

    let gate_satisfied = row.image_ready && row.no_blockers && row.capacity_free;
    let past_threshold = elapsed_hours > threshold_hours;

    let inputs = serde_json::json!({
        "task_id": row.task_id,
        "deferred_until": iso_format(deferred_until),
        "now": iso_format(now),
        "deferred_for_hours": elapsed_hours,
        "threshold_hours": threshold_hours,
        "image_ready": row.image_ready,
        "no_blockers": row.no_blockers,
        "capacity_free": row.capacity_free,
        "gate_satisfied": gate_satisfied,
    });

    let outputs = serde_json::json!({
        "past_threshold": past_threshold,
        "gate_satisfied": gate_satisfied,
        "should_flag": past_threshold && gate_satisfied,
    });

    if !(past_threshold && gate_satisfied) {
        return None;
    }

    let finding = Finding::new(
        FindingSeverity::Warn,
        DEFER_FOREVER_NAME,
        ResolverSnapshot::new("resolve_defer_forever", inputs.clone(), outputs.clone()),
        format!(
            "task {} has been deferred for {}h (threshold {}h) but its \
             dispatch gate is satisfied (image ready, no blockers, capacity \
             free) — the deferral may be stale",
            row.task_id, elapsed_hours, threshold_hours,
        ),
    )
    .with_entity_id("task_id", &row.task_id)
    .with_evidence(serde_json::json!({
        "deferred_for_hours": elapsed_hours,
        "threshold_hours": threshold_hours,
        "image_ready": row.image_ready,
        "no_blockers": row.no_blockers,
        "capacity_free": row.capacity_free,
        "gate_satisfied": gate_satisfied,
    }));

    Some(finding)
}

/// Format an [`OffsetDateTime`] as an ISO-8601 string for JSON snapshots.
fn iso_format(ts: OffsetDateTime) -> String {
    ts.format(&time::format_description::well_known::Iso8601::DEFAULT)
        .unwrap_or_else(|_| ts.unix_timestamp().to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // In-memory test double
    // -----------------------------------------------------------------

    /// Simple in-memory implementation of [`DispositionDb`] for fabrication
    /// tests. No live database is touched.
    #[derive(Default)]
    struct MemoryDb {
        rows: Vec<TaskDispositionRow>,
    }

    impl DispositionDb for MemoryDb {
        fn disposition_candidates(&self) -> Vec<TaskDispositionRow> {
            self.rows.clone()
        }
    }

    /// Helper to build a minimal row with sensible defaults.
    fn make_row(task_id: &str) -> TaskDispositionRow {
        TaskDispositionRow {
            task_id: task_id.to_string(),
            status: "in_progress".to_string(),
            has_running_session: false,
            has_inflight_dispatch: false,
            has_open_pr: false,
            deferred_until: None,
            image_ready: false,
            no_blockers: false,
            capacity_free: false,
        }
    }

    fn arc_db(rows: Vec<TaskDispositionRow>) -> Arc<dyn DispositionDb> {
        Arc::new(MemoryDb { rows })
    }

    // -----------------------------------------------------------------
    // DispositionOrphanCheck tests
    // -----------------------------------------------------------------

    #[test]
    fn orphan_finding_for_divergent_in_progress_task() {
        let row = make_row("task-orphan-1");
        let db = arc_db(vec![row]);
        let check = DispositionOrphanCheck::new(db);

        let findings = check.run().expect("run should succeed");
        assert_eq!(findings.len(), 1, "should produce exactly one finding");

        let f = &findings[0];
        assert_eq!(f.severity, FindingSeverity::Critical);
        assert_eq!(f.check_name, DISPOSITION_ORPHAN_NAME);

        // entity_ids contains the divergent task id.
        assert_eq!(
            f.entity_ids.get("task_id").map(String::as_str),
            Some("task-orphan-1")
        );

        // Evidence contains the three missing-evidence booleans.
        assert_eq!(f.evidence["has_running_session"], false);
        assert_eq!(f.evidence["has_inflight_dispatch"], false);
        assert_eq!(f.evidence["has_open_pr"], false);
        assert_eq!(f.evidence["status"], "in_progress");

        // Resolver snapshot is populated with the inputs.
        let snap = &f.resolver_snapshot;
        assert_eq!(snap.resolver, "resolve_orphan");
        assert_eq!(snap.inputs["task_id"], "task-orphan-1");
        assert_eq!(snap.inputs["status"], "in_progress");
        assert_eq!(snap.inputs["has_running_session"], false);
        assert_eq!(snap.inputs["has_inflight_dispatch"], false);
        assert_eq!(snap.inputs["has_open_pr"], false);

        // The resolver outputs reproduce from the inputs (shared-resolver
        // invariant).
        let replayed = resolve_orphan(&make_row("task-orphan-1"));
        assert!(replayed.is_some(), "resolver should still flag the orphan");
    }

    #[test]
    fn orphan_finding_for_pr_review_status() {
        let mut row = make_row("task-pr-review-orphan");
        row.status = "pr_review".to_string();
        let db = arc_db(vec![row]);
        let check = DispositionOrphanCheck::new(db);

        let findings = check.run().expect("run should succeed");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].evidence["status"], "pr_review");
    }

    #[test]
    fn orphan_no_finding_when_task_has_running_session() {
        let mut row = make_row("task-healthy");
        row.has_running_session = true;
        let db = arc_db(vec![row]);
        let check = DispositionOrphanCheck::new(db);

        let findings = check.run().expect("run should succeed");
        assert!(
            findings.is_empty(),
            "task with a running session is healthy"
        );
    }

    #[test]
    fn orphan_no_finding_when_task_has_inflight_dispatch() {
        let mut row = make_row("task-dispatched");
        row.has_inflight_dispatch = true;
        let db = arc_db(vec![row]);
        let check = DispositionOrphanCheck::new(db);

        let findings = check.run().expect("run should succeed");
        assert!(
            findings.is_empty(),
            "task with inflight dispatch is not an orphan"
        );
    }

    #[test]
    fn orphan_no_finding_when_task_has_open_pr() {
        let mut row = make_row("task-has-pr");
        row.has_open_pr = true;
        let db = arc_db(vec![row]);
        let check = DispositionOrphanCheck::new(db);

        let findings = check.run().expect("run should succeed");
        assert!(findings.is_empty(), "task with an open PR is not an orphan");
    }

    #[test]
    fn orphan_no_finding_for_non_active_status() {
        let mut row = make_row("task-closed");
        row.status = "closed".to_string();
        let db = arc_db(vec![row]);
        let check = DispositionOrphanCheck::new(db);

        let findings = check.run().expect("run should succeed");
        assert!(findings.is_empty(), "closed task is not applicable");
    }

    #[test]
    fn orphan_no_finding_for_empty_input() {
        let db = arc_db(vec![]);
        let check = DispositionOrphanCheck::new(db);

        let findings = check.run().expect("run should succeed");
        assert!(findings.is_empty());
    }

    // -----------------------------------------------------------------
    // DeferForeverCheck tests
    // -----------------------------------------------------------------

    #[test]
    fn defer_forever_finding_for_stale_deferred_task() {
        let now = OffsetDateTime::now_utc();
        let deferred_until = now - time::Duration::hours(7);

        let mut row = make_row("task-deferred-7h");
        row.deferred_until = Some(deferred_until);
        row.image_ready = true;
        row.no_blockers = true;
        row.capacity_free = true;

        let db = arc_db(vec![row]);
        let check = DeferForeverCheck::new(db);

        let findings = check.run().expect("run should succeed");
        assert_eq!(findings.len(), 1, "should produce exactly one finding");

        let f = &findings[0];
        assert_eq!(f.severity, FindingSeverity::Warn);
        assert_eq!(f.check_name, DEFER_FOREVER_NAME);
        assert_eq!(
            f.entity_ids.get("task_id").map(String::as_str),
            Some("task-deferred-7h")
        );

        // Evidence contains the gate conditions.
        assert_eq!(f.evidence["deferred_for_hours"], 7);
        assert_eq!(f.evidence["threshold_hours"], 6);
        assert_eq!(f.evidence["image_ready"], true);
        assert_eq!(f.evidence["no_blockers"], true);
        assert_eq!(f.evidence["capacity_free"], true);
        assert_eq!(f.evidence["gate_satisfied"], true);

        // Resolver snapshot inputs contain the relevant fields.
        let snap = &f.resolver_snapshot;
        assert_eq!(snap.resolver, "resolve_defer_forever");
        assert_eq!(snap.inputs["task_id"], "task-deferred-7h");
        assert_eq!(snap.inputs["deferred_for_hours"], 7);
        assert_eq!(snap.inputs["threshold_hours"], 6);
        assert_eq!(snap.inputs["image_ready"], true);
        assert_eq!(snap.inputs["no_blockers"], true);
        assert_eq!(snap.inputs["capacity_free"], true);
        assert_eq!(snap.inputs["gate_satisfied"], true);
    }

    #[test]
    fn defer_forever_no_finding_when_gate_unsatisfied_capacity_busy() {
        let now = OffsetDateTime::now_utc();
        let deferred_until = now - time::Duration::hours(7);

        let mut row = make_row("task-capacity-busy");
        row.deferred_until = Some(deferred_until);
        row.image_ready = true;
        row.no_blockers = true;
        row.capacity_free = false; // capacity busy → gate unsatisfied

        let db = arc_db(vec![row]);
        let check = DeferForeverCheck::new(db);

        let findings = check.run().expect("run should succeed");
        assert!(
            findings.is_empty(),
            "deferral is justified when gate is unsatisfied"
        );
    }

    #[test]
    fn defer_forever_no_finding_when_gate_unsatisfied_image_not_ready() {
        let now = OffsetDateTime::now_utc();
        let deferred_until = now - time::Duration::hours(10);

        let mut row = make_row("task-image-not-ready");
        row.deferred_until = Some(deferred_until);
        row.image_ready = false;
        row.no_blockers = true;
        row.capacity_free = true;

        let db = arc_db(vec![row]);
        let check = DeferForeverCheck::new(db);

        let findings = check.run().expect("run should succeed");
        assert!(findings.is_empty(), "gate unsatisfied: image not ready");
    }

    #[test]
    fn defer_forever_no_finding_when_gate_unsatisfied_has_blockers() {
        let now = OffsetDateTime::now_utc();
        let deferred_until = now - time::Duration::hours(10);

        let mut row = make_row("task-has-blockers");
        row.deferred_until = Some(deferred_until);
        row.image_ready = true;
        row.no_blockers = false;
        row.capacity_free = true;

        let db = arc_db(vec![row]);
        let check = DeferForeverCheck::new(db);

        let findings = check.run().expect("run should succeed");
        assert!(findings.is_empty(), "gate unsatisfied: has blockers");
    }

    #[test]
    fn defer_forever_threshold_boundary_just_under_no_finding() {
        let now = OffsetDateTime::now_utc();
        // 5 hours — just under the 6-hour threshold.
        let deferred_until = now - time::Duration::hours(5);

        let mut row = make_row("task-just-under");
        row.deferred_until = Some(deferred_until);
        row.image_ready = true;
        row.no_blockers = true;
        row.capacity_free = true;

        let db = arc_db(vec![row]);
        let check = DeferForeverCheck::new(db);

        let findings = check.run().expect("run should succeed");
        assert!(findings.is_empty(), "just under threshold → no finding");
    }

    #[test]
    fn defer_forever_threshold_boundary_just_over_finding() {
        let now = OffsetDateTime::now_utc();
        // 7 hours — just over the 6-hour threshold.
        let deferred_until = now - time::Duration::hours(7);

        let mut row = make_row("task-just-over");
        row.deferred_until = Some(deferred_until);
        row.image_ready = true;
        row.no_blockers = true;
        row.capacity_free = true;

        let db = arc_db(vec![row]);
        let check = DeferForeverCheck::new(db);

        let findings = check.run().expect("run should succeed");
        assert_eq!(findings.len(), 1, "just over threshold → finding");
    }

    #[test]
    fn defer_forever_custom_threshold_boundary() {
        let now = OffsetDateTime::now_utc();
        let deferred_until = now - time::Duration::hours(3);

        let mut row = make_row("task-custom-threshold");
        row.deferred_until = Some(deferred_until);
        row.image_ready = true;
        row.no_blockers = true;
        row.capacity_free = true;

        // Custom threshold of 2 hours: 3h past > 2h threshold → finding.
        let db = arc_db(vec![row]);
        let check = DeferForeverCheck::with_threshold_hours(db, 2);

        let findings = check.run().expect("run should succeed");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].evidence["threshold_hours"], 2);
        assert_eq!(findings[0].evidence["deferred_for_hours"], 3);
    }

    #[test]
    fn defer_forever_no_finding_when_not_deferred() {
        let row = make_row("task-not-deferred");
        let db = arc_db(vec![row]);
        let check = DeferForeverCheck::new(db);

        let findings = check.run().expect("run should succeed");
        assert!(findings.is_empty(), "non-deferred task is not applicable");
    }

    #[test]
    fn defer_forever_no_finding_when_deferred_until_in_future() {
        let now = OffsetDateTime::now_utc();
        let deferred_until = now + time::Duration::hours(2);

        let mut row = make_row("task-future-deferred");
        row.deferred_until = Some(deferred_until);
        row.image_ready = true;
        row.no_blockers = true;
        row.capacity_free = true;

        let db = arc_db(vec![row]);
        let check = DeferForeverCheck::new(db);

        let findings = check.run().expect("run should succeed");
        assert!(findings.is_empty(), "future deferral is active, not stale");
    }

    #[test]
    fn defer_forever_no_finding_for_empty_input() {
        let db = arc_db(vec![]);
        let check = DeferForeverCheck::new(db);

        let findings = check.run().expect("run should succeed");
        assert!(findings.is_empty());
    }

    // -----------------------------------------------------------------
    // Shared-resolver invariant tests
    // -----------------------------------------------------------------

    #[test]
    fn orphan_resolver_snapshot_reproduces_outputs() {
        let row = make_row("task-replay");
        let db = arc_db(vec![row.clone()]);
        let check = DispositionOrphanCheck::new(db);

        let findings = check.run().expect("run should succeed");
        let f = &findings[0];

        // The snapshot's inputs must reproduce the finding when fed back
        // through resolve_orphan.
        let snap = &f.resolver_snapshot;
        let replay_inputs = &snap.inputs;

        // Reconstruct a row from the snapshot inputs and confirm
        // resolve_orphan agrees.
        let replayed_row = TaskDispositionRow {
            task_id: replay_inputs["task_id"].as_str().unwrap().to_string(),
            status: replay_inputs["status"].as_str().unwrap().to_string(),
            has_running_session: replay_inputs["has_running_session"].as_bool().unwrap(),
            has_inflight_dispatch: replay_inputs["has_inflight_dispatch"].as_bool().unwrap(),
            has_open_pr: replay_inputs["has_open_pr"].as_bool().unwrap(),
            deferred_until: None,
            image_ready: false,
            no_blockers: false,
            capacity_free: false,
        };
        let replayed = resolve_orphan(&replayed_row);
        assert!(
            replayed.is_some(),
            "resolver replayed from snapshot inputs must still flag the orphan"
        );
        let replayed_f = replayed.unwrap();
        assert_eq!(replayed_f.severity, FindingSeverity::Critical);
        assert_eq!(
            replayed_f.entity_ids.get("task_id").map(String::as_str),
            Some("task-replay")
        );
    }

    #[test]
    fn defer_forever_default_threshold_is_six_hours() {
        assert_eq!(
            DeferForeverCheck::default_threshold_hours(),
            DEFER_FOREVER_DEFAULT_THRESHOLD_HOURS
        );
        assert_eq!(DEFER_FOREVER_DEFAULT_THRESHOLD_HOURS, 6);
    }

    #[test]
    fn defer_forever_resolver_snapshot_reproduces_outputs() {
        let now = OffsetDateTime::now_utc();
        let deferred_until = now - time::Duration::hours(8);

        let row = {
            let mut r = make_row("task-replay-defer");
            r.deferred_until = Some(deferred_until);
            r.image_ready = true;
            r.no_blockers = true;
            r.capacity_free = true;
            r
        };

        let db = arc_db(vec![row]);
        let check = DeferForeverCheck::new(db);

        let findings = check.run().expect("run should succeed");
        let f = &findings[0];

        // The snapshot's inputs must reproduce the finding.
        let snap = &f.resolver_snapshot;
        let replay_threshold = snap.inputs["threshold_hours"].as_i64().unwrap();
        let replay_hours = snap.inputs["deferred_for_hours"].as_i64().unwrap();
        let replay_gate = snap.inputs["gate_satisfied"].as_bool().unwrap();

        // Assert the snapshot is internally consistent.
        assert!(replay_hours > replay_threshold);
        assert!(replay_gate);

        // Outputs reflect the decision.
        assert_eq!(snap.outputs["should_flag"], true);
        assert_eq!(snap.outputs["gate_satisfied"], true);
        assert_eq!(snap.outputs["past_threshold"], true);
    }

    // -----------------------------------------------------------------
    // Multi-row and integration sanity
    // -----------------------------------------------------------------

    #[test]
    fn orphan_multiple_findings() {
        let rows = vec![make_row("task-a"), make_row("task-b")];
        let db = arc_db(rows);
        let check = DispositionOrphanCheck::new(db);

        let findings = check.run().expect("run should succeed");
        assert_eq!(findings.len(), 2);
        let ids: Vec<&str> = findings
            .iter()
            .map(|f| f.entity_ids.get("task_id").unwrap().as_str())
            .collect();
        assert!(ids.contains(&"task-a"));
        assert!(ids.contains(&"task-b"));
    }

    #[test]
    fn mixed_rows_orphan_and_healthy() {
        let rows = vec![make_row("orphan-1"), {
            let mut r = make_row("healthy-1");
            r.has_running_session = true;
            r
        }];
        let db = arc_db(rows);
        let check = DispositionOrphanCheck::new(db);

        let findings = check.run().expect("run should succeed");
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].entity_ids.get("task_id").map(String::as_str),
            Some("orphan-1")
        );
    }

    #[test]
    fn both_checks_on_same_db() {
        let now = OffsetDateTime::now_utc();
        let orphan_row = make_row("orphan-only");
        let deferred_row = {
            let mut r = make_row("deferred-only");
            r.status = "deferred".to_string();
            r.deferred_until = Some(now - time::Duration::hours(8));
            r.image_ready = true;
            r.no_blockers = true;
            r.capacity_free = true;
            r
        };

        let db = arc_db(vec![orphan_row, deferred_row]);

        let orphan_findings = DispositionOrphanCheck::new(Arc::clone(&db))
            .run()
            .expect("orphan run");
        // Only the orphan task has active status; the deferred task has
        // status "deferred" which is not in {in_progress, pr_review}.
        assert_eq!(orphan_findings.len(), 1);

        let defer_findings = DeferForeverCheck::new(db).run().expect("defer run");
        assert_eq!(defer_findings.len(), 1);
    }
}
