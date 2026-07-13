# Quality-gate routing and cache operations

This runbook is the operating contract for the routed quality-gate roadmap (reference: `design/1udb-roadmap`). It describes the checked-in `quality-gate.yml` workflow; collect GitHub-run evidence when operating the workflow, but do not treat an external run link as a prerequisite for a repository change.

## Routing contract

`preflight` always runs first. It checks `scripts/ci-changed-scope.test.mjs`, computes an event-safe diff, runs `scripts/ci-changed-scope.mjs`, uploads `preflight/manifest.json`, and exposes these job outputs:

| Manifest lane | What it means | Output/job family |
| --- | --- | --- |
| `docs` | Documentation-only paths | no lane on ordinary PRs |
| `ui` | UI paths | `ui` / `ui-frontend` |
| `rustCore` | Server source | protected server outputs |
| `migrations` | PostgreSQL migration paths | `migrations` / `server-migrations-guard` |
| `sqlx` | `server/.sqlx` | `sqlxFreshness` / `server-sqlx-freshness` |
| `sandboxAarch64` | sandbox, workspace-hack, toolchain, or target configuration | `aarch64` / `server-aarch64-check` |
| `memoryRanking` | memory-eval or ranking code | `memoryEval` / `memory-eval` |
| `workflowCi` | workflow configuration | all outputs |
| `unknown` | unclassified executable/configuration path or unsafe/empty scope | all outputs (fail closed) |

The manifest outputs are `cargoDeny`, `clippy`, `aarch64`, `size`, `migrations`, `rawSql`, `capability`, `boundaries`, `serverTest`, `sqlxFreshness`, `memoryEval`, and `ui`. The protected server jobs are cargo-deny, clippy, size, migration, raw-SQL, capability, architectural-boundary, server-test, SQLx freshness, and memory evaluation. The server test plan publishes its matrix rows (stable shard ID, zero-based `shardIndex`, test IDs, and exact nextest filter) and exact-once proof alongside timing artifacts; timing data balances discovered tests only and never selects tests.

A documentation-only **pull request** intentionally has no selected product lane, so the aggregate can complete from preflight. A documentation-only **merge_group** is different: merge-queue safety selects cargo-deny, clippy, size/migration/raw-SQL/capability/boundary guards, server tests, SQLx freshness, and memory evaluation. UI remains unselected unless its lane (or full validation) is selected. `workflow_dispatch` and a full-validation fallback validate all lanes.

`quality-gate` is the protected aggregate check. It is fail closed: preflight must succeed, every selected job must report `success`, and an unselected job must report only `skipped`. A failed, cancelled, absent, or unexpected result is a gate failure; a neutral skip is acceptable only when the manifest explicitly did not select that work.

## Cache ownership policy

The only saving owners are deliberately isolated warmers:

| Cache family / shared key | Single saving owner | Restore-only consumers |
| --- | --- | --- |
| `server-quality` | `cache-warm-x86_64-quality` | clippy, SQLx freshness, memory evaluation, and any quality consumer |
| `server-test` | `cache-warm-x86_64-test` | all selected server-test shards |
| `server-aarch64-check` | `cache-warm-aarch64` | `server-aarch64-check` |

Consumers must use `Swatinem/rust-cache` with `save-if: false` (or an explicitly restore-only `actions/cache/restore` action). They must never become a fallback saver. The `cache-warm-aarch64` owner is reachable from both `push` to `main` and `workflow_dispatch`; this is necessary to recover an architecture-specific cache without opening a PR.

`node --test scripts/ci-cache-policy.test.mjs` is run by preflight. It statically rejects undeclared cache families, save-capable `actions/cache` use outside an owner, `rust-cache` actions without an explicit restore-only configuration for consumers, duplicate owners, missing owners, and an aarch64 owner that is not reachable from both main and dispatch. Update its explicit allowlist in the same change as any intentional new cache family.

## Timing restore, fallback, and recovery

The nextest planner accepts only a compatible `ci-nextest-timing/v1` timing artifact within its freshness window (seven days by default). It discards unknown/deleted test IDs and uses current `cargo nextest list` discovery as the sole test-selection authority. When no artifact is available, the version is incompatible, the data is stale, or no usable samples remain, it cold-starts deterministically with four shards and fallback duration estimates. The resulting plan/matrix/exact-once proof make the fallback auditable.

Each test shard uploads its current timing result and the workflow retains the plan, matrix, and proof artifacts. Recover from a bad or unavailable timing artifact by allowing the cold-start plan to run successfully, then use the newly uploaded compatible timing artifact for the next run. Do not hand-edit timing data to omit tests or change selection.

## Concurrency and admission

- **Pull requests:** group by PR number and cancel superseded runs.
- **Main pushes:** share the cancellable `ci-main` group. `main-admission` waits 90 seconds before expensive main warm work, allowing a newer push to replace a stale admission.
- **Merge groups:** group by candidate SHA/ref and do **not** cancel in progress; every merge-queue candidate receives its own complete decision.
- **Manual dispatch:** uses an isolated run-ID group and is cancellable only by its own invocation.

Do not compare queue-delayed runs with ordinary PR runs when measuring job wallclock: the main debounce and merge-group admission semantics are intentionally different.

## Operator procedures

### Verify aarch64 warming with two reproducible runs

1. Start a manual **workflow_dispatch** run from the same commit that selects the aarch64 lane (a full validation dispatch is suitable). Record the run URL, SHA, workflow event, cache key, runner OS, Rust version, and the `cache-warm-aarch64` log line containing `cache-family=server-aarch64-check`, `shared-key=server-aarch64-check`, and `cache-hit=`.
2. Wait for that warmer to finish successfully so its post-job save can complete. Do not use a cancelled run as evidence.
3. Dispatch the same SHA again with the same inputs. In the second `cache-warm-aarch64` and `server-aarch64-check` logs, record the same family/key/platform fields and the cache-hit status. The second run should report a restore for the compatible key; a miss requires recording the key/platform/toolchain difference before retrying.
4. Save a short redacted record containing both run IDs, timestamps, SHA, owner job conclusion, consumer conclusion, and the two cache-hit lines. This record is operational evidence, not a prerequisite to merging this repository task.

### Measure equivalent PR wallclock

1. Choose two PR runs with equivalent scope: the same routed lanes, same selected shard count/profile, same event (`pull_request`), and no unrelated retries, cancellations, or queue/main-admission delay. Prefer the same PR after a no-op re-run or two equivalent PRs touching the same lane.
2. Record run URL/ID, commit SHA, manifest outputs, selected jobs, cache-hit lines, start/finish timestamps, and total wallclock from GitHub's run summary. Exclude time before job start caused by runner queueing; report it separately if relevant.
3. Treat the first successful compatible-cache run as the cold/baseline observation and the subsequent comparable run as warm. Compute improvement as `(baseline wallclock - warm wallclock) / baseline wallclock * 100` and retain the raw timestamps.
4. If improvement is below **30%**, record the dominant remaining lane (for example server-test, clippy, SQLx freshness, or memory evaluation), its job duration, and the routing/cache facts that explain it. Open follow-up work against that dominant lane rather than weakening routing or allowing another cache saver.
