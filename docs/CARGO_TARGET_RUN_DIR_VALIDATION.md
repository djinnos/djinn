# Cargo target run-dir rollout validation

This runbook verifies that per-task-run Cargo target directories prevent same-project worker Pods from contending on Cargo's build-directory lock, and identifies the log fields operators should use to compare seed/cleanup behavior against storage budgets.

For the canonical cross-path rollout order, ownership, stop conditions, and
rollback/rebuild procedures for Zot, sccache, run-root debris, and warm-base
cleanup, use [Shared-cache cleanup rollout and rollback runbook](SHARED_CACHE_CLEANUP_ROLLOUT.md).
That runbook does not change the UUID run-directory lifecycle invariants below.

## What is expected

- Task-run Pods set `CARGO_TARGET_DIR=/cache/cargo-target-runs/<task_run_id>`.
- Warm/verification Pods may still use the shared per-project warm base at `/cache/cargo-target/<project_id>`.
- Shared caches such as `CARGO_HOME=/cache/cargo`, `SCCACHE_DIR=/cache/sccache/<project_id>`, Go, pnpm, pip, and sccache routing are unchanged.
- No chart/PVC storage migration is required; this uses the existing cache PVC and the `cargo-target-runs/` subdirectory.

The deterministic regression test for the manifest-level contract is:

```bash
cd server
cargo test -p djinn-k8s same_project_task_runs_get_distinct_private_cargo_target_dirs --all-features
```

That test constructs two same-project task-run Jobs and proves their `CARGO_TARGET_DIR` values are distinct `/cache/cargo-target-runs/<task_run_id>` paths, not the shared warm base.

## Concurrent same-project lock-contention check

1. Pick a Rust project whose normal worker verification runs Cargo.
2. Dispatch two worker task runs for that same project close together so their Pods overlap.
3. Record both task-run ids:

```bash
export NS=djinn
export TASK_RUN_A=<first-task-run-id>
export TASK_RUN_B=<second-task-run-id>
```

4. Confirm each Pod has a distinct target dir:

```bash
for id in "$TASK_RUN_A" "$TASK_RUN_B"; do
  pod=$(kubectl get pods -n "$NS" -l "djinn.app/task-run-id=$id" -o jsonpath='{.items[0].metadata.name}')
  echo "task_run_id=$id pod=$pod"
  kubectl exec -n "$NS" "$pod" -- sh -lc 'printf "CARGO_TARGET_DIR=%s\n" "$CARGO_TARGET_DIR"'
done
```

Expected output is exactly one private path per task run, e.g. `/cache/cargo-target-runs/$TASK_RUN_A` and `/cache/cargo-target-runs/$TASK_RUN_B`; they must not be equal and neither should be `/cache/cargo-target/<project_id>`.

5. Assert Cargo did not wait on the shared build-dir lock:

```bash
for id in "$TASK_RUN_A" "$TASK_RUN_B"; do
  kubectl logs -n "$NS" -l "djinn.app/task-run-id=$id" --all-containers --since=2h \
    | tee "/var/tmp/djinn-taskrun-$id.log" \
    | grep -F 'Blocking waiting for file lock on build directory' && {
        echo "FAIL: Cargo build-dir lock wait appeared for task_run_id=$id"
        exit 1
      }
done

echo "PASS: zero Cargo build-dir lock waits in concurrent same-project task-run logs"
```

## Seed/clone duration budget evidence

Worker logs emit structured seed records with these messages:

- `cargo target seed: preparing private run target dir`
- `cargo target seed: seeded private run target dir`
- `cargo target seed: falling back to cold private target dir`
- `cargo target seed: proceeding with cold private target dir after setup error`
- `cargo target seed: proceeding with cold private target dir after setup task failure`

Capture them by `task_run_id`:

```bash
kubectl logs -n "$NS" -l "djinn.app/task-run-id=$TASK_RUN_A" --all-containers --since=2h \
  | grep -E 'cargo target seed:|task_run_id=.*'$TASK_RUN_A
```

The completion/fallback log lines include `task_run_id`, `project_id`, `source_base`, `destination_run_dir`, `seed_duration_ms`, the compatibility alias `clone_duration_ms`, file counts (`linked_file_count`, `copied_file_count`, `skipped_file_count`), byte counts (`linked_bytes`, `copied_bytes`), and `fallback_reason` when cold-starting. Compare `seed_duration_ms` against the rollout budget: under 30s on VPS/ext4 and under 60s on EKS/EFS.

## Cleanup observability

Normal worker teardown logs one of:

- `cargo target teardown: private run target dir cleanup completed`
- `cargo target teardown: failed to remove private run target dir`

Those lines include `task_run_id`, `project_id`, `destination_run_dir`, `cleanup_outcome` (`removed`, `already_absent`, or `failed`), `removed_count`, and `error_count`.

Coordinator stale-sweep GC logs per-dir outcomes plus a summary:

- `CoordinatorActor: deleted orphaned cargo target run-dir`
- `CoordinatorActor: failed to delete orphaned cargo target run-dir; continuing`
- `CoordinatorActor: cargo target run-dir sweep completed`

Per-dir logs include `task_run_id`, `path`, `cleanup_outcome`, `deleted_count`, and `error_count`; the summary includes `root`, `scanned`, `deleted`, `retained`, `errors`, and `cleanup_outcome` (`completed` or `completed_with_errors`).

Useful queries:

```bash
kubectl logs -n "$NS" deploy/djinn-server --since=2h \
  | grep -E 'cargo target run-dir sweep|orphaned cargo target run-dir|task_run_id='$TASK_RUN_A

kubectl logs -n "$NS" -l "djinn.app/task-run-id=$TASK_RUN_A" --all-containers --since=2h \
  | grep -E 'cargo target teardown|cargo target seed'
```
