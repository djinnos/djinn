//! `zombie_running_session` doctor check.
//!
//! A "zombie" session is a row in the `sessions` table that says it is
//! `running` but has no live pod, no slot entry, and the worker RPC
//! registry reports the corresponding `task_run` as disconnected. The
//! existing DB-truth zombie reaper in
//! `djinn-agent::actors::coordinator::dispatch::session_recovery::reap_zombie_sessions`
//! *acts* on these rows; this check *detects* them so the doctor surface
//! can surface the wedge before the reaper's hard cap (or, more
//! importantly, when the reaper is briefly disabled / failing open).
//!
//! Per the doctor design, the check is a *detector* — it never mutates
//! state. The framework's `fix()` is left as the default
//! `Err(FixNotSupported)` because per-check fixers are out of scope for
//! the seed-check wave.

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::doctor::{DoctorCheck, DoctorResult, Finding, FindingSeverity, ResolverSnapshot};

/// A read-only projection of the inputs the zombie-session check needs.
///
/// The check takes a `&dyn CheckDb` so the fabrication tests can use a
/// pure in-memory double — the check itself never opens a real database.
/// A future adapter (in a follow-up epic) will provide an impl backed by
/// `djinn_db` + the worker RPC registry.
pub trait CheckDb {
    /// Sessions whose `status` is `running`. The check filters further
    /// (e.g. skipping `agent_type == "chat"`) — the trait intentionally
    /// returns the raw active list so the resolver can decide.
    fn zombie_running_sessions(&self) -> Vec<SessionRow>;

    /// Slot-pool entries. The check uses this to know whether the
    /// candidate session has *any* live slot entry. An empty `Vec` (or
    /// a missing key) is treated as "no slot present" — the same wedge
    /// the reaper reacts to.
    fn slot_entries(&self) -> Vec<SlotRow>;

    /// `true` iff the worker RPC registry currently reports a live
    /// connection for the given `task_run_id`. A real impl bridges to
    /// the supervisor's connection registry; the fabrication test
    /// returns the value it has staged in its in-memory state.
    ///
    /// `None` (no `task_run_id` on the row) is treated as "not
    /// connected" — the reaper itself cannot gate on a missing key.
    fn is_worker_connected(&self, task_run_id: Option<&str>) -> bool;

    /// `true` iff a live pod is currently reported for `task_id` (via
    /// the k8s client or equivalent). A real impl bridges to the k8s
    /// client; the fabrication test returns its staged value.
    fn pod_present(&self, task_id: &str) -> bool;
}

/// A minimal projection of a `sessions` row the zombie check consumes.
///
/// Field-for-field this matches the columns the reaper reads off
/// `SessionRepository::list_active`; we keep a private struct so the
/// check is testable without `djinn_db` as a build-time dep of
/// `djinn-core`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRow {
    pub id: String,
    pub task_id: Option<String>,
    pub agent_type: String,
    pub started_at: String,
    pub tokens_in: i64,
    pub tokens_out: i64,
    /// Optional `task_runs.id` mirror — used as the lookup key into the
    /// RPC registry to determine `is_connected`. Mirrors
    /// `SessionRecord::task_run_id` from `djinn_core::models::session`.
    pub task_run_id: Option<String>,
}

/// A minimal projection of a slot-pool row. The full pool state is an
/// in-memory actor in `djinn_agent`; the doctor only needs enough to
/// detect "this session has no live slot".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotRow {
    pub slot_id: String,
    pub model_id: String,
    pub user_id: String,
    /// One of `"free"`, `"busy"`, `"draining"`. The check normalizes
    /// case on read; values are stored lowercase to match the in-memory
    /// pool's tag.
    pub state: String,
    /// Required when `state == "busy"`. The check uses this to look up
    /// the corresponding `task_runs` row and confirm the slot's
    /// `busy_for_task` is still live.
    pub busy_for_task: Option<String>,
}

/// Inputs the resolver consumes for one candidate session.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ZombieSessionInputs {
    pub candidate_session_id: String,
    pub candidate_task_id: Option<String>,
    pub candidate_task_run_id: Option<String>,
    pub is_connected: bool,
    pub pod_present: bool,
    pub slot_present: bool,
    pub agent_type: String,
    pub started_at: String,
    pub tokens_in: i64,
    pub tokens_out: i64,
}

/// Outputs the resolver returns. The fields are the *observed* truth
/// the fix path will replay `resolve()` against.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ZombieSessionOutputs {
    pub is_zombie: bool,
    pub reason: ZombieReason,
}

/// Why the resolver concluded the session is or is not a zombie.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ZombieReason {
    /// The row matches every wedge the reaper reacts to: `running`, no
    /// live pod, no slot, and `is_connected = false`.
    Zombie,
    /// `agent_type == "chat"`. The chat reaper owns this row, not
    /// this check.
    ChatSession,
    /// Session has no `task_id` — the reaper skips these because
    /// they cannot be redispatched.
    NoTaskId,
    /// At least one of the liveness signals is positive, so the row
    /// is not a zombie (it is "healthy" from the doctor's view).
    Healthy,
}

/// The shared resolver. Both `run()` and the (future) `fix()` call this
/// so the snapshot's `inputs` can reproduce the snapshot's `outputs`
/// exactly — the shared-resolver invariant from the doctor framework
/// module docs.
fn resolve_state(inputs: &ZombieSessionInputs) -> ZombieSessionOutputs {
    if inputs.agent_type == "chat" {
        return ZombieSessionOutputs {
            is_zombie: false,
            reason: ZombieReason::ChatSession,
        };
    }
    if inputs.candidate_task_id.is_none() {
        return ZombieSessionOutputs {
            is_zombie: false,
            reason: ZombieReason::NoTaskId,
        };
    }

    // Per the task description: a `running` session is a zombie iff
    // (a) there is no live pod, (b) there is no slot entry, and
    // (c) the worker RPC registry reports it as disconnected. The
    // reaper adds a time-based hard cap on top, but that is a
    // *reaper* concern (when to act) — the *detector* is purely
    // structural: "is this row's liveness in conflict with its row
    // state?".
    let is_zombie = !inputs.pod_present && !inputs.slot_present && !inputs.is_connected;
    ZombieSessionOutputs {
        is_zombie,
        reason: if is_zombie {
            ZombieReason::Zombie
        } else {
            ZombieReason::Healthy
        },
    }
}

/// `DoctorCheck` impl that flags any `running` session whose liveness
/// state is empty (no pod, no slot, not connected).
///
/// The check is read-only. It does not call any `fix()`-shaped function
/// and does not import `supervisor_impl::pr`; it mirrors the reaper's
/// *detection* gate, not its action set.
pub struct ZombieRunningSessionCheck<D: CheckDb> {
    db: D,
}

impl<D: CheckDb> ZombieRunningSessionCheck<D> {
    /// Construct a check bound to a specific `CheckDb` projection. In
    /// production this will be backed by a thin adapter over
    /// `djinn_db::SessionRepository` + the worker RPC registry; in
    /// tests it is backed by `MemoryCheckDb`.
    pub fn new(db: D) -> Self {
        Self { db }
    }

    /// Resolve one candidate session into a [`Finding`], if it is a
    /// zombie. Kept private so the snapshot's `inputs`/`outputs` fields
    /// are guaranteed to come from the *same* `resolve_state()` call the
    /// checker used.
    fn resolve(inputs: &ZombieSessionInputs) -> Option<Finding> {
        let outputs = resolve_state(inputs);
        if !outputs.is_zombie {
            return None;
        }

        let resolver_inputs_json =
            serde_json::to_value(inputs).expect("ZombieSessionInputs serializes");
        let resolver_outputs_json =
            serde_json::to_value(&outputs).expect("ZombieSessionOutputs serializes");
        let snapshot = ResolverSnapshot::new(
            "resolve_zombie_session",
            resolver_inputs_json.clone(),
            resolver_outputs_json,
        );

        let evidence = json!({
            "session_id": inputs.candidate_session_id,
            "task_id": inputs.candidate_task_id,
            "task_run_id": inputs.candidate_task_run_id,
            "agent_type": inputs.agent_type,
            "started_at": inputs.started_at,
            "tokens_in": inputs.tokens_in,
            "tokens_out": inputs.tokens_out,
            "is_connected": inputs.is_connected,
            "pod_present": inputs.pod_present,
            "slot_present": inputs.slot_present,
        });

        let detail = format!(
            "session {} for task {} is `running` with no live pod, no slot \
             entry, and is_connected=false (started {}, tokens in/out {}/{}); \
             the DB-truth zombie reaper would reap this on its next pass",
            inputs.candidate_session_id,
            inputs.candidate_task_id.as_deref().unwrap_or("?"),
            inputs.started_at,
            inputs.tokens_in,
            inputs.tokens_out,
        );

        let mut finding = Finding::new(
            FindingSeverity::Critical,
            "zombie_running_session",
            snapshot,
            detail,
        );
        finding = finding
            .with_entity_id("session_id", inputs.candidate_session_id.clone())
            .with_evidence(evidence);
        if let Some(task_id) = inputs.candidate_task_id.as_deref() {
            finding = finding.with_entity_id("task_id", task_id.to_owned());
        }
        if let Some(task_run_id) = inputs.candidate_task_run_id.as_deref() {
            finding = finding.with_entity_id("task_run_id", task_run_id.to_owned());
        }
        Some(finding)
    }
}

impl<D: CheckDb + Send + Sync> DoctorCheck for ZombieRunningSessionCheck<D> {
    fn name(&self) -> &'static str {
        "zombie_running_session"
    }

    fn description(&self) -> &'static str {
        "Flags sessions with status=running that have no live pod, no slot \
         entry, and no live RPC connection — the same wedge the DB-truth \
         zombie reaper in session_recovery reacts to, surfaced as a \
         detector (no state mutation)."
    }

    fn run(&self) -> DoctorResult<Vec<Finding>> {
        let active = self.db.zombie_running_sessions();
        let mut findings = Vec::new();
        for session in active {
            let Some(task_id) = session.task_id.as_deref() else {
                continue;
            };
            let slot_present = self
                .db
                .slot_entries()
                .iter()
                .any(|s| s.busy_for_task.as_deref() == Some(task_id));
            let is_connected = self.db.is_worker_connected(session.task_run_id.as_deref());
            let pod_present = self.db.pod_present(task_id);

            let inputs = ZombieSessionInputs {
                candidate_session_id: session.id.clone(),
                candidate_task_id: session.task_id.clone(),
                candidate_task_run_id: session.task_run_id.clone(),
                is_connected,
                pod_present,
                slot_present,
                agent_type: session.agent_type.clone(),
                started_at: session.started_at.clone(),
                tokens_in: session.tokens_in,
                tokens_out: session.tokens_out,
            };
            if let Some(finding) = Self::resolve(&inputs) {
                findings.push(finding);
            }
        }
        Ok(findings)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// In-memory `CheckDb` test double. The fabrication tests use it
    /// to stage specific divergence patterns and assert the check
    /// returns the expected finding shape.
    #[derive(Default)]
    struct MemoryCheckDb {
        sessions: Vec<SessionRow>,
        slots: Vec<SlotRow>,
        /// `task_run_id -> is_connected` overrides. Missing entries
        /// default to `false` (the divergent case).
        connected: BTreeMap<String, bool>,
        /// `task_id -> pod_present` overrides. Missing entries default
        /// to `false`.
        pods: BTreeMap<String, bool>,
    }

    impl MemoryCheckDb {
        fn with_zombie_session() -> Self {
            let mut db = Self::default();
            db.sessions.push(SessionRow {
                id: "sess-zombie".to_owned(),
                task_id: Some("task-1".to_owned()),
                agent_type: "worker".to_owned(),
                started_at: "2026-01-02T03:04:05.000Z".to_owned(),
                tokens_in: 0,
                tokens_out: 0,
                task_run_id: Some("run-1".to_owned()),
            });
            db
        }

        fn with_connected_worker() -> Self {
            let mut db = Self::default();
            db.sessions.push(SessionRow {
                id: "sess-alive".to_owned(),
                task_id: Some("task-1".to_owned()),
                agent_type: "worker".to_owned(),
                started_at: "2026-01-02T03:04:05.000Z".to_owned(),
                tokens_in: 12,
                tokens_out: 34,
                task_run_id: Some("run-1".to_owned()),
            });
            db.connected.insert("run-1".to_owned(), true);
            db.pods.insert("task-1".to_owned(), true);
            db
        }

        fn with_slot() -> Self {
            let mut db = Self::default();
            db.sessions.push(SessionRow {
                id: "sess-with-slot".to_owned(),
                task_id: Some("task-1".to_owned()),
                agent_type: "worker".to_owned(),
                started_at: "2026-01-02T03:04:05.000Z".to_owned(),
                tokens_in: 12,
                tokens_out: 34,
                task_run_id: Some("run-1".to_owned()),
            });
            db.slots.push(SlotRow {
                slot_id: "slot-1".to_owned(),
                model_id: "model-a".to_owned(),
                user_id: "user-1".to_owned(),
                state: "busy".to_owned(),
                busy_for_task: Some("task-1".to_owned()),
            });
            db
        }

        fn with_chat_session() -> Self {
            let mut db = Self::default();
            db.sessions.push(SessionRow {
                id: "sess-chat".to_owned(),
                task_id: Some("task-chat".to_owned()),
                agent_type: "chat".to_owned(),
                started_at: "2026-01-02T03:04:05.000Z".to_owned(),
                tokens_in: 0,
                tokens_out: 0,
                task_run_id: Some("run-chat".to_owned()),
            });
            db
        }

        fn with_session_without_task_id() -> Self {
            let mut db = Self::default();
            db.sessions.push(SessionRow {
                id: "sess-orphan".to_owned(),
                task_id: None,
                agent_type: "worker".to_owned(),
                started_at: "2026-01-02T03:04:05.000Z".to_owned(),
                tokens_in: 0,
                tokens_out: 0,
                task_run_id: None,
            });
            db
        }
    }

    impl CheckDb for MemoryCheckDb {
        fn zombie_running_sessions(&self) -> Vec<SessionRow> {
            self.sessions.clone()
        }
        fn slot_entries(&self) -> Vec<SlotRow> {
            self.slots.clone()
        }
        fn is_worker_connected(&self, task_run_id: Option<&str>) -> bool {
            task_run_id
                .and_then(|id| self.connected.get(id).copied())
                .unwrap_or(false)
        }
        fn pod_present(&self, task_id: &str) -> bool {
            self.pods.get(task_id).copied().unwrap_or(false)
        }
    }

    fn run_check(db: MemoryCheckDb) -> Vec<Finding> {
        let check = ZombieRunningSessionCheck::new(db);
        check.run().expect("run succeeds")
    }

    // -------------------------------------------------------------------
    // Happy path
    // -------------------------------------------------------------------

    #[test]
    fn happy_path_no_finding() {
        let findings = run_check(MemoryCheckDb::default());
        assert!(
            findings.is_empty(),
            "empty session list must produce no findings, got {:?}",
            findings
        );
    }

    #[test]
    fn happy_path_session_with_live_slot_is_not_zombie() {
        let findings = run_check(MemoryCheckDb::with_slot());
        assert!(
            findings.is_empty(),
            "session with a live slot must not be flagged, got {:?}",
            findings
        );
    }

    #[test]
    fn happy_path_connected_worker_is_not_zombie() {
        let findings = run_check(MemoryCheckDb::with_connected_worker());
        assert!(
            findings.is_empty(),
            "connected worker must not be flagged as zombie, got {:?}",
            findings
        );
    }

    // -------------------------------------------------------------------
    // Divergence (negative test)
    // -------------------------------------------------------------------

    #[test]
    fn divergence_finding_shape() {
        // The canonical zombie: running, no pod, no slot, not
        // connected. Must produce a Critical finding with the
        // session id in entity_ids, the resolver snapshot populated,
        // and the divergent row's id in `inputs`.
        let findings = run_check(MemoryCheckDb::with_zombie_session());
        assert_eq!(findings.len(), 1, "exactly one zombie finding expected");
        let finding = &findings[0];
        assert_eq!(finding.severity, FindingSeverity::Critical);
        assert_eq!(finding.check_name, "zombie_running_session");
        assert_eq!(
            finding.entity_ids.get("session_id").map(String::as_str),
            Some("sess-zombie"),
            "entity_ids must contain the divergent session id"
        );
        assert_eq!(
            finding.entity_ids.get("task_id").map(String::as_str),
            Some("task-1"),
            "entity_ids must contain the task id the zombie was running for"
        );
        // Evidence must surface the reaper-relevant fields.
        assert_eq!(finding.evidence["session_id"], "sess-zombie");
        assert_eq!(finding.evidence["is_connected"], false);
        assert_eq!(finding.evidence["pod_present"], false);
        assert_eq!(finding.evidence["slot_present"], false);

        // Snapshot must be populated and re-runnable: feeding
        // `snapshot.inputs` back into the same resolver reproduces
        // `snapshot.outputs` exactly.
        assert_eq!(finding.resolver_snapshot.resolver, "resolve_zombie_session");
        let snapshot_inputs: ZombieSessionInputs =
            serde_json::from_value(finding.resolver_snapshot.inputs.clone())
                .expect("snapshot inputs deserialize as ZombieSessionInputs");
        let replay_outputs = resolve_state(&snapshot_inputs);
        let replay_outputs_json = serde_json::to_value(&replay_outputs).expect("outputs serialize");
        assert_eq!(
            replay_outputs_json, finding.resolver_snapshot.outputs,
            "resolver snapshot must be reproducible from snapshot.inputs"
        );
        assert_eq!(snapshot_inputs.candidate_session_id, "sess-zombie");
        assert!(!snapshot_inputs.is_connected);
        assert!(!snapshot_inputs.pod_present);
        assert!(!snapshot_inputs.slot_present);
    }

    #[test]
    fn divergence_zombie_with_reaper_relevant_state_emitted() {
        // Explicit negative test: divergent input where the reaper
        // *would* catch it (running AND is_connected = false) — the
        // doctor must still emit a finding (verifies, doesn't
        // replace).
        let findings = run_check(MemoryCheckDb::with_zombie_session());
        assert_eq!(findings.len(), 1);
        let finding = &findings[0];
        let inputs: ZombieSessionInputs =
            serde_json::from_value(finding.resolver_snapshot.inputs.clone()).unwrap();
        assert!(!inputs.is_connected, "is_connected must be false");
        assert!(!inputs.pod_present, "pod_present must be false");
        assert!(!inputs.slot_present, "slot_present must be false");
        assert_eq!(inputs.agent_type, "worker");
    }

    // -------------------------------------------------------------------
    // Filters mirrored from the reaper
    // -------------------------------------------------------------------

    #[test]
    fn divergence_chat_session_skipped() {
        let findings = run_check(MemoryCheckDb::with_chat_session());
        assert!(
            findings.is_empty(),
            "chat session must not be flagged, got {:?}",
            findings
        );
    }

    #[test]
    fn divergence_session_with_no_task_id_skipped() {
        let findings = run_check(MemoryCheckDb::with_session_without_task_id());
        assert!(
            findings.is_empty(),
            "session with no task_id must not be flagged, got {:?}",
            findings
        );
    }

    // -------------------------------------------------------------------
    // Resolver purity / shared-resolver invariant
    // -------------------------------------------------------------------

    #[test]
    fn resolve_is_pure() {
        let inputs = ZombieSessionInputs {
            candidate_session_id: "sess-x".to_owned(),
            candidate_task_id: Some("task-x".to_owned()),
            candidate_task_run_id: Some("run-x".to_owned()),
            is_connected: false,
            pod_present: false,
            slot_present: false,
            agent_type: "worker".to_owned(),
            started_at: "2026-01-02T03:04:05.000Z".to_owned(),
            tokens_in: 0,
            tokens_out: 0,
        };
        let a = resolve_state(&inputs);
        let b = resolve_state(&inputs);
        assert_eq!(a, b);
        assert!(a.is_zombie);
        assert_eq!(a.reason, ZombieReason::Zombie);
    }

    #[test]
    fn resolve_healthy_when_only_one_signal_fires() {
        let mut inputs = ZombieSessionInputs {
            candidate_session_id: "sess-x".to_owned(),
            candidate_task_id: Some("task-x".to_owned()),
            candidate_task_run_id: Some("run-x".to_owned()),
            is_connected: false,
            pod_present: false,
            slot_present: false,
            agent_type: "worker".to_owned(),
            started_at: "2026-01-02T03:04:05.000Z".to_owned(),
            tokens_in: 0,
            tokens_out: 0,
        };
        inputs.is_connected = true;
        let out = resolve_state(&inputs);
        assert!(!out.is_zombie);
        assert_eq!(out.reason, ZombieReason::Healthy);

        inputs.is_connected = false;
        inputs.pod_present = true;
        let out = resolve_state(&inputs);
        assert!(!out.is_zombie);

        inputs.pod_present = false;
        inputs.slot_present = true;
        let out = resolve_state(&inputs);
        assert!(!out.is_zombie);
    }

    #[test]
    fn resolve_skips_chat_and_no_task_id() {
        let mut inputs = ZombieSessionInputs {
            candidate_session_id: "sess-x".to_owned(),
            candidate_task_id: Some("task-x".to_owned()),
            candidate_task_run_id: Some("run-x".to_owned()),
            is_connected: false,
            pod_present: false,
            slot_present: false,
            agent_type: "chat".to_owned(),
            started_at: "2026-01-02T03:04:05.000Z".to_owned(),
            tokens_in: 0,
            tokens_out: 0,
        };
        let out = resolve_state(&inputs);
        assert!(!out.is_zombie);
        assert_eq!(out.reason, ZombieReason::ChatSession);

        inputs.agent_type = "worker".to_owned();
        inputs.candidate_task_id = None;
        let out = resolve_state(&inputs);
        assert!(!out.is_zombie);
        assert_eq!(out.reason, ZombieReason::NoTaskId);
    }

    // -------------------------------------------------------------------
    // Check name / description / default fix
    // -------------------------------------------------------------------

    #[test]
    fn check_name_and_description_are_stable() {
        let check = ZombieRunningSessionCheck::new(MemoryCheckDb::default());
        assert_eq!(check.name(), "zombie_running_session");
        assert!(
            check.description().contains("zombie"),
            "description should mention zombie: got {:?}",
            check.description()
        );
    }

    #[test]
    fn check_does_not_override_fix() {
        // Per the design, T1's checks do not override `fix`; the
        // default `Err(FixNotSupported)` from the framework is
        // intentional. Asserting the trait default keeps that
        // contract explicit.
        let check = ZombieRunningSessionCheck::new(MemoryCheckDb::default());
        let finding = Finding::new(
            FindingSeverity::Critical,
            "zombie_running_session",
            ResolverSnapshot::new("resolve_zombie_session", json!({}), json!({})),
            "synthetic",
        );
        let err = check
            .fix(&finding)
            .expect_err("default fix must return FixNotSupported");
        match err {
            crate::doctor::DoctorError::FixNotSupported { check } => {
                assert_eq!(check, "zombie_running_session");
            }
            other => panic!("expected FixNotSupported, got {other:?}"),
        }
    }
}

// ===========================================================================
// T3 — force_close_orphan_session
// ===========================================================================
//
// `force_close` closes a task but does not evict the running
// session/slot. A session left in `status = 'running'` after its task
// has been `force_close`d is the open item from txr4 in
// `cases/project-runaway-loop-second-strike-and-slot-wedge` — it
// holds a dispatch slot until the zombie reaper finalizes it, and if
// the reaper is briefly disabled / failing open, the slot stays wedged
// indefinitely.
//
// This check is a *detector*: it flags the orphaned session but does
// not close it, evict the slot, or delete any pod (per the epic's
// "doctor verifies, doesn't replace" guardrail). The check is
// additive to T1's `zombie_running_session` content above — T3 does
// not modify any of T1's code.

/// A read-only projection of the inputs the force-close-orphan check
/// needs.
///
/// This is T3's own trait, disjoint from T1's [`CheckDb`] above. The
/// fabrication tests use a pure in-memory double; a future adapter (in
/// a follow-up epic) will provide an impl backed by `djinn_db`.
pub trait ForceCloseCheckDb {
    /// Sessions whose `status` is `running` AND whose owning task has
    /// been `force_close`d. The check relies on the adapter to perform
    /// the join (sessions ↔ tasks by `task_id`, filtering tasks whose
    /// `close_reason == 'force_close'`); the fabrication test stages
    /// the joined rows directly.
    fn force_close_orphan_sessions(&self) -> Vec<ForceCloseOrphanSessionRow>;

    /// `true` iff a live pod is currently reported for `task_id`. A
    /// real impl bridges to the k8s client; the fabrication test
    /// returns its staged value.
    fn pod_present(&self, task_id: &str) -> bool;
}

/// A minimal projection of a `sessions` row left orphaned by a
/// `force_close`. Only the fields the orphan check reads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForceCloseOrphanSessionRow {
    pub session_id: String,
    pub task_id: String,
    /// The session's status. Expected to be `running` (that is the
    /// divergence); carried in the snapshot for the future fix path.
    pub session_status: String,
}

/// Inputs the resolver consumes for one orphaned session.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForceCloseOrphanInputs {
    pub session_id: String,
    pub task_id: String,
    /// The owning task's close reason. Always `force_close` for rows
    /// this check examines, but carried explicitly so the snapshot is
    /// self-describing.
    pub task_close_reason: String,
    pub session_status: String,
    /// Whether a live pod is still reported for the task. `true` means
    /// the orphaned session still has a backing pod consuming cluster
    /// resources.
    pub pod_present: bool,
}

/// Outputs the resolver returns. The fields are the *observed* truth
/// the fix path will replay `resolve()` against.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForceCloseOrphanOutputs {
    pub is_orphan: bool,
    pub reason: ForceCloseOrphanReason,
}

/// Why the resolver concluded the session is or is not an orphan.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ForceCloseOrphanReason {
    /// The session is `running` but its task has been `force_close`d.
    /// The session/slot is orphaned.
    Orphan,
    /// The session is not `running` (it has already been finalized),
    /// or the task's close reason is not `force_close`. No finding.
    Healthy,
}

/// The shared resolver. Both `run()` and the (future) `fix()` call this
/// so the snapshot's `inputs` can reproduce the snapshot's `outputs`
/// exactly — the shared-resolver invariant from the doctor framework
/// module docs.
fn resolve_force_close_state(inputs: &ForceCloseOrphanInputs) -> ForceCloseOrphanOutputs {
    // The session is an orphan iff it is still `running` and its task
    // was `force_close`d. The adapter already filters to
    // `close_reason == 'force_close'`, but the resolver re-checks so
    // the snapshot is a faithful record of the decision.
    let is_orphan = inputs.session_status == "running" && inputs.task_close_reason == "force_close";
    ForceCloseOrphanOutputs {
        is_orphan,
        reason: if is_orphan {
            ForceCloseOrphanReason::Orphan
        } else {
            ForceCloseOrphanReason::Healthy
        },
    }
}

/// `DoctorCheck` impl that flags sessions left in `status = 'running'`
/// after the task they belong to has been `force_close`d.
///
/// The check is read-only. It does not close any session, evict any
/// slot, delete any pod, or import `supervisor_impl::pr` — it mirrors
/// the force-close orphan wedge as a detector (per the epic's "doctor
/// verifies, doesn't replace" guardrail).
pub struct ForceCloseOrphanSessionCheck<D: ForceCloseCheckDb> {
    db: D,
}

impl<D: ForceCloseCheckDb> ForceCloseOrphanSessionCheck<D> {
    /// Construct a check bound to a specific `ForceCloseCheckDb`
    /// projection. In production this will be backed by a thin adapter
    /// over `djinn_db::sessions` + `djinn_db::tasks`; in tests it is
    /// backed by `MemoryForceCloseCheckDb`.
    pub fn new(db: D) -> Self {
        Self { db }
    }

    /// Resolve one orphaned-session candidate into a [`Finding`], if it
    /// is an orphan. Kept private so the snapshot's `inputs`/`outputs`
    /// fields are guaranteed to come from the *same*
    /// `resolve_force_close_state()` call the checker used.
    fn resolve(inputs: &ForceCloseOrphanInputs) -> Option<Finding> {
        let outputs = resolve_force_close_state(inputs);
        if !outputs.is_orphan {
            return None;
        }

        let resolver_inputs_json =
            serde_json::to_value(inputs).expect("ForceCloseOrphanInputs serializes");
        let resolver_outputs_json =
            serde_json::to_value(&outputs).expect("ForceCloseOrphanOutputs serializes");
        let snapshot = ResolverSnapshot::new(
            "resolve_force_close_orphan_session",
            resolver_inputs_json.clone(),
            resolver_outputs_json,
        );

        let evidence = json!({
            "session_id": inputs.session_id,
            "task_id": inputs.task_id,
            "task_close_reason": inputs.task_close_reason,
            "session_status": inputs.session_status,
            "pod_present": inputs.pod_present,
        });

        let detail = format!(
            "session '{}' is still `running` after task '{}' was `force_close`d; \
             force_close does not evict the running session/slot, so the session \
             holds a dispatch slot until the zombie reaper finalizes it — and if the \
             reaper is briefly disabled / failing open, the slot stays wedged \
             indefinitely (the open item from txr4)",
            inputs.session_id, inputs.task_id,
        );

        let mut finding = Finding::new(
            FindingSeverity::Critical,
            "force_close_orphan_session",
            snapshot,
            detail,
        );
        finding = finding
            .with_entity_id("session_id", inputs.session_id.clone())
            .with_entity_id("task_id", inputs.task_id.clone())
            .with_evidence(evidence);
        Some(finding)
    }
}

impl<D: ForceCloseCheckDb + Send + Sync> DoctorCheck for ForceCloseOrphanSessionCheck<D> {
    fn name(&self) -> &'static str {
        "force_close_orphan_session"
    }

    fn description(&self) -> &'static str {
        "Flags sessions left in status=running after the task they belong \
         to has been force_closed — force_close does not evict the running \
         session/slot. The open item from txr4. No state mutation."
    }

    fn run(&self) -> DoctorResult<Vec<Finding>> {
        let candidates = self.db.force_close_orphan_sessions();
        let mut findings = Vec::new();
        for row in candidates {
            let pod_present = self.db.pod_present(&row.task_id);
            let inputs = ForceCloseOrphanInputs {
                session_id: row.session_id.clone(),
                task_id: row.task_id.clone(),
                task_close_reason: "force_close".to_owned(),
                session_status: row.session_status.clone(),
                pod_present,
            };
            if let Some(finding) = Self::resolve(&inputs) {
                findings.push(finding);
            }
        }
        Ok(findings)
    }
}

// ---------------------------------------------------------------------------
// T3 tests — force_close_orphan_session
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests_force_close {
    use super::*;
    use std::collections::BTreeMap;

    /// In-memory `ForceCloseCheckDb` test double for the
    /// force-close-orphan check. Disjoint from T1's `MemoryCheckDb`.
    #[derive(Default)]
    struct MemoryForceCloseCheckDb {
        sessions: Vec<ForceCloseOrphanSessionRow>,
        /// `task_id -> pod_present` overrides. Missing entries default
        /// to `false`.
        pods: BTreeMap<String, bool>,
    }

    impl MemoryForceCloseCheckDb {
        fn with_orphan_running_session() -> Self {
            let mut db = Self::default();
            db.sessions.push(ForceCloseOrphanSessionRow {
                session_id: "sess-orphan".to_owned(),
                task_id: "task-fc".to_owned(),
                session_status: "running".to_owned(),
            });
            db.pods.insert("task-fc".to_owned(), true);
            db
        }

        fn with_orphan_no_pod() -> Self {
            let mut db = Self::default();
            db.sessions.push(ForceCloseOrphanSessionRow {
                session_id: "sess-orphan-nopod".to_owned(),
                task_id: "task-fc2".to_owned(),
                session_status: "running".to_owned(),
            });
            db
        }

        fn with_finalized_session() -> Self {
            let mut db = Self::default();
            db.sessions.push(ForceCloseOrphanSessionRow {
                session_id: "sess-done".to_owned(),
                task_id: "task-done".to_owned(),
                session_status: "closed".to_owned(),
            });
            db
        }
    }

    impl ForceCloseCheckDb for MemoryForceCloseCheckDb {
        fn force_close_orphan_sessions(&self) -> Vec<ForceCloseOrphanSessionRow> {
            self.sessions.clone()
        }
        fn pod_present(&self, task_id: &str) -> bool {
            self.pods.get(task_id).copied().unwrap_or(false)
        }
    }

    fn run_check(db: MemoryForceCloseCheckDb) -> Vec<Finding> {
        let check = ForceCloseOrphanSessionCheck::new(db);
        check.run().expect("run succeeds")
    }

    // -------------------------------------------------------------------
    // Happy path
    // -------------------------------------------------------------------

    #[test]
    fn happy_path_no_finding() {
        let findings = run_check(MemoryForceCloseCheckDb::default());
        assert!(
            findings.is_empty(),
            "empty candidate list must produce no findings, got {:?}",
            findings
        );
    }

    #[test]
    fn happy_path_finalized_session_not_flagged() {
        // A session whose status has moved past `running` is not an
        // orphan even if the task was force_closed.
        let findings = run_check(MemoryForceCloseCheckDb::with_finalized_session());
        assert!(
            findings.is_empty(),
            "a finalized session must not be flagged, got {:?}",
            findings
        );
    }

    // -------------------------------------------------------------------
    // Divergence
    // -------------------------------------------------------------------

    #[test]
    fn divergence_finding_shape() {
        // The canonical orphan: a `running` session whose task was
        // force_closed, with a live pod still present.
        let findings = run_check(MemoryForceCloseCheckDb::with_orphan_running_session());
        assert_eq!(findings.len(), 1, "exactly one orphan finding expected");
        let finding = &findings[0];
        assert_eq!(finding.severity, FindingSeverity::Critical);
        assert_eq!(finding.check_name, "force_close_orphan_session");
        assert_eq!(
            finding.entity_ids.get("session_id").map(String::as_str),
            Some("sess-orphan"),
            "entity_ids must contain the divergent session id"
        );
        assert_eq!(
            finding.entity_ids.get("task_id").map(String::as_str),
            Some("task-fc"),
            "entity_ids must contain the force-closed task id"
        );
        // Evidence must surface the reaper-relevant fields.
        assert_eq!(finding.evidence["session_id"], "sess-orphan");
        assert_eq!(finding.evidence["task_id"], "task-fc");
        assert_eq!(finding.evidence["task_close_reason"], "force_close");
        assert_eq!(finding.evidence["session_status"], "running");
        assert_eq!(finding.evidence["pod_present"], true);

        // Snapshot must be populated and re-runnable: feeding
        // `snapshot.inputs` back into the same resolver reproduces
        // `snapshot.outputs` exactly.
        assert_eq!(
            finding.resolver_snapshot.resolver,
            "resolve_force_close_orphan_session"
        );
        let snapshot_inputs: ForceCloseOrphanInputs =
            serde_json::from_value(finding.resolver_snapshot.inputs.clone())
                .expect("snapshot inputs deserialize as ForceCloseOrphanInputs");
        let replay_outputs = resolve_force_close_state(&snapshot_inputs);
        let replay_outputs_json = serde_json::to_value(&replay_outputs).expect("outputs serialize");
        assert_eq!(
            replay_outputs_json, finding.resolver_snapshot.outputs,
            "resolver snapshot must be reproducible from snapshot.inputs"
        );
        assert_eq!(snapshot_inputs.session_id, "sess-orphan");
        assert_eq!(snapshot_inputs.task_id, "task-fc");
        assert_eq!(snapshot_inputs.task_close_reason, "force_close");
        assert_eq!(snapshot_inputs.session_status, "running");
        assert!(snapshot_inputs.pod_present);
    }

    #[test]
    fn divergence_orphan_without_pod_still_flagged() {
        // The pod may already be gone (e.g. activeDeadline dropped it)
        // but the session row is still `running` — still an orphan.
        let findings = run_check(MemoryForceCloseCheckDb::with_orphan_no_pod());
        assert_eq!(findings.len(), 1);
        let finding = &findings[0];
        assert_eq!(finding.evidence["pod_present"], false);
    }

    // -------------------------------------------------------------------
    // Resolver purity / shared-resolver invariant
    // -------------------------------------------------------------------

    #[test]
    fn resolve_is_pure() {
        let inputs = ForceCloseOrphanInputs {
            session_id: "sess-x".to_owned(),
            task_id: "task-x".to_owned(),
            task_close_reason: "force_close".to_owned(),
            session_status: "running".to_owned(),
            pod_present: true,
        };
        let a = resolve_force_close_state(&inputs);
        let b = resolve_force_close_state(&inputs);
        assert_eq!(a, b);
        assert!(a.is_orphan);
        assert_eq!(a.reason, ForceCloseOrphanReason::Orphan);
    }

    #[test]
    fn resolve_healthy_when_session_not_running() {
        let inputs = ForceCloseOrphanInputs {
            session_id: "sess-y".to_owned(),
            task_id: "task-y".to_owned(),
            task_close_reason: "force_close".to_owned(),
            session_status: "closed".to_owned(),
            pod_present: false,
        };
        let out = resolve_force_close_state(&inputs);
        assert!(!out.is_orphan);
        assert_eq!(out.reason, ForceCloseOrphanReason::Healthy);
    }

    #[test]
    fn resolve_healthy_when_close_reason_not_force_close() {
        let inputs = ForceCloseOrphanInputs {
            session_id: "sess-z".to_owned(),
            task_id: "task-z".to_owned(),
            task_close_reason: "completed".to_owned(),
            session_status: "running".to_owned(),
            pod_present: false,
        };
        let out = resolve_force_close_state(&inputs);
        assert!(!out.is_orphan);
        assert_eq!(out.reason, ForceCloseOrphanReason::Healthy);
    }

    // -------------------------------------------------------------------
    // Check name / description / default fix
    // -------------------------------------------------------------------

    #[test]
    fn check_name_and_description_are_stable() {
        let check = ForceCloseOrphanSessionCheck::new(MemoryForceCloseCheckDb::default());
        assert_eq!(check.name(), "force_close_orphan_session");
        assert!(
            check.description().contains("force_close"),
            "description should mention force_close: got {:?}",
            check.description()
        );
    }

    #[test]
    fn check_does_not_override_fix() {
        // Per the design, T3's checks do not override `fix`; the
        // default `Err(FixNotSupported)` from the framework is
        // intentional. Asserting the trait default keeps that contract
        // explicit.
        let check = ForceCloseOrphanSessionCheck::new(MemoryForceCloseCheckDb::default());
        let finding = Finding::new(
            FindingSeverity::Critical,
            "force_close_orphan_session",
            ResolverSnapshot::new("resolve_force_close_orphan_session", json!({}), json!({})),
            "synthetic",
        );
        let err = check
            .fix(&finding)
            .expect_err("default fix must return FixNotSupported");
        match err {
            crate::doctor::DoctorError::FixNotSupported { check } => {
                assert_eq!(check, "force_close_orphan_session");
            }
            other => panic!("expected FixNotSupported, got {other:?}"),
        }
    }
}
