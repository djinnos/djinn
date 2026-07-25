//! Deterministic fixtures and tests for the run-dir reconciliation planner.

use std::collections::HashMap;

use super::*;

const VOLUME: &str = "node-vol-a";

/// A resolver backed by a fixed map for deterministic fixtures.
struct MapResolver {
    map: HashMap<String, ResolvedOwnership>,
}

impl RunDirOwnershipResolver for MapResolver {
    fn resolve(&self, dir_name: &str) -> Option<ResolvedOwnership> {
        self.map.get(dir_name).cloned()
    }
}

fn entry(name: &str, bytes: u64, malformed: bool) -> ReconcileInventoryEntry {
    ReconcileInventoryEntry {
        dir_name: name.to_owned(),
        measured_bytes: bytes,
        final_path: format!("/cache/cargo-target-runs/{name}"),
        malformed,
    }
}

/// A fixed three-directory inventory: one resolvable-live, one resolvable-
/// terminal, one unresolved, plus one malformed entry.
fn fixture_inventory() -> Vec<ReconcileInventoryEntry> {
    vec![
        entry("11111111-1111-1111-1111-111111111111", 1_000, false),
        entry("22222222-2222-2222-2222-222222222222", 2_000, false),
        entry("33333333-3333-3333-3333-333333333333", 4_000, false),
        entry("not-a-uuid", 8_000, true),
    ]
}

fn fixture_resolver() -> MapResolver {
    let mut map = HashMap::new();
    map.insert(
        "11111111-1111-1111-1111-111111111111".to_owned(),
        ResolvedOwnership {
            pod_uid: "pod-live".to_owned(),
            task_run_id: "run-live".to_owned(),
            project_id: "proj-1".to_owned(),
            base_fingerprint: "fp-1".to_owned(),
            state: RunDirState::ReadyActive,
        },
    );
    map.insert(
        "22222222-2222-2222-2222-222222222222".to_owned(),
        ResolvedOwnership {
            pod_uid: "pod-terminal".to_owned(),
            task_run_id: "run-terminal".to_owned(),
            project_id: "proj-1".to_owned(),
            base_fingerprint: "fp-1".to_owned(),
            state: RunDirState::Reclaimable,
        },
    );
    // "3333..." is deliberately absent from the map -> unresolved -> quarantine.
    MapResolver { map }
}

#[test]
fn phase1_no_resolver_quarantines_everything() {
    let inventory = fixture_inventory();
    let plan = plan_reconciliation(VOLUME, &inventory, &NoOwnershipResolver);
    assert_eq!(plan.resolved_count, 0);
    assert_eq!(plan.quarantined_count, 4);
    assert_eq!(plan.quarantined_bytes, 1_000 + 2_000 + 4_000 + 8_000);
    assert!(
        plan.upserts
            .iter()
            .all(|u| u.state == RunDirState::QuarantinedUnowned),
        "phase 1 default resolver must quarantine every dir"
    );
    // No deletion intent is expressible: the plan is upserts only.
}

#[test]
fn resolved_and_unresolved_partition_deterministically() {
    let inventory = fixture_inventory();
    let plan = plan_reconciliation(VOLUME, &inventory, &fixture_resolver());

    assert_eq!(plan.resolved_count, 2);
    assert_eq!(plan.resolved_bytes, 1_000 + 2_000);
    assert_eq!(plan.quarantined_count, 2); // unresolved + malformed
    assert_eq!(plan.quarantined_bytes, 4_000 + 8_000);

    // Order is preserved; identity is taken from authoritative evidence.
    let live = &plan.upserts[0];
    assert_eq!(live.state, RunDirState::ReadyActive);
    assert_eq!(live.key.pod_uid, "pod-live");
    assert_eq!(live.task_run_id.as_deref(), Some("run-live"));

    let terminal = &plan.upserts[1];
    assert_eq!(terminal.state, RunDirState::Reclaimable);
    assert_eq!(terminal.key.pod_uid, "pod-terminal");

    // Unresolved: keyed by the untrusted dir name, no task-run binding stored.
    let unresolved = &plan.upserts[2];
    assert_eq!(unresolved.state, RunDirState::QuarantinedUnowned);
    assert_eq!(
        unresolved.key.pod_uid,
        "33333333-3333-3333-3333-333333333333"
    );
    assert_eq!(unresolved.task_run_id, None);

    // Malformed is always quarantined regardless of any resolver answer.
    let malformed = &plan.upserts[3];
    assert_eq!(malformed.state, RunDirState::QuarantinedUnowned);
    assert_eq!(malformed.key.pod_uid, "not-a-uuid");
}

#[test]
fn malformed_entry_is_quarantined_even_if_resolvable() {
    // A resolver that WOULD resolve the malformed name must be ignored.
    let mut map = HashMap::new();
    map.insert(
        "not-a-uuid".to_owned(),
        ResolvedOwnership {
            pod_uid: "pod-x".to_owned(),
            task_run_id: "run-x".to_owned(),
            project_id: "proj-1".to_owned(),
            base_fingerprint: "fp-1".to_owned(),
            state: RunDirState::ReadyActive,
        },
    );
    let inventory = vec![entry("not-a-uuid", 8_000, true)];
    let plan = plan_reconciliation(VOLUME, &inventory, &MapResolver { map });
    assert_eq!(plan.quarantined_count, 1);
    assert_eq!(plan.resolved_count, 0);
    assert_eq!(plan.upserts[0].state, RunDirState::QuarantinedUnowned);
}

#[test]
fn non_authoritative_resolved_state_is_quarantined() {
    // A resolver returning a non-authoritative state (e.g. seeding) must not be
    // asserted as a reconciled row.
    let mut map = HashMap::new();
    map.insert(
        "11111111-1111-1111-1111-111111111111".to_owned(),
        ResolvedOwnership {
            pod_uid: "pod-live".to_owned(),
            task_run_id: "run-live".to_owned(),
            project_id: "proj-1".to_owned(),
            base_fingerprint: "fp-1".to_owned(),
            state: RunDirState::Seeding,
        },
    );
    let inventory = vec![entry("11111111-1111-1111-1111-111111111111", 1_000, false)];
    let plan = plan_reconciliation(VOLUME, &inventory, &MapResolver { map });
    assert_eq!(plan.resolved_count, 0);
    assert_eq!(plan.quarantined_count, 1);
    assert_eq!(plan.upserts[0].state, RunDirState::QuarantinedUnowned);
}
