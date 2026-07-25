//! Startup reconciliation planner for the run-dir ledger (proposal nquz,
//! phase 1 — DARK/OBSERVE).
//!
//! This is a PURE planner: it maps an on-disk inventory of UUID-named run
//! directories to durable-ledger upsert intents. It NEVER deletes a directory
//! and NEVER writes to the database — the caller applies the plan through
//! `djinn_db::RunDirRepository::upsert_reconciled`.
//!
//! Reconciliation only inserts an authoritative lifecycle state
//! (`ready_active` / `ready_idle` / `reclaimable`) when a resolver supplies
//! pod/terminal evidence. Every unresolved, malformed, or non-authoritative
//! directory is recorded as `quarantined_unowned`: it counts against observed
//! physical bytes and is NEVER an automated deletion candidate.

use djinn_db::{ReconciledRunDirInput, RunDirKey, RunDirState};

/// One on-disk run directory observed by the inventory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconcileInventoryEntry {
    /// The top-level directory name (a task-run UUID for a well-formed dir).
    pub dir_name: String,
    /// Allocated bytes measured for this directory.
    pub measured_bytes: u64,
    /// Absolute final path of the directory on the volume.
    pub final_path: String,
    /// True when the inventory could not treat this as a well-formed run dir
    /// (malformed name, unreadable subtree, symlink, ...). Always quarantined.
    pub malformed: bool,
}

/// Authoritative ownership resolved from pod/terminal evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedOwnership {
    pub pod_uid: String,
    pub task_run_id: String,
    pub project_id: String,
    pub base_fingerprint: String,
    pub state: RunDirState,
}

/// Resolves a run-dir name to authoritative ownership, or `None` when no
/// pod/terminal evidence exists (which forces a quarantine).
pub trait RunDirOwnershipResolver {
    fn resolve(&self, dir_name: &str) -> Option<ResolvedOwnership>;
}

/// A resolver that never resolves anything — the phase-1 default, under which
/// every inventoried directory is quarantined as unowned.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoOwnershipResolver;

impl RunDirOwnershipResolver for NoOwnershipResolver {
    fn resolve(&self, _dir_name: &str) -> Option<ResolvedOwnership> {
        None
    }
}

/// The reconciliation plan: durable upserts plus bounded accounting. Contains no
/// deletion intent of any kind.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconciliationPlan {
    /// Rows to insert through the ledger repository, in inventory order.
    pub upserts: Vec<ReconciledRunDirInput>,
    /// Number of directories reconciled from authoritative evidence.
    pub resolved_count: usize,
    /// Number of directories quarantined as unowned.
    pub quarantined_count: usize,
    /// Measured bytes attributed to authoritative rows.
    pub resolved_bytes: u64,
    /// Measured bytes attributed to quarantined-unowned rows.
    pub quarantined_bytes: u64,
}

/// The only lifecycle states a reconciler may assert from evidence.
fn is_authoritative_state(state: RunDirState) -> bool {
    matches!(
        state,
        RunDirState::ReadyActive | RunDirState::ReadyIdle | RunDirState::Reclaimable
    )
}

/// Build the reconciliation plan for one volume.
///
/// Deterministic: entries are processed in the order supplied and the plan
/// preserves that order, so a fixed inventory yields a byte-identical plan.
pub fn plan_reconciliation(
    volume_id: &str,
    entries: &[ReconcileInventoryEntry],
    resolver: &dyn RunDirOwnershipResolver,
) -> ReconciliationPlan {
    let mut plan = ReconciliationPlan::default();
    for entry in entries {
        let resolved = if entry.malformed {
            None
        } else {
            resolver
                .resolve(&entry.dir_name)
                .filter(|owned| is_authoritative_state(owned.state))
        };

        match resolved {
            Some(owned) => {
                plan.resolved_count += 1;
                plan.resolved_bytes = plan.resolved_bytes.saturating_add(entry.measured_bytes);
                plan.upserts.push(ReconciledRunDirInput {
                    key: RunDirKey {
                        volume_id: volume_id.to_owned(),
                        pod_uid: owned.pod_uid,
                    },
                    task_run_id: Some(owned.task_run_id),
                    project_id: Some(owned.project_id),
                    base_fingerprint: Some(owned.base_fingerprint),
                    state: owned.state,
                    measured_bytes: entry.measured_bytes as i64,
                    final_path: Some(entry.final_path.clone()),
                });
            }
            None => {
                plan.quarantined_count += 1;
                plan.quarantined_bytes =
                    plan.quarantined_bytes.saturating_add(entry.measured_bytes);
                plan.upserts.push(ReconciledRunDirInput {
                    // With no resolved pod UID, the unique directory name keys the
                    // quarantine row; the untrusted name is NOT stored as an
                    // authoritative task-run binding.
                    key: RunDirKey {
                        volume_id: volume_id.to_owned(),
                        pod_uid: entry.dir_name.clone(),
                    },
                    task_run_id: None,
                    project_id: None,
                    base_fingerprint: None,
                    state: RunDirState::QuarantinedUnowned,
                    measured_bytes: entry.measured_bytes as i64,
                    final_path: Some(entry.final_path.clone()),
                });
            }
        }
    }
    plan
}

#[cfg(test)]
#[path = "run_dir_reconcile_tests.rs"]
mod tests;
