//! `slot_pool_divergence` doctor check.
//!
//! The slot pool is the in-memory `(model, user) -> slot_id` map that
//! the supervisor uses to hand work to a worker. Two classes of
//! divergence can wedge it:
//!
//! - **Duplicate free-list entries**: the same `(model_id, user_id)`
//!   pair has two slots indexed under it. Either the pool double-counted
//!   a free slot (capacity reported as `2*max` when the underlying
//!   resource is `max`), or — the historical wedge in
//!   `[[cases/project-runaway-loop-second-strike-and-slot-wedge]]` — a
//!   slot got added back to the free list twice and the dispatcher's
//!   "is there a free slot?" check returns true when it should return
//!   false, so a task is dispatched onto a slot that is still
//!   "logically" busy. The dispatcher then errors with
//!   `SlotError::Busy` and the task is deferred — exactly the
//!   "deferred forever" pattern from the case note.
//! - **Orphan busy slot**: a slot in `busy` state has no corresponding
//!   active `task_runs` row. Either the task was finalized without the
//!   pool being told (a leak), or the pool registered a busy slot
//!   before the task_run row was committed (a race). In both cases the
//!   slot is "wedged busy" and a future dispatch will fail with
//!   `SlotError::Busy` for an unrelated task.
//!
//! This check is a *detector*: it flags both divergence kinds but does
//! not repair. The framework's `fix()` is left as the default
//! `Err(FixNotSupported)` because per-check fixers are out of scope
//! for the seed-check wave.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::doctor::{DoctorCheck, DoctorResult, Finding, FindingSeverity, ResolverSnapshot};

/// A read-only projection of the inputs the slot-pool check needs.
///
/// The check takes `&dyn CheckDb` so the fabrication tests use a pure
/// in-memory double. A future adapter (in a follow-up epic) will
/// provide an impl backed by `djinn_db::task_runs` + the live
/// `SlotPoolHandle`.
pub trait CheckDb {
    /// Every slot-pool entry visible to the doctor. The check groups
    /// by `(model_id, user_id)` to find duplicates.
    fn slot_pool(&self) -> Vec<SlotRow>;

    /// IDs of `task_runs` rows whose status is `running` (or any
    /// equivalent "active" state). The check uses this to detect
    /// orphan-busy slots — a `busy` slot whose `busy_for_task` is
    /// not in this set has no live task to back it.
    fn active_task_run_ids(&self) -> Vec<String>;
}

/// A minimal projection of a slot-pool row. Mirrors the shape
/// `[[djinn-agent::actors::slot::SlotInfo]]` but stripped of
/// djinn-agent-specific types so `djinn-core` does not need a build
/// dep on `djinn-agent`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotRow {
    pub slot_id: String,
    pub model_id: String,
    pub user_id: String,
    /// One of `"free"`, `"busy"`, `"draining"`. Stored lowercase to
    /// match the in-memory pool's `serde(tag = "state", rename_all =
    /// "snake_case")` tag. Values are matched case-sensitively against
    /// the constants below.
    pub state: String,
    /// Required when `state == "busy"`. The check uses this to look up
    /// the corresponding `task_runs` row.
    pub busy_for_task: Option<String>,
}

impl SlotRow {
    /// `true` iff the slot is in the `busy` state.
    fn is_busy(&self) -> bool {
        self.state == "busy"
    }
}

/// Inputs the resolver consumes for one divergent slot group.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SlotDivergenceInputs {
    /// What kind of divergence this finding represents.
    pub divergence_kind: DivergenceKind,
    pub model_id: String,
    pub user_id: String,
    /// All slot ids in the divergent group, in stable (sorted) order
    /// so the snapshot is reproducible.
    pub slot_ids: Vec<String>,
    /// States of the slots in `slot_ids` (same order).
    pub slot_states: Vec<String>,
    /// If `divergence_kind == OrphanBusy`, the task_id the orphan
    /// slot was `busy_for` and that has no corresponding active
    /// `task_runs` row. `None` otherwise.
    pub orphan_busy_for_task: Option<String>,
}

/// The two divergence classes the resolver distinguishes.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DivergenceKind {
    /// Two or more slot-pool entries share the same `(model_id,
    /// user_id)` pair. This is the historical "slot is busy" wedge.
    Duplicate,
    /// A slot in `busy` state has no corresponding active `task_runs`
    /// row. The slot is "wedged busy" and a future dispatch will
    /// fail.
    OrphanBusy,
}

/// Outputs the resolver returns.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SlotDivergenceOutputs {
    pub is_divergent: bool,
    pub reason: DivergenceKind,
    /// Number of slot ids in the divergent group. For `Duplicate`
    /// this is `>= 2`; for `OrphanBusy` it is `1`.
    pub slot_count: usize,
}

/// The shared resolver. Both `run()` and the (future) `fix()` call
/// this so the snapshot's `inputs` can reproduce `snapshot.outputs`
/// exactly — the shared-resolver invariant from the doctor framework
/// module docs.
fn resolve_state(inputs: &SlotDivergenceInputs) -> SlotDivergenceOutputs {
    let is_divergent = match inputs.divergence_kind {
        DivergenceKind::Duplicate => inputs.slot_ids.len() >= 2,
        DivergenceKind::OrphanBusy => {
            inputs.slot_ids.len() == 1 && inputs.orphan_busy_for_task.is_some()
        }
    };
    SlotDivergenceOutputs {
        is_divergent,
        reason: inputs.divergence_kind,
        slot_count: inputs.slot_ids.len(),
    }
}

/// `DoctorCheck` impl that flags slot-pool free-list divergence.
///
/// The check is read-only. It does not call any `fix()`-shaped
/// function and does not import `supervisor_impl::pr`; it mirrors the
/// historical "slot is busy" wedge as a detector.
pub struct SlotPoolDivergenceCheck<D: CheckDb> {
    db: D,
}

impl<D: CheckDb> SlotPoolDivergenceCheck<D> {
    /// Construct a check bound to a specific `CheckDb` projection. In
    /// production this will be backed by a thin adapter over the live
    /// `SlotPoolHandle` + `djinn_db::task_runs`; in tests it is backed
    /// by `MemoryCheckDb`.
    pub fn new(db: D) -> Self {
        Self { db }
    }

    /// Find duplicate `(model_id, user_id)` groups in the slot pool.
    /// Returns one `SlotDivergenceInputs` per divergent group, with
    /// the slot ids sorted so the snapshot is deterministic.
    fn find_duplicates(pool: &[SlotRow]) -> Vec<SlotDivergenceInputs> {
        let mut by_pair: BTreeMap<(String, String), Vec<&SlotRow>> = BTreeMap::new();
        for slot in pool {
            by_pair
                .entry((slot.model_id.clone(), slot.user_id.clone()))
                .or_default()
                .push(slot);
        }
        let mut out = Vec::new();
        for ((model_id, user_id), slots) in by_pair {
            if slots.len() < 2 {
                continue;
            }
            let mut sorted_slots = slots;
            sorted_slots.sort_by(|a, b| a.slot_id.cmp(&b.slot_id));
            let slot_ids: Vec<String> = sorted_slots.iter().map(|s| s.slot_id.clone()).collect();
            let slot_states: Vec<String> = sorted_slots.iter().map(|s| s.state.clone()).collect();
            out.push(SlotDivergenceInputs {
                divergence_kind: DivergenceKind::Duplicate,
                model_id,
                user_id,
                slot_ids,
                slot_states,
                orphan_busy_for_task: None,
            });
        }
        out
    }

    /// Find busy slots whose `busy_for_task` is not in the active
    /// `task_runs` set. Returns one `SlotDivergenceInputs` per orphan.
    fn find_orphan_busy(
        pool: &[SlotRow],
        active_task_runs: &[String],
    ) -> Vec<SlotDivergenceInputs> {
        let mut out = Vec::new();
        // Sort the active set so the per-finding snapshot is
        // deterministic and the resolver's outputs are reproducible
        // for any future fix path.
        let mut active: Vec<String> = active_task_runs.to_vec();
        active.sort();
        let active_set: std::collections::BTreeSet<&str> =
            active.iter().map(String::as_str).collect();
        let mut sorted_pool: Vec<&SlotRow> = pool.iter().collect();
        sorted_pool.sort_by(|a, b| a.slot_id.cmp(&b.slot_id));
        for slot in sorted_pool {
            if !slot.is_busy() {
                continue;
            }
            let Some(task_id) = slot.busy_for_task.as_deref() else {
                continue;
            };
            if active_set.contains(task_id) {
                continue;
            }
            out.push(SlotDivergenceInputs {
                divergence_kind: DivergenceKind::OrphanBusy,
                model_id: slot.model_id.clone(),
                user_id: slot.user_id.clone(),
                slot_ids: vec![slot.slot_id.clone()],
                slot_states: vec![slot.state.clone()],
                orphan_busy_for_task: Some(task_id.to_owned()),
            });
        }
        out
    }

    /// Resolve one slot-pool divergence candidate into a [`Finding`], if
    /// it is divergent. Kept private so the snapshot's `inputs`/`outputs`
    /// fields are guaranteed to come from the *same* `resolve_state()` call
    /// the checker used.
    fn resolve(inputs: &SlotDivergenceInputs) -> Option<Finding> {
        let outputs = resolve_state(inputs);
        if !outputs.is_divergent {
            return None;
        }

        let resolver_inputs_json =
            serde_json::to_value(inputs).expect("SlotDivergenceInputs serializes");
        let resolver_outputs_json =
            serde_json::to_value(&outputs).expect("SlotDivergenceOutputs serializes");
        let snapshot = ResolverSnapshot::new(
            "resolve_slot_pool_divergence",
            resolver_inputs_json.clone(),
            resolver_outputs_json,
        );

        let kind_str = match inputs.divergence_kind {
            DivergenceKind::Duplicate => "duplicate",
            DivergenceKind::OrphanBusy => "orphan_busy",
        };
        let evidence = json!({
            "divergence_kind": kind_str,
            "model_id": inputs.model_id,
            "user_id": inputs.user_id,
            "slot_ids": inputs.slot_ids,
            "slot_states": inputs.slot_states,
            "orphan_busy_for_task": inputs.orphan_busy_for_task,
            "slot_count": inputs.slot_ids.len(),
        });

        let detail = match inputs.divergence_kind {
            DivergenceKind::Duplicate => format!(
                "slot pool has {} slot(s) indexed under (model='{}', \
                 user='{}'): slot_ids={:?}; the historical 'slot is \
                 busy' wedge allows a busy slot to leak back into the \
                 free list and the dispatcher dispatches onto it, then \
                 errors with SlotError::Busy and the task is deferred",
                inputs.slot_ids.len(),
                inputs.model_id,
                inputs.user_id,
                inputs.slot_ids,
            ),
            DivergenceKind::OrphanBusy => format!(
                "slot '{}' on (model='{}', user='{}') is `busy` for \
                 task '{}' but no active task_runs row backs it; the \
                 slot is wedged and future dispatches will fail with \
                 SlotError::Busy",
                inputs.slot_ids.first().map(String::as_str).unwrap_or("?"),
                inputs.model_id,
                inputs.user_id,
                inputs.orphan_busy_for_task.as_deref().unwrap_or("?"),
            ),
        };

        let mut finding = Finding::new(
            FindingSeverity::Critical,
            "slot_pool_divergence",
            snapshot,
            detail,
        );
        finding = finding
            .with_entity_id("model_id", inputs.model_id.clone())
            .with_entity_id("user_id", inputs.user_id.clone())
            .with_entity_id("divergence_kind", kind_str.to_owned())
            .with_evidence(evidence);
        if inputs.slot_ids.len() == 1 {
            finding = finding.with_entity_id("slot_id", inputs.slot_ids[0].clone());
        } else {
            finding = finding.with_entity_id("slot_ids", inputs.slot_ids.join(","));
        }
        if let Some(task_id) = inputs.orphan_busy_for_task.as_deref() {
            finding = finding.with_entity_id("task_id", task_id.to_owned());
        }
        Some(finding)
    }
}

impl<D: CheckDb + Send + Sync> DoctorCheck for SlotPoolDivergenceCheck<D> {
    fn name(&self) -> &'static str {
        "slot_pool_divergence"
    }

    fn description(&self) -> &'static str {
        "Flags slot-pool free-list divergence: duplicate slot entries \
         for the same (model_id, user_id) pair, or a slot in `busy` \
         state with no corresponding active task_runs row. Maps to the \
         historical 'slot is busy' wedge."
    }

    fn run(&self) -> DoctorResult<Vec<Finding>> {
        let pool = self.db.slot_pool();
        let active = self.db.active_task_run_ids();
        let mut findings = Vec::new();
        for inputs in Self::find_duplicates(&pool) {
            if let Some(finding) = Self::resolve(&inputs) {
                findings.push(finding);
            }
        }
        for inputs in Self::find_orphan_busy(&pool, &active) {
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

    /// In-memory `CheckDb` test double.
    #[derive(Default)]
    struct MemoryCheckDb {
        pool: Vec<SlotRow>,
        active_task_runs: Vec<String>,
    }

    impl MemoryCheckDb {
        fn with_duplicate_slots() -> Self {
            let mut db = Self::default();
            db.pool.push(SlotRow {
                slot_id: "slot-1".to_owned(),
                model_id: "model-a".to_owned(),
                user_id: "user-1".to_owned(),
                state: "free".to_owned(),
                busy_for_task: None,
            });
            db.pool.push(SlotRow {
                slot_id: "slot-2".to_owned(),
                model_id: "model-a".to_owned(),
                user_id: "user-1".to_owned(),
                state: "free".to_owned(),
                busy_for_task: None,
            });
            db
        }

        fn with_orphan_busy_slot() -> Self {
            let mut db = Self::default();
            db.pool.push(SlotRow {
                slot_id: "slot-orphan".to_owned(),
                model_id: "model-b".to_owned(),
                user_id: "user-2".to_owned(),
                state: "busy".to_owned(),
                busy_for_task: Some("task-orphan".to_owned()),
            });
            db
        }

        fn with_healthy_pool() -> Self {
            let mut db = Self::default();
            db.pool.push(SlotRow {
                slot_id: "slot-1".to_owned(),
                model_id: "model-a".to_owned(),
                user_id: "user-1".to_owned(),
                state: "busy".to_owned(),
                busy_for_task: Some("task-1".to_owned()),
            });
            db.pool.push(SlotRow {
                slot_id: "slot-2".to_owned(),
                model_id: "model-b".to_owned(),
                user_id: "user-2".to_owned(),
                state: "free".to_owned(),
                busy_for_task: None,
            });
            db.active_task_runs.push("task-1".to_owned());
            db
        }

        fn with_orphan_busy_backed_by_active_task_run() -> Self {
            // Edge: a busy slot whose busy_for_task IS in the
            // active task_runs set. The check must NOT flag this —
            // it is the normal healthy busy slot.
            let mut db = Self::default();
            db.pool.push(SlotRow {
                slot_id: "slot-1".to_owned(),
                model_id: "model-a".to_owned(),
                user_id: "user-1".to_owned(),
                state: "busy".to_owned(),
                busy_for_task: Some("task-live".to_owned()),
            });
            db.active_task_runs.push("task-live".to_owned());
            db
        }
    }

    impl CheckDb for MemoryCheckDb {
        fn slot_pool(&self) -> Vec<SlotRow> {
            self.pool.clone()
        }
        fn active_task_run_ids(&self) -> Vec<String> {
            self.active_task_runs.clone()
        }
    }

    fn run_check(db: MemoryCheckDb) -> Vec<Finding> {
        let check = SlotPoolDivergenceCheck::new(db);
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
            "empty slot pool must produce no findings, got {:?}",
            findings
        );
    }

    #[test]
    fn happy_path_healthy_pool_emits_no_finding() {
        let findings = run_check(MemoryCheckDb::with_healthy_pool());
        assert!(
            findings.is_empty(),
            "healthy pool with one busy slot backed by an active \
             task_run must not be flagged, got {:?}",
            findings
        );
    }

    #[test]
    fn happy_path_busy_slot_backed_by_active_task_run_emits_no_finding() {
        let findings = run_check(MemoryCheckDb::with_orphan_busy_backed_by_active_task_run());
        assert!(
            findings.is_empty(),
            "busy slot whose busy_for_task is active must not be \
             flagged, got {:?}",
            findings
        );
    }

    // -------------------------------------------------------------------
    // Divergence: duplicate
    // -------------------------------------------------------------------

    #[test]
    fn divergence_duplicate_finding_shape() {
        let findings = run_check(MemoryCheckDb::with_duplicate_slots());
        assert_eq!(
            findings.len(),
            1,
            "exactly one duplicate finding expected, got {:?}",
            findings
        );
        let finding = &findings[0];
        assert_eq!(finding.severity, FindingSeverity::Critical);
        assert_eq!(finding.check_name, "slot_pool_divergence");
        assert_eq!(
            finding.entity_ids.get("model_id").map(String::as_str),
            Some("model-a")
        );
        assert_eq!(
            finding.entity_ids.get("user_id").map(String::as_str),
            Some("user-1")
        );
        assert_eq!(
            finding
                .entity_ids
                .get("divergence_kind")
                .map(String::as_str),
            Some("duplicate")
        );
        assert_eq!(
            finding.entity_ids.get("slot_ids").map(String::as_str),
            Some("slot-1,slot-2"),
            "entity_ids must contain the divergent slot ids"
        );

        // Evidence must surface the divergent fields.
        assert_eq!(finding.evidence["divergence_kind"], "duplicate");
        assert_eq!(finding.evidence["model_id"], "model-a");
        assert_eq!(finding.evidence["user_id"], "user-1");
        let evidence_slot_ids = finding.evidence["slot_ids"]
            .as_array()
            .expect("slot_ids is an array");
        assert_eq!(evidence_slot_ids.len(), 2);
        assert_eq!(finding.evidence["slot_count"], 2);

        // Snapshot must be populated and re-runnable.
        assert_eq!(
            finding.resolver_snapshot.resolver,
            "resolve_slot_pool_divergence"
        );
        let snapshot_inputs: SlotDivergenceInputs =
            serde_json::from_value(finding.resolver_snapshot.inputs.clone())
                .expect("snapshot inputs deserialize");
        let replay_outputs = resolve_state(&snapshot_inputs);
        let replay_outputs_json = serde_json::to_value(&replay_outputs).expect("outputs serialize");
        assert_eq!(
            replay_outputs_json, finding.resolver_snapshot.outputs,
            "resolver snapshot must be reproducible from snapshot.inputs"
        );
        assert_eq!(snapshot_inputs.divergence_kind, DivergenceKind::Duplicate);
        assert_eq!(snapshot_inputs.model_id, "model-a");
        assert_eq!(snapshot_inputs.user_id, "user-1");
        assert_eq!(snapshot_inputs.slot_ids.len(), 2);
        assert!(snapshot_inputs.slot_ids.contains(&"slot-1".to_owned()));
        assert!(snapshot_inputs.slot_ids.contains(&"slot-2".to_owned()));
    }

    // -------------------------------------------------------------------
    // Divergence: orphan busy
    // -------------------------------------------------------------------

    #[test]
    fn divergence_orphan_busy_finding_shape() {
        let findings = run_check(MemoryCheckDb::with_orphan_busy_slot());
        assert_eq!(
            findings.len(),
            1,
            "exactly one orphan-busy finding expected, got {:?}",
            findings
        );
        let finding = &findings[0];
        assert_eq!(finding.severity, FindingSeverity::Critical);
        assert_eq!(finding.check_name, "slot_pool_divergence");
        assert_eq!(
            finding.entity_ids.get("model_id").map(String::as_str),
            Some("model-b")
        );
        assert_eq!(
            finding.entity_ids.get("user_id").map(String::as_str),
            Some("user-2")
        );
        assert_eq!(
            finding
                .entity_ids
                .get("divergence_kind")
                .map(String::as_str),
            Some("orphan_busy")
        );
        assert_eq!(
            finding.entity_ids.get("slot_id").map(String::as_str),
            Some("slot-orphan")
        );
        assert_eq!(
            finding.entity_ids.get("task_id").map(String::as_str),
            Some("task-orphan")
        );

        // Evidence structure.
        assert_eq!(finding.evidence["divergence_kind"], "orphan_busy");
        assert_eq!(finding.evidence["orphan_busy_for_task"], "task-orphan");
        assert_eq!(finding.evidence["slot_count"], 1);

        // Snapshot must be populated and re-runnable.
        assert_eq!(
            finding.resolver_snapshot.resolver,
            "resolve_slot_pool_divergence"
        );
        let snapshot_inputs: SlotDivergenceInputs =
            serde_json::from_value(finding.resolver_snapshot.inputs.clone())
                .expect("snapshot inputs deserialize");
        let replay_outputs = resolve_state(&snapshot_inputs);
        let replay_outputs_json = serde_json::to_value(&replay_outputs).expect("outputs serialize");
        assert_eq!(
            replay_outputs_json, finding.resolver_snapshot.outputs,
            "resolver snapshot must be reproducible from snapshot.inputs"
        );
        assert_eq!(snapshot_inputs.divergence_kind, DivergenceKind::OrphanBusy);
        assert_eq!(
            snapshot_inputs.orphan_busy_for_task.as_deref(),
            Some("task-orphan")
        );
        assert_eq!(snapshot_inputs.slot_ids, vec!["slot-orphan".to_owned()]);
    }

    // -------------------------------------------------------------------
    // Negative test: divergent input where the historical wedge would
    // catch it (busy slot with a leak).
    // -------------------------------------------------------------------

    #[test]
    fn divergence_negative_orphan_busy_emits_finding() {
        // Divergent input the historical wedge would catch: a busy
        // slot whose busy_for_task has no active task_run. The
        // doctor verifies (doesn't replace) — it must still emit a
        // finding.
        let findings = run_check(MemoryCheckDb::with_orphan_busy_slot());
        assert_eq!(findings.len(), 1);
        let finding = &findings[0];
        let inputs: SlotDivergenceInputs =
            serde_json::from_value(finding.resolver_snapshot.inputs.clone()).unwrap();
        assert_eq!(inputs.divergence_kind, DivergenceKind::OrphanBusy);
        assert_eq!(inputs.orphan_busy_for_task.as_deref(), Some("task-orphan"));
    }

    // -------------------------------------------------------------------
    // Combined: duplicates + orphans
    // -------------------------------------------------------------------

    #[test]
    fn divergence_combined_duplicate_and_orphan_busy() {
        let mut db = MemoryCheckDb::with_duplicate_slots();
        // Add an orphan busy slot on a different (model, user) pair
        // so both kinds of divergence appear in the same run.
        db.pool.push(SlotRow {
            slot_id: "slot-orphan".to_owned(),
            model_id: "model-c".to_owned(),
            user_id: "user-3".to_owned(),
            state: "busy".to_owned(),
            busy_for_task: Some("task-orphan".to_owned()),
        });
        let findings = run_check(db);
        assert_eq!(findings.len(), 2, "expected one duplicate + one orphan");
        let kinds: Vec<&str> = findings
            .iter()
            .map(|f| {
                f.entity_ids
                    .get("divergence_kind")
                    .map(String::as_str)
                    .unwrap_or("?")
            })
            .collect();
        assert!(kinds.contains(&"duplicate"));
        assert!(kinds.contains(&"orphan_busy"));
    }

    // -------------------------------------------------------------------
    // Resolver purity / shared-resolver invariant
    // -------------------------------------------------------------------

    #[test]
    fn resolve_duplicate_is_pure() {
        let inputs = SlotDivergenceInputs {
            divergence_kind: DivergenceKind::Duplicate,
            model_id: "model-a".to_owned(),
            user_id: "user-1".to_owned(),
            slot_ids: vec!["slot-1".to_owned(), "slot-2".to_owned()],
            slot_states: vec!["free".to_owned(), "free".to_owned()],
            orphan_busy_for_task: None,
        };
        let a = resolve_state(&inputs);
        let b = resolve_state(&inputs);
        assert_eq!(a, b);
        assert!(a.is_divergent);
        assert_eq!(a.reason, DivergenceKind::Duplicate);
        assert_eq!(a.slot_count, 2);
    }

    #[test]
    fn resolve_orphan_busy_is_pure() {
        let inputs = SlotDivergenceInputs {
            divergence_kind: DivergenceKind::OrphanBusy,
            model_id: "model-b".to_owned(),
            user_id: "user-2".to_owned(),
            slot_ids: vec!["slot-orphan".to_owned()],
            slot_states: vec!["busy".to_owned()],
            orphan_busy_for_task: Some("task-orphan".to_owned()),
        };
        let a = resolve_state(&inputs);
        let b = resolve_state(&inputs);
        assert_eq!(a, b);
        assert!(a.is_divergent);
        assert_eq!(a.reason, DivergenceKind::OrphanBusy);
        assert_eq!(a.slot_count, 1);
    }

    #[test]
    fn resolve_rejects_duplicate_with_fewer_than_two_slots() {
        let inputs = SlotDivergenceInputs {
            divergence_kind: DivergenceKind::Duplicate,
            model_id: "model-a".to_owned(),
            user_id: "user-1".to_owned(),
            slot_ids: vec!["slot-1".to_owned()],
            slot_states: vec!["free".to_owned()],
            orphan_busy_for_task: None,
        };
        let out = resolve_state(&inputs);
        assert!(!out.is_divergent);
    }

    #[test]
    fn resolve_rejects_orphan_busy_without_task() {
        let inputs = SlotDivergenceInputs {
            divergence_kind: DivergenceKind::OrphanBusy,
            model_id: "model-b".to_owned(),
            user_id: "user-2".to_owned(),
            slot_ids: vec!["slot-orphan".to_owned()],
            slot_states: vec!["busy".to_owned()],
            orphan_busy_for_task: None,
        };
        let out = resolve_state(&inputs);
        assert!(!out.is_divergent);
    }

    // -------------------------------------------------------------------
    // Snapshot stability
    // -------------------------------------------------------------------

    #[test]
    fn duplicate_finding_slot_ids_are_sorted() {
        // Insert slots in reverse-alphabetical order; the snapshot
        // must record them in sorted order so a future replay is
        // deterministic.
        let mut db = MemoryCheckDb::default();
        db.pool.push(SlotRow {
            slot_id: "slot-z".to_owned(),
            model_id: "model-a".to_owned(),
            user_id: "user-1".to_owned(),
            state: "free".to_owned(),
            busy_for_task: None,
        });
        db.pool.push(SlotRow {
            slot_id: "slot-a".to_owned(),
            model_id: "model-a".to_owned(),
            user_id: "user-1".to_owned(),
            state: "free".to_owned(),
            busy_for_task: None,
        });
        let findings = run_check(db);
        assert_eq!(findings.len(), 1);
        let inputs: SlotDivergenceInputs =
            serde_json::from_value(findings[0].resolver_snapshot.inputs.clone()).unwrap();
        assert_eq!(
            inputs.slot_ids,
            vec!["slot-a".to_owned(), "slot-z".to_owned()],
            "slot ids must be sorted for snapshot stability"
        );
    }

    // -------------------------------------------------------------------
    // Check name / description / default fix
    // -------------------------------------------------------------------

    #[test]
    fn check_name_and_description_are_stable() {
        let check = SlotPoolDivergenceCheck::new(MemoryCheckDb::default());
        assert_eq!(check.name(), "slot_pool_divergence");
        assert!(
            check.description().contains("slot"),
            "description should mention slot: got {:?}",
            check.description()
        );
    }

    #[test]
    fn check_does_not_override_fix() {
        let check = SlotPoolDivergenceCheck::new(MemoryCheckDb::default());
        let finding = Finding::new(
            FindingSeverity::Critical,
            "slot_pool_divergence",
            ResolverSnapshot::new("resolve_slot_pool_divergence", json!({}), json!({})),
            "synthetic",
        );
        let err = check
            .fix(&finding)
            .expect_err("default fix must return FixNotSupported");
        match err {
            crate::doctor::DoctorError::FixNotSupported { check } => {
                assert_eq!(check, "slot_pool_divergence");
            }
            other => panic!("expected FixNotSupported, got {other:?}"),
        }
    }
}
