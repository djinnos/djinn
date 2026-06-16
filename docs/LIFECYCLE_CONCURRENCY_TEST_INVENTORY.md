# Lifecycle concurrency test inventory (vjs6 / ir2i)

This is the authoritative inventory for the `ir2i` CI/profile follow-up tasks.  The
current merge-queue server test job already runs:

```sh
cd server
cargo nextest run --workspace --all-targets --all-features --profile ci
```

That profile has no test filters, so every test below is discoverable by the
existing merge-queue path.  The local/CI infrastructure requirement is the same
as the rest of the DB-backed Rust suite: a Postgres test template
(`djinn_test_template`) available through `DJINN_TEST_DATABASE_URL` / the default
`127.0.0.1:5433` test database.  No listed test requires kind, a Kubernetes
cluster, or Docker-in-Docker.

Evidence checked while creating this inventory:

- `server/.config/nextest.toml` defines `profile.ci` with retries/slow-timeout
  only; it does not exclude any of these tests.
- `.github/workflows/quality-gate.yml` merge-queue/manual `server-test` builds
  `djinn_test_template` and runs the unfiltered `cargo nextest run --workspace
  --all-targets --all-features --profile ci` command.
- `djinn_db::Database::open_in_memory()` is now Postgres template-clone based
  (`CREATE DATABASE djinn_test_<uuid> TEMPLATE djinn_test_template`), and
  `djinn_agent::test_helpers::create_test_db()` delegates to it.
- Slot-pool/control-plane tests route task-run teardown through recording
  `RuntimeOps` fakes; the slot-pool module also has an explicit layering guard
  that production slot-pool lifecycle code does not import `djinn_k8s` directly.

## Inventory and profile classification

| Family | Exact test path / function | Suggested nextest filter | Runtime support verified | Profile classification |
| --- | --- | --- | --- | --- |
| Dispatch-cap race / per-user max sessions | `server/crates/djinn-agent/src/actors/coordinator/dispatch/task_dispatch.rs::tests::wnd1_dispatch_race_harness_never_exceeds_caps_1_through_5` | `-E 'test(wnd1_dispatch_race_harness_never_exceeds_caps_1_through_5)'` | Uses `test_helpers::create_test_db()` (template Postgres), an in-process `CoordinatorActor`, and a controlled `SlotHandle::spawn_with_test_runner` runtime. The test comment explicitly notes normal nextest discoverability and no kind/k8s. | Existing merge-queue `profile ci`. Deterministic controlled runner; bounded progress deadline; no real worker pod or cluster dependency. |
| Dispatch-cap fixture sanity | `server/crates/djinn-agent/src/actors/coordinator/dispatch/task_dispatch.rs::tests::wnd1_ready_queue_fixture_is_visible_to_dispatch_selection_and_reads_caps` | `-E 'test(wnd1_ready_queue_fixture_is_visible_to_dispatch_selection_and_reads_caps)'` | Uses `test_helpers::create_test_db()` and repository queries only. | Existing merge-queue `profile ci`. Fast DB fixture/selection regression, not a stress test. |
| Slot-pool lifecycle event races | `server/crates/djinn-agent/src/actors/slot/pool/tests.rs::invariant_harness_accepts_stale_busy_slot_self_heal` | `-E 'test(invariant_harness_accepts_stale_busy_slot_self_heal)'` | Uses `test_app_state()` → `create_test_db()`, in-process `SlotPool::new_with_factory`, and a test slot runner. | Existing merge-queue `profile ci`. Focused invariant harness; no timing-sensitive external dependency. |
| Slot-pool lifecycle event races | `server/crates/djinn-agent/src/actors/slot/pool/tests.rs::lifecycle_permutations_preserve_slot_pool_invariants` | `-E 'test(lifecycle_permutations_preserve_slot_pool_invariants)'` | Table-driven in-process permutations over `SlotPool` events using template Postgres and test runners. | Existing merge-queue `profile ci`. This is the primary lifecycle-race regression; bounded deterministic permutations rather than soak/stress. |
| Slot-pool lifecycle event races | `server/crates/djinn-agent/src/actors/slot/pool/tests.rs::mark_slot_free_is_idempotent_and_skips_retired` | `-E 'test(mark_slot_free_is_idempotent_and_skips_retired)'` | White-box in-process slot-pool state test with template Postgres-backed app context. | Existing merge-queue `profile ci`. Pure invariant regression. |
| Slot-pool late event / reclaimed mapping race | `server/crates/djinn-agent/src/actors/slot/pool/tests.rs::actor_handle_evict_then_late_killed_event_preserves_reclaimed_mapping` | `-E 'test(actor_handle_evict_then_late_killed_event_preserves_reclaimed_mapping)'` | Uses `test_app_state()`, `RecordingRuntimeOps`, and `SlotPoolHandle::spawn_with_factory`; no live runtime/k8s. | Existing merge-queue `profile ci`. Deterministic late-event race coverage. |
| Slot-pool killed-event teardown backstop | `server/crates/djinn-agent/src/actors/slot/pool/tests.rs::slot_event_killed_tears_down_taskrun_job` | `-E 'test(slot_event_killed_tears_down_taskrun_job)'` | Uses `RecordingRuntimeOps` and direct `SlotEvent::Killed` handling; DB is template Postgres. | Existing merge-queue `profile ci`. Single event-path regression. |
| Slot-pool synchronous terminate settlement | `server/crates/djinn-agent/src/actors/slot/pool/tests.rs::terminate_session_synchronously_reclaims_mapping_activity_and_session_row` | `-E 'test(terminate_session_synchronously_reclaims_mapping_activity_and_session_row)'` | Uses fake runtime teardown and template Postgres session rows. | Existing merge-queue `profile ci`. Deterministic settlement check; belongs with normal CI coverage. |
| Slot-pool/k8s boundary guard | `server/crates/djinn-agent/src/actors/slot/pool/tests.rs::slot_pool_lifecycle_does_not_import_djinn_k8s_directly` | `-E 'test(slot_pool_lifecycle_does_not_import_djinn_k8s_directly)'` | Reads source files under `src/actors/slot/pool` and asserts no direct `djinn_k8s` imports in production slot-pool lifecycle code. | Existing merge-queue `profile ci`. Fast static guard proving the no-kind/k8s boundary. |
| `execution_kill_task` settlement | `server/crates/djinn-control-plane/tests/execution_tools.rs::execution_kill_task_settles_live_run_through_control_plane_tool_route` | `-E 'test(execution_kill_task_settles_live_run_through_control_plane_tool_route)'` | `RealPoolKillHarness` uses `Database::open_in_memory()`, a real `SlotPoolHandle`, `McpTestHarness`, and `RecordingRuntimeOps`; teardown is recorded, not sent to Kubernetes. | Existing merge-queue `profile ci`. End-to-end tool route with fake runtime side effects; bounded waits. |
| `execution_kill_task` kill-vs-completion race | `server/crates/djinn-control-plane/tests/execution_tools.rs::execution_kill_task_racing_natural_completion_settles_once_and_releases_capacity` | `-E 'test(execution_kill_task_racing_natural_completion_settles_once_and_releases_capacity)'` | Uses a controlled completion gate (`CompletionRaceControl`) so natural settlement and kill interleave deterministically inside the real pool/tool harness. | Existing merge-queue `profile ci`. Deterministic race interleaving; no soak/stress profile needed. |
| `execution_kill_task` double-kill | `server/crates/djinn-control-plane/tests/execution_tools.rs::execution_kill_task_double_kill_is_harmless_and_leaves_capacity_available` | `-E 'test(execution_kill_task_double_kill_is_harmless_and_leaves_capacity_available)'` | Same real-pool/control-plane harness with template Postgres and recording runtime. | Existing merge-queue `profile ci`. Bounded idempotency regression. |
| Reopen/intervention chaos | `server/crates/djinn-agent/src/actors/coordinator/tests/intervention.rs::reopen_loop_guard_second_strike_chaos_parks_without_rearming` | `-E 'test(reopen_loop_guard_second_strike_chaos_parks_without_rearming)'` | `InterventionChaosHarness::new()` uses `Database::open_in_memory()`, an in-process test coordinator, task repository transitions, and durable dispatch-state rows. | Existing merge-queue `profile ci`. Pure DB/coordinator state-machine coverage; no external runtime. |
| Reopen/intervention/same-role cycling chaos | `server/crates/djinn-agent/src/actors/coordinator/tests/intervention.rs::same_role_cycling_trigger_b_chaos_intervenes_then_terminally_closes` | `-E 'test(same_role_cycling_trigger_b_chaos_intervenes_then_terminally_closes)'` | Same in-process `InterventionChaosHarness`; simulates dispatch reappearances by mutating coordinator backoff/dispatch marker state and DB rows. | Existing merge-queue `profile ci`. Deterministic state-machine chaos coverage, not timing-sensitive stress. |

## Combined filters for downstream validation

Use these when validating discovery or running only the lifecycle concurrency
coverage.  They intentionally do not replace the merge-queue command; they are
for targeted local/diagnostic checks.

```sh
cd server

# djinn-agent lifecycle/concurrency inventory
AGENT_FILTER='test(wnd1_dispatch_race_harness_never_exceeds_caps_1_through_5) or test(wnd1_ready_queue_fixture_is_visible_to_dispatch_selection_and_reads_caps) or test(invariant_harness_accepts_stale_busy_slot_self_heal) or test(lifecycle_permutations_preserve_slot_pool_invariants) or test(mark_slot_free_is_idempotent_and_skips_retired) or test(actor_handle_evict_then_late_killed_event_preserves_reclaimed_mapping) or test(slot_event_killed_tears_down_taskrun_job) or test(terminate_session_synchronously_reclaims_mapping_activity_and_session_row) or test(slot_pool_lifecycle_does_not_import_djinn_k8s_directly) or test(reopen_loop_guard_second_strike_chaos_parks_without_rearming) or test(same_role_cycling_trigger_b_chaos_intervenes_then_terminally_closes)'
cargo nextest list -p djinn-agent --all-targets --all-features --profile ci -E "$AGENT_FILTER"

# djinn-control-plane execution_kill_task inventory
KILL_FILTER='test(execution_kill_task_settles_live_run_through_control_plane_tool_route) or test(execution_kill_task_racing_natural_completion_settles_once_and_releases_capacity) or test(execution_kill_task_double_kill_is_harmless_and_leaves_capacity_available)'
cargo nextest list -p djinn-control-plane --test execution_tools --all-features --profile ci -E "$KILL_FILTER"
```

Equivalent grep commands used to build this inventory:

```sh
cd server
grep -RInE '#\[tokio::test|#\[test|async fn|fn .*dispatch|fn .*slot|fn .*kill|fn .*reopen|fn .*intervention' \
  crates src tests --include='*.rs' \
  | grep -Ei 'dispatch|cap|max_sessions|slot|kill|completion|double|reopen|intervention|same_role|chaos|lifecycle|settlement'

grep -RIn 'TestRuntime\|open_in_memory\|DJINN_TEST_DATABASE_URL\|template\|kind\|k8s\|SlotPool\|execution_kill_task' \
  crates src tests --include='*.rs'
```

## CI profile decision

No separate slower/stress profile is justified by the landed code.  The tests are
not long-running soak tests; they use deterministic in-process harnesses,
controlled channels/notifies, template Postgres isolation, and bounded polling
windows.  Keeping them in the existing merge-queue `profile ci` path preserves
the vjs6 lifecycle regression coverage without requiring kind/k8s cluster
infrastructure or weakening assertions.

## Validation breadcrumbs (`ynvk`, 2026-06-16)

These are the practical nextest discovery / profile / no-run commands run
against the final wiring.  The first three are pure discovery + parse
checks and require no infrastructure.  The last one is the same merge-queue
command used by `.github/workflows/quality-gate.yml` `server-test` and
proves the entire workspace compiles + links under `profile ci`.

```sh
cd server

# (1) Profile parsing — proves server/.config/nextest.toml is valid.
cargo nextest show-config version --profile ci
# → current nextest version: 0.9.137  (exit 0)

# (2) Targeted djinn-agent inventory discovery.
AGENT_FILTER='test(wnd1_dispatch_race_harness_never_exceeds_caps_1_through_5) or test(wnd1_ready_queue_fixture_is_visible_to_dispatch_selection_and_reads_caps) or test(invariant_harness_accepts_stale_busy_slot_self_heal) or test(lifecycle_permutations_preserve_slot_pool_invariants) or test(mark_slot_free_is_idempotent_and_skips_retired) or test(actor_handle_evict_then_late_killed_event_preserves_reclaimed_mapping) or test(slot_event_killed_tears_down_taskrun_job) or test(terminate_session_synchronously_reclaims_mapping_activity_and_session_row) or test(slot_pool_lifecycle_does_not_import_djinn_k8s_directly) or test(reopen_loop_guard_second_strike_chaos_parks_without_rearming) or test(same_role_cycling_trigger_b_chaos_intervenes_then_terminally_closes)'
cargo nextest list -p djinn-agent --all-targets --all-features --profile ci -E "$AGENT_FILTER"
# → 11/11 inventory tests discovered under the unfiltered merge-queue profile.

# (3) Targeted djinn-control-plane execution_kill_task inventory discovery.
KILL_FILTER='test(execution_kill_task_settles_live_run_through_control_plane_tool_route) or test(execution_kill_task_racing_natural_completion_settles_once_and_releases_capacity) or test(execution_kill_task_double_kill_is_harmless_and_leaves_capacity_available)'
cargo nextest list -p djinn-control-plane --test execution_tools --all-features --profile ci -E "$KILL_FILTER"
# → 3/3 inventory tests discovered under the unfiltered merge-queue profile.

# (4) Full unfiltered discovery — proves the inventory is a subset of the
#     merge-queue command, not a side list.
cargo nextest list --workspace --all-targets --all-features --profile ci | wc -l
# → 3531 tests total, all 14 inventory tests appear in this list.

# (5) No-run compile of the full merge-queue command — the strongest local
#     check that does not require Postgres/Docker.  Exits 0 means the
#     workspace compiles and links under `profile ci`; the merge-queue
#     `server-test` job only has to add Postgres/template DB on top.
cargo nextest run --workspace --all-targets --all-features --profile ci --no-run
# → Finished `test` profile [unoptimized + debuginfo] target(s)  (exit 0)
```

### Local execution vs CI execution split

- **Locally runnable now (no infrastructure):** only
  `slot_pool_lifecycle_does_not_import_djinn_k8s_directly` — a static
  source-file guard.  It passed in 0.04s on a stripped-down worker pod.
- **DB-backed tests (13 / 14):** need Postgres at
  `127.0.0.1:5433` + a migrated `djinn_test_template`.  The merge-queue
  `server-test` job supplies both.  In the worker pod used for
  validation, no `psql`/`postgres` is installed and no
  `DJINN_TEST_DATABASE_URL` is set; running a representative DB-backed
  test (`wnd1_ready_queue_fixture_is_visible_to_dispatch_selection_and_reads_caps`)
  produced exactly the expected local blocker:

  ```
  thread '...' panicked at crates/djinn-agent/src/test_helpers.rs:114:10:
  failed to create test project: Sqlx(Io(Os { code: 111, kind: ConnectionRefused, message: "Connection refused" }))
  ```

  This is a missing-Postgres / template-DB blocker, not a kind/k8s or
  operator-only proof.  It is the same infrastructure gap the design
  section of this task explicitly anticipated.

### One-line per-family command for the closing planner

For each of the four vjs6 families, the command/filter to run in CI
(or locally when Postgres + `djinn_test_template` are available):

| Family | Command / filter |
| --- | --- |
| Dispatch-cap race / per-user max sessions | `cd server && cargo nextest run -p djinn-agent --all-targets --all-features --profile ci -E 'test(wnd1_dispatch_race_harness_never_exceeds_caps_1_through_5) or test(wnd1_ready_queue_fixture_is_visible_to_dispatch_selection_and_reads_caps)'` |
| Slot-pool lifecycle event races | `cd server && cargo nextest run -p djinn-agent --all-targets --all-features --profile ci -E 'test(invariant_harness_accepts_stale_busy_slot_self_heal) or test(lifecycle_permutations_preserve_slot_pool_invariants) or test(mark_slot_free_is_idempotent_and_skips_retired) or test(actor_handle_evict_then_late_killed_event_preserves_reclaimed_mapping) or test(slot_event_killed_tears_down_taskrun_job) or test(terminate_session_synchronously_reclaims_mapping_activity_and_session_row) or test(slot_pool_lifecycle_does_not_import_djinn_k8s_directly)'` |
| `execution_kill_task` settlement / race / double-kill | `cd server && cargo nextest run -p djinn-control-plane --test execution_tools --all-features --profile ci -E 'test(execution_kill_task_settles_live_run_through_control_plane_tool_route) or test(execution_kill_task_racing_natural_completion_settles_once_and_releases_capacity) or test(execution_kill_task_double_kill_is_harmless_and_leaves_capacity_available)'` |
| Reopen / intervention / same-role cycling chaos | `cd server && cargo nextest run -p djinn-agent --all-targets --all-features --profile ci -E 'test(reopen_loop_guard_second_strike_chaos_parks_without_rearming) or test(same_role_cycling_trigger_b_chaos_intervenes_then_terminally_closes)'` |

All of the above are also discoverable by the unfiltered merge-queue
command the `server-test` job runs:

```sh
cd server && cargo nextest run --workspace --all-targets --all-features --profile ci
```

### ir2i close-criteria breadcrumbs for the next planner

1. **CI wiring is in place** — `server/.config/nextest.toml` keeps
   `profile.ci` unfiltered, and `.github/workflows/quality-gate.yml`
   `server-test` (merge_group + workflow_dispatch only) builds
   `djinn_test_template`, applies migrations, sets up the vault key,
   and runs the unfiltered merge-queue command.  Both files carry
   comments cross-linking the four test families and
   `docs/LIFECYCLE_CONCURRENCY_TEST_INVENTORY.md`.
2. **Documentation is in place** — `README.md` development guidance,
   `Makefile` (`test-all` target), and `server/scripts/verify` all
   describe the same PR-fast vs merge-queue-full split, point to the
   unfiltered `profile ci` path, and explicitly say kind/k8s is not
   required.
3. **All 14 inventory tests are discoverable by the merge-queue
   command.**  Verified by `cargo nextest list --workspace --all-targets
   --all-features --profile ci` (3531 tests, all 14 inventory names
   present) and by two targeted `cargo nextest list -E` filters that
   return 11/11 djinn-agent and 3/3 djinn-control-plane tests.
4. **Workspace compiles + links under `profile ci` no-run** — the
   strongest local check available without Postgres.  Exit 0.
5. **Local execution blocker is scoped to worker-pod infrastructure,
   not kind/k8s or operator-only proof.**  The single local-execution
   attempt that was made failed with
   `Sqlx(Io(Os { code: 111, kind: ConnectionRefused }))` from
   `crates/djinn-agent/src/test_helpers.rs:114`, the exact "no Postgres
   on 127.0.0.1:5433" preflight the design called out.  The merge-queue
   `server-test` job supplies the missing service.  Stripped-down
   Djinn worker pods (this validator) intentionally do not carry
   Postgres, so this gap is expected and does not block ir2i closing.
6. **The pure-in-process static guard
   (`slot_pool_lifecycle_does_not_import_djinn_k8s_directly`) did run
   locally and passed in 0.04s**, providing a positive execution
   signal for at least one inventory test on every worker pod, with the
   rest gated on the merge-queue `server-test` job as designed.

These six points give the closing planner enough to confirm ir2i can
close once this task lands.
