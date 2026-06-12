# Task-run backstop E2E evidence

## Final proposal 4369 residual-criteria reconciliation — 2026-06-12

This evidence pack reconciles the remaining proposal 4369 / epic 8451 criteria against the completed evidence tasks:

| Residual criterion | Evidence source | Reconciliation status |
| --- | --- | --- |
| Full Rust validation for the landed task-run teardown/backstop implementation. | `de12` (`019eb930-2164-7a23-ae8f-96d630fe1146`), `t2cs` (`019eb9c5-82a9-7e93-8ae5-cb2860fd792c`) | **Partially satisfied with an environmental blocker:** `cargo build` and strict `cargo clippy --all-features -- -D warnings` passed in `de12`. `t2cs` reran `cargo nextest run` after explicitly checking for the required Postgres path; Docker, `psql`, and `pg_isready` were absent, TCP `127.0.0.1:5433` refused connections, and nextest reached test execution then failed only in DB-backed tests with `Connection refused`. No teardown code defect was identified. |
| Real Kubernetes proof that `execution_kill_task` removes `djinn-taskrun-*` Pods/Jobs within roughly 60 seconds. | `9l2v` (`019eb930-5068-7213-b348-2c2c2d2d75f7`), section [`execution_kill_task` cleanup verification attempt](#execution_kill_task-cleanup-verification-attempt--blocked-by-worker-cluster-access). | **Not satisfied:** a Kubernetes-backed attempt was made, but the worker environment was missing `kubectl`/`KUBECONFIG`, lacked RBAC to read Pods/Jobs/logs, and lacked an authenticated MCP/control-plane path to invoke `execution_kill_task`. No before/after cleanup success is claimed. |
| Real Kubernetes proof that force-close/operator-close removes `djinn-taskrun-*` Pods/Jobs within roughly 60 seconds. | `6eg3` (`019eb930-803a-7033-9b7a-42678aac97a3`), section [Force-close/operator-close cleanup verification attempt](#force-closeoperator-close-cleanup-verification-attempt--blocked-by-worker-cluster-access). | **Not satisfied:** a Kubernetes-backed attempt was made, but the worker environment was missing `kubectl`/`KUBECONFIG`, lacked RBAC to read Pods/Jobs/logs, and available projected tokens were rejected by `/mcp` for operator/admin force-close. No force-close action was executed and no cleanup success is claimed. |

### Full validation outcome from `de12`

- **Validation task:** `de12` / `019eb930-2164-7a23-ae8f-96d630fe1146`
- **Closed:** 2026-06-12T02:11:38.572Z
- **Reviewer result:** approved. The reviewer confirmed the branch contained only a narrow diagnostic improvement and no generated/junk files.
- **Commands and outcomes recorded by `de12`:**
  - `cd server && cargo fmt`: passed; no substantive formatting diff beyond the intentional test assertion edit.
  - `cd server && cargo build`: **passed**; final run completed with `Finished dev profile ...` and exit 0.
  - `cd server && cargo clippy --all-features -- -D warnings`: **passed**; final run completed with `Finished dev profile ...` and exit 0.
  - `cd server && cargo nextest run`: **environmentally blocked after test compilation / during DB-backed test execution**. The required Postgres test service was unavailable on `127.0.0.1:5433`, and Docker was not installed so `docker compose up -d postgres-test` could not be used in that worker environment.
- **Fix made during validation:** `server/src/mcp_contract_tests/settings_tools.rs` was adjusted to print the full `settings_set` response when the DB-backed settings contract assertion fails, exposing the underlying DB/environment error. No task-run teardown architecture was changed.
- **Validation reconciliation:** build and strict clippy criteria are satisfied. The nextest criterion remains blocked by test infrastructure availability rather than by a known code defect; rerun `cd server && cargo nextest run` where `postgres-test` is running and reachable on `127.0.0.1:5433`.

### Follow-up nextest outcome from `t2cs`

- **Validation task:** `t2cs` / `019eb9c5-82a9-7e93-8ae5-cb2860fd792c`
- **Attempt timestamp (UTC):** 2026-06-12T03:38:48Z
- **Command:** `cd server && cargo nextest run`
- **Preflight outcome:** the worker still could not provide the required Postgres test service. `docker` was not installed, so `docker compose up -d postgres-test` could not start the repository's `postgres-test` service. `psql` and `pg_isready` were also not installed, and a direct TCP probe to `127.0.0.1:5433` returned `Connection refused`.
- **Nextest outcome:** environmentally blocked. The command compiled the test profile successfully, started nextest (`Nextest run ID 76807dfc-1ec3-40c3-a1da-005a18ba5ca2`), and ran 97 of 247 tests before fail-fast cancellation. The summary was `97/247 tests run: 92 passed, 5 failed, 2 skipped`; all five failures were DB-backed tests whose stderr showed `Connection refused` while creating or using a test project/database.
- **Fixes made:** none. The failures are consistent with the missing Postgres infrastructure and do not indicate a task-run teardown code defect.
- **Validation reconciliation:** the proposal validation criterion is **not yet satisfied** for nextest. The remaining blocker is operational: rerun `cd server && cargo nextest run` where Postgres is reachable at `127.0.0.1:5433`, or where Docker is available to run `docker compose up -d postgres-test` plus the documented test-template setup.

Exact command output excerpt from `t2cs`:

```console
$ command -v docker || true

$ docker compose up -d postgres-test
bash: line 12: docker: command not found
docker compose exit=127

$ command -v psql || true

$ command -v pg_isready || true

$ bash -lc '</dev/tcp/127.0.0.1/5433'
bash: connect: Connection refused
bash: line 1: /dev/tcp/127.0.0.1/5433: Connection refused
tcp 127.0.0.1:5433 unreachable

$ cd server && cargo nextest run
    Finished `test` profile [unoptimized + debuginfo] target(s) in 7m 39s
────────────
 Nextest run ID 76807dfc-1ec3-40c3-a1da-005a18ba5ca2 with nextest profile: default
    Starting 247 tests across 2 binaries (2 tests skipped)
        FAIL [   0.059s] ( 92/247) djinn-server mcp_contract_tests::session_tools::session_active_returns_error_without_pool
  stderr ───

    thread 'mcp_contract_tests::session_tools::session_active_returns_error_without_pool' (1322) panicked at src/test_helpers.rs:96:10:
    failed to create test project: Sqlx(Io(Os { code: 111, kind: ConnectionRefused, message: "Connection refused" }))

        FAIL [   0.101s] ( 93/247) djinn-server mcp_contract_tests::session_tools::session_for_task_returns_error_without_pool
  stderr ───

    thread 'mcp_contract_tests::session_tools::session_for_task_returns_error_without_pool' (1319) panicked at src/test_helpers.rs:96:10:
    failed to create test project: Sqlx(Io(Os { code: 111, kind: ConnectionRefused, message: "Connection refused" }))

        FAIL [   0.122s] ( 94/247) djinn-server mcp_contract_tests::project_tools::project_remove_success_and_missing
  stderr ───

    thread 'mcp_contract_tests::project_tools::project_remove_success_and_missing' (1315) panicked at src/test_helpers.rs:113:10:
    failed to create test project: Sqlx(Io(Os { code: 111, kind: ConnectionRefused, message: "Connection refused" }))

        FAIL [   0.127s] ( 95/247) djinn-server mcp_contract_tests::board_tools::board_reconcile_releases_stuck_in_progress_without_active_session
  stderr ───

    thread 'mcp_contract_tests::board_tools::board_reconcile_releases_stuck_in_progress_without_active_session' (1325) panicked at src/test_helpers.rs:96:10:
    failed to create test project: Sqlx(Io(Os { code: 111, kind: ConnectionRefused, message: "Connection refused" }))

        FAIL [   0.293s] ( 97/247) djinn-server mcp_contract_tests::settings_tools::settings_set_get_reset_round_trip
  stderr ───

    thread 'mcp_contract_tests::settings_tools::settings_set_get_reset_round_trip' (1320) panicked at src/mcp_contract_tests/settings_tools.rs:27:5:
    assertion `left == right` failed: settings_set response: {"ok":false,"applied":false,"error":"database error: error communicating with database: Connection refused (os error 111)"}
      left: Bool(false)
     right: true

────────────
     Summary [   0.940s] 97/247 tests run: 92 passed, 5 failed, 2 skipped
        FAIL [   0.059s] ( 92/247) djinn-server mcp_contract_tests::session_tools::session_active_returns_error_without_pool
        FAIL [   0.101s] ( 93/247) djinn-server mcp_contract_tests::session_tools::session_for_task_returns_error_without_pool
        FAIL [   0.122s] ( 94/247) djinn-server mcp_contract_tests::project_tools::project_remove_success_and_missing
        FAIL [   0.127s] ( 95/247) djinn-server mcp_contract_tests::board_tools::board_reconcile_releases_stuck_in_progress_without_active_session
        FAIL [   0.293s] ( 97/247) djinn-server mcp_contract_tests::settings_tools::settings_set_get_reset_round_trip
warning: 150/247 tests were not run due to test failure (run with --no-fail-fast to run all tests, or run with --max-fail)
error: test run failed
cargo nextest exit=100
```

### Kubernetes evidence outcome from `9l2v` (`execution_kill_task`)

- **Evidence task:** `9l2v` / `019eb930-5068-7213-b348-2c2c2d2d75f7`
- **Worker task_run_id:** `019eb963-d937-7930-9e68-6e14953792d9`
- **Attempt window:** 2026-06-12T01:13:33Z through 2026-06-12T01:14:40Z
- **Namespace/context evidence:** namespace `djinn`; Kubernetes API reachable through the mounted service-account token; identity was `system:serviceaccount:djinn:djinn-djinn-taskrun`.
- **Pre-kill evidence:** documented below under [Documented pre-kill Kubernetes checks](#documented-pre-kill-kubernetes-checks) and [Direct in-cluster Kubernetes API fallback](#direct-in-cluster-kubernetes-api-fallback). The intended `kubectl get jobs,pods ... -l djinn.app/task-run-id=019eb963-d937-7930-9e68-6e14953792d9` commands failed because `kubectl` was not installed. Direct API fallback showed the API was reachable, then returned 403 for listing Pods, listing Jobs, and getting `jobs.batch/djinn-taskrun-019eb963-d937-7930-9e68-6e14953792d9`.
- **Kill invocation evidence:** documented below under [`execution_kill_task` invocation path](#execution_kill_task-invocation-path). `/mcp` initialization with the projected Djinn token returned `authentication required`, so `execution_kill_task` could not be invoked from the worker.
- **Post-kill/log evidence:** documented below under [Post-kill polling and logs](#post-kill-polling-and-logs). No post-kill polling or server/coordinator logs were captured because resource reads, logs, and authenticated kill invocation were blocked.
- **Reconciliation:** the attempt is valid blocker evidence, but the proposal criterion requiring actual before/after cleanup proof is **not satisfied**.

### Kubernetes evidence outcome from `6eg3` (force-close/operator-close)

- **Evidence task:** `6eg3` / `019eb930-803a-7033-9b7a-42678aac97a3`
- **Worker task_run_id:** `019eb9a2-bc65-7e71-a61e-026dc12e1516`
- **Attempt window:** 2026-06-12T02:22:27Z through 2026-06-12T02:22:28Z
- **Namespace/context evidence:** namespace `djinn`; Kubernetes API endpoint `https://kubernetes.default.svc:443` (`KUBERNETES_SERVICE_HOST=10.43.0.1`, `KUBERNETES_SERVICE_PORT=443`); identity was `system:serviceaccount:djinn:djinn-djinn-taskrun`; Kubernetes API discovery returned `APIVersions` for `v1`.
- **Pre-force-close evidence:** documented below under [Documented pre-force-close Kubernetes checks](#documented-pre-force-close-kubernetes-checks) and the force-close [Direct in-cluster Kubernetes API fallback](#direct-in-cluster-kubernetes-api-fallback-1). Intended `kubectl get jobs,pods ... -l djinn.app/task-run-id=019eb9a2-bc65-7e71-a61e-026dc12e1516` commands failed because `kubectl` was not installed. Direct API fallback returned 403 for listing Pods, listing Jobs, and getting `jobs.batch/djinn-taskrun-019eb9a2-bc65-7e71-a61e-026dc12e1516`.
- **Force-close action evidence:** documented below under [Force-close/operator-close action path](#force-closeoperator-close-action-path). Both the projected Djinn token and Kubernetes service-account token returned HTTP 401 `authentication required` from `/mcp`, so no authenticated operator/admin/proposal-abort/force-close action was executed.
- **Post-force-close/log evidence:** documented below under [Post-force-close polling and logs](#post-force-close-polling-and-logs). No post-force-close polling or server/coordinator logs were captured because resource reads, logs, and authenticated force-close invocation were blocked.
- **Reconciliation:** the attempt is valid blocker evidence, but the proposal criterion requiring actual before/after cleanup proof is **not satisfied**.

### Epic 8451 closure recommendation

Epic 8451 should **remain open** for a follow-up wave. The exact residual blockers are:

1. Run `cd server && cargo nextest run` in an environment with the required Postgres test service reachable on `127.0.0.1:5433` (or with Docker available to start `postgres-test`) and record the final pass/fail result.
2. Run `docs/TASKRUN_BACKSTOP_VERIFICATION.md` from an operator environment with `kubectl`, a configured `djinn` context, RBAC to list/get Pods and Jobs plus read `deploy/djinn-server` logs, and authenticated Djinn control-plane/MCP access to invoke `execution_kill_task`.
3. Run the same verification for force-close/operator-close with authenticated operator/admin/proposal-abort capability.

Once those three residual items pass, the next Planner can close epic 8451. Until then, proposal 4369 residual Kubernetes cleanup proof criteria are not met, and the nextest criterion has only blocker evidence rather than a successful full pass.


## `execution_kill_task` cleanup verification attempt — blocked by worker cluster access

- **Attempt timestamp (UTC):** 2026-06-12T01:13:33Z through 2026-06-12T01:14:40Z
- **Worker task under which the verification was attempted:** `019eb930-5068-7213-b348-2c2c2d2d75f7` (`9l2v`)
- **Worker task_run_id:** `019eb963-d937-7930-9e68-6e14953792d9`
- **In-cluster namespace:** `djinn` (from `/var/run/secrets/kubernetes.io/serviceaccount/namespace`)
- **Kubernetes identity:** `system:serviceaccount:djinn:djinn-djinn-taskrun` (reported by the Kubernetes API in RBAC denials)
- **Kubernetes API reachability:** reachable from the worker via the mounted service account token; `/api` returned `APIVersions` for `v1`.
- **Control-plane endpoint reachability:** `http://djinn-server.djinn.svc.cluster.local:3000/health` returned HTTP 200. The health payload included a Postgres database target with credentials, which are intentionally not copied here.

### Result

A real Kubernetes-backed verification was attempted, but the task-run worker environment does **not** have the access needed to perform the documented runbook. No cleanup result is claimed.

The blockers were:

1. `kubectl` is not installed in the worker image.
2. `KUBECONFIG` is unset.
3. The in-cluster service account can reach the Kubernetes API but is forbidden from listing or getting Pods/Jobs in namespace `djinn`, including the canonical `djinn-taskrun-$TASK_RUN_ID` Job.
4. The worker's projected Djinn token was not accepted by `/mcp`; MCP `initialize` returned `authentication required`, so this session could not invoke `execution_kill_task` through the documented control-plane path.
5. Because Pods/Jobs/logs could not be read, no long-running task-run resource could be proven present before kill and no server/coordinator logs could be captured. The current session's own `TASK_RUN_ID` is recorded above only as the active Kubernetes-backed task-run visible to the dispatcher; it was not killed.

### Commands attempted and observed output

#### Baseline context

```console
$ date -u +%Y-%m-%dT%H:%M:%SZ
2026-06-12T01:13:33Z

$ command -v kubectl || true

$ kubectl version --client=true
bash: line 6: kubectl: command not found

$ kubectl config current-context
bash: line 7: kubectl: command not found

$ kubectl config get-contexts
bash: line 8: kubectl: command not found

$ kubectl get ns
bash: line 9: kubectl: command not found

$ printf '%s\n' "${KUBECONFIG:-<unset>}"
<unset>

$ cat /var/run/secrets/kubernetes.io/serviceaccount/namespace
djinn
```

#### Documented pre-kill Kubernetes checks

```console
$ NS=djinn TASK_RUN_ID=019eb963-d937-7930-9e68-6e14953792d9 \
  kubectl get jobs,pods -n "$NS" \
    -l "djinn.app/task-run-id=$TASK_RUN_ID" -o wide
bash: line 4: kubectl: command not found

$ NS=djinn TASK_RUN_ID=019eb963-d937-7930-9e68-6e14953792d9 \
  kubectl get pods -n "$NS" \
    --field-selector=status.phase=Running \
    -l "djinn.app/task-run-id=$TASK_RUN_ID" -o name
bash: line 6: kubectl: command not found
```

#### Direct in-cluster Kubernetes API fallback

The mounted service account token can authenticate to the API server for discovery:

```console
$ curl --cacert /var/run/secrets/kubernetes.io/serviceaccount/ca.crt \
  -H 'Authorization: Bearer <redacted service-account token>' \
  https://kubernetes.default.svc:443/api
{
  "kind": "APIVersions",
  "versions": [
    "v1"
  ],
  "serverAddressByClientCIDRs": [
    {
      "clientCIDR": "0.0.0.0/0",
      "serverAddress": "13.140.147.151:6443"
    }
  ]
}
```

But the same identity is RBAC-blocked from the resources required by the runbook:

```console
$ curl --cacert /var/run/secrets/kubernetes.io/serviceaccount/ca.crt \
  -H 'Authorization: Bearer <redacted service-account token>' \
  'https://kubernetes.default.svc:443/api/v1/namespaces/djinn/pods?labelSelector=djinn.app%2Ftask-run-id%3D019eb963-d937-7930-9e68-6e14953792d9'
{
  "kind": "Status",
  "apiVersion": "v1",
  "metadata": {},
  "status": "Failure",
  "message": "pods is forbidden: User \"system:serviceaccount:djinn:djinn-djinn-taskrun\" cannot list resource \"pods\" in API group \"\" in the namespace \"djinn\"",
  "reason": "Forbidden",
  "details": { "kind": "pods" },
  "code": 403
}

$ curl --cacert /var/run/secrets/kubernetes.io/serviceaccount/ca.crt \
  -H 'Authorization: Bearer <redacted service-account token>' \
  'https://kubernetes.default.svc:443/apis/batch/v1/namespaces/djinn/jobs?labelSelector=djinn.app%2Ftask-run-id%3D019eb963-d937-7930-9e68-6e14953792d9'
{
  "kind": "Status",
  "apiVersion": "v1",
  "metadata": {},
  "status": "Failure",
  "message": "jobs.batch is forbidden: User \"system:serviceaccount:djinn:djinn-djinn-taskrun\" cannot list resource \"jobs\" in API group \"batch\" in the namespace \"djinn\"",
  "reason": "Forbidden",
  "details": { "group": "batch", "kind": "jobs" },
  "code": 403
}

$ curl --cacert /var/run/secrets/kubernetes.io/serviceaccount/ca.crt \
  -H 'Authorization: Bearer <redacted service-account token>' \
  'https://kubernetes.default.svc:443/apis/batch/v1/namespaces/djinn/jobs/djinn-taskrun-019eb963-d937-7930-9e68-6e14953792d9'
{
  "kind": "Status",
  "apiVersion": "v1",
  "metadata": {},
  "status": "Failure",
  "message": "jobs.batch \"djinn-taskrun-019eb963-d937-7930-9e68-6e14953792d9\" is forbidden: User \"system:serviceaccount:djinn:djinn-djinn-taskrun\" cannot get resource \"jobs\" in API group \"batch\" in the namespace \"djinn\"",
  "reason": "Forbidden",
  "details": {
    "name": "djinn-taskrun-019eb963-d937-7930-9e68-6e14953792d9",
    "group": "batch",
    "kind": "jobs"
  },
  "code": 403
}
```

#### `execution_kill_task` invocation path

The server is reachable, but the worker-projected Djinn token did not authorize MCP tool calls:

```console
$ curl -H 'Authorization: Bearer <redacted /var/run/secrets/tokens/djinn token>' \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"taskrun-backstop-evidence-worker","version":"0"}}}' \
  http://djinn-server.djinn.svc.cluster.local:3000/mcp
authentication required
```

Because MCP authentication failed, this worker could not safely invoke:

```json
{
  "tool": "execution_kill_task",
  "arguments": {
    "task_id": "<long-running-task-id>",
    "reason": "task-run backstop e2e verification"
  }
}
```

#### Post-kill polling and logs

No post-kill polling evidence or coordinator/server cleanup logs were captured, because the pre-kill Kubernetes reads and `execution_kill_task` invocation were blocked as shown above. The documented log command also could not run because `kubectl` is absent:

```console
$ NS=djinn TASK_RUN_ID=019eb963-d937-7930-9e68-6e14953792d9 \
  kubectl logs -n "$NS" deploy/djinn-server --since=10m | \
  grep -E "task-run Job backstop|backstop reaped orphaned task-run Job|task_run_id=$TASK_RUN_ID"
bash: line 1: kubectl: command not found
```

### Follow-up needed for passing evidence

Run `docs/TASKRUN_BACKSTOP_VERIFICATION.md` from an operator environment that has:

- `kubectl` installed and configured for the `djinn` namespace/context;
- RBAC to list/get Pods and Jobs and read `deploy/djinn-server` logs;
- an authenticated Djinn MCP/control-plane session allowed to call `execution_kill_task` on a deliberately long-running worker task.

Only that environment can produce the required before/after proof that `djinn-taskrun-$TASK_RUN_ID` resources disappear within roughly 60 seconds.

## Force-close/operator-close cleanup verification attempt — blocked by worker cluster access

- **Attempt timestamp (UTC):** 2026-06-12T02:22:27Z through 2026-06-12T02:22:28Z
- **Worker task under which the verification was attempted:** `019eb930-803a-7033-9b7a-42678aac97a3` (`6eg3`)
- **Worker task_run_id:** `019eb9a2-bc65-7e71-a61e-026dc12e1516`
- **In-cluster namespace:** `djinn` (from `/var/run/secrets/kubernetes.io/serviceaccount/namespace`)
- **Kubernetes API endpoint:** `https://kubernetes.default.svc:443` (`KUBERNETES_SERVICE_HOST=10.43.0.1`, `KUBERNETES_SERVICE_PORT=443`)
- **Kubernetes identity:** `system:serviceaccount:djinn:djinn-djinn-taskrun` (reported by the Kubernetes API in RBAC denials)
- **Kubernetes API reachability:** reachable from the worker via the mounted service account token; `/api` returned `APIVersions` for `v1`.
- **Control-plane endpoint reachability:** `http://djinn-server.djinn.svc.cluster.local:3000/health` returned HTTP 200. The health payload included a Postgres database target with credentials, which are intentionally redacted below.

### Result

A real Kubernetes-backed force-close/operator-close verification was attempted from the active task-run worker environment, but the worker does **not** have the access needed to perform the documented force-close runbook. No cleanup success is claimed.

The blockers were:

1. `kubectl` is not installed in the worker image.
2. `KUBECONFIG` is unset.
3. The in-cluster service account can reach the Kubernetes API but is forbidden from listing Pods, listing Jobs, or getting the canonical `djinn-taskrun-$TASK_RUN_ID` Job in namespace `djinn`.
4. The worker's projected Djinn token and Kubernetes service-account token were not accepted by `/mcp`; MCP `initialize` returned HTTP 401 `authentication required`, so this session could not invoke an authenticated operator/admin/proposal-abort/force-close action through the documented control-plane path.
5. Self-submitting this evidence task was not used as a substitute for force-close: it is a normal worker completion path, not an operator/admin/proposal-abort force-close, and it would end the evidence session before the required 60-second Kubernetes polling and log capture could be performed.
6. Because Pods/Jobs/logs could not be read, no task-run resource could be proven present before force-close, no post-force-close polling could be captured, and no server/coordinator cleanup logs could be captured. The current session's own `TASK_RUN_ID` is recorded above only as the active Kubernetes-backed task-run visible to the dispatcher; it was not force-closed.

### Commands attempted and observed output

#### Baseline context

```console
$ date -u +%Y-%m-%dT%H:%M:%SZ
2026-06-12T02:22:27Z

$ printf '%s\n' "${KUBECONFIG:-<unset>}"
<unset>

$ printf '%s %s\n' "$KUBERNETES_SERVICE_HOST" "$KUBERNETES_SERVICE_PORT"
10.43.0.1 443

$ cat /var/run/secrets/kubernetes.io/serviceaccount/namespace
djinn

$ command -v kubectl || true

$ kubectl version --client=true
bash: line 13: kubectl: command not found

$ kubectl config current-context
bash: line 14: kubectl: command not found
```

#### Documented pre-force-close Kubernetes checks

The documented `kubectl` checks could not run because `kubectl` is absent:

```console
$ NS=djinn TASK_RUN_ID=019eb9a2-bc65-7e71-a61e-026dc12e1516 \
  kubectl get jobs,pods -n "$NS" \
    -l "djinn.app/task-run-id=$TASK_RUN_ID" -o wide
bash: kubectl: command not found

$ NS=djinn TASK_RUN_ID=019eb9a2-bc65-7e71-a61e-026dc12e1516 \
  kubectl get pods -n "$NS" \
    --field-selector=status.phase=Running \
    -l "djinn.app/task-run-id=$TASK_RUN_ID" -o name
bash: kubectl: command not found
```

#### Direct in-cluster Kubernetes API fallback

The mounted service account token can authenticate to the API server for discovery:

```console
$ curl --cacert /var/run/secrets/kubernetes.io/serviceaccount/ca.crt \
  -H 'Authorization: Bearer <redacted service-account token>' \
  https://kubernetes.default.svc:443/api
{
  "kind": "APIVersions",
  "versions": [
    "v1"
  ],
  "serverAddressByClientCIDRs": [
    {
      "clientCIDR": "0.0.0.0/0",
      "serverAddress": "13.140.147.151:6443"
    }
  ]
}
```

But the same identity is RBAC-blocked from the resources required by the runbook:

```console
$ curl --cacert /var/run/secrets/kubernetes.io/serviceaccount/ca.crt \
  -H 'Authorization: Bearer <redacted service-account token>' \
  'https://kubernetes.default.svc:443/api/v1/namespaces/djinn/pods?labelSelector=djinn.app%2Ftask-run-id%3D019eb9a2-bc65-7e71-a61e-026dc12e1516'
{
  "kind": "Status",
  "apiVersion": "v1",
  "metadata": {},
  "status": "Failure",
  "message": "pods is forbidden: User \"system:serviceaccount:djinn:djinn-djinn-taskrun\" cannot list resource \"pods\" in API group \"\" in the namespace \"djinn\"",
  "reason": "Forbidden",
  "details": { "kind": "pods" },
  "code": 403
}

$ curl --cacert /var/run/secrets/kubernetes.io/serviceaccount/ca.crt \
  -H 'Authorization: Bearer <redacted service-account token>' \
  'https://kubernetes.default.svc:443/apis/batch/v1/namespaces/djinn/jobs?labelSelector=djinn.app%2Ftask-run-id%3D019eb9a2-bc65-7e71-a61e-026dc12e1516'
{
  "kind": "Status",
  "apiVersion": "v1",
  "metadata": {},
  "status": "Failure",
  "message": "jobs.batch is forbidden: User \"system:serviceaccount:djinn:djinn-djinn-taskrun\" cannot list resource \"jobs\" in API group \"batch\" in the namespace \"djinn\"",
  "reason": "Forbidden",
  "details": { "group": "batch", "kind": "jobs" },
  "code": 403
}

$ curl --cacert /var/run/secrets/kubernetes.io/serviceaccount/ca.crt \
  -H 'Authorization: Bearer <redacted service-account token>' \
  'https://kubernetes.default.svc:443/apis/batch/v1/namespaces/djinn/jobs/djinn-taskrun-019eb9a2-bc65-7e71-a61e-026dc12e1516'
{
  "kind": "Status",
  "apiVersion": "v1",
  "metadata": {},
  "status": "Failure",
  "message": "jobs.batch \"djinn-taskrun-019eb9a2-bc65-7e71-a61e-026dc12e1516\" is forbidden: User \"system:serviceaccount:djinn:djinn-djinn-taskrun\" cannot get resource \"jobs\" in API group \"batch\" in the namespace \"djinn\"",
  "reason": "Forbidden",
  "details": {
    "name": "djinn-taskrun-019eb9a2-bc65-7e71-a61e-026dc12e1516",
    "group": "batch",
    "kind": "jobs"
  },
  "code": 403
}
```

#### Force-close/operator-close action path

The server is reachable, but the available worker-projected credentials did not authorize MCP tool calls:

```console
$ curl -i -H 'Authorization: Bearer <redacted /var/run/secrets/tokens/djinn token>' \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"taskrun-backstop-force-close-evidence-worker","version":"0"}}}' \
  http://djinn-server.djinn.svc.cluster.local:3000/mcp
HTTP/1.1 401 Unauthorized
content-type: text/plain; charset=utf-8
www-authenticate: Bearer resource_metadata="https://code.djinnai.io/.well-known/oauth-protected-resource/mcp"

authentication required

$ curl -i -H 'Authorization: Bearer <redacted Kubernetes service-account token>' \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"taskrun-backstop-force-close-evidence-worker","version":"0"}}}' \
  http://djinn-server.djinn.svc.cluster.local:3000/mcp
HTTP/1.1 401 Unauthorized
content-type: text/plain; charset=utf-8
www-authenticate: Bearer resource_metadata="https://code.djinnai.io/.well-known/oauth-protected-resource/mcp"

authentication required
```

Because MCP authentication failed, this worker could not safely invoke or verify an operator/admin/proposal-abort/force-close action. The force-close action used for this attempt is therefore: **none executed; blocked before action by missing authenticated operator/admin control-plane access and missing Kubernetes read/log RBAC**.

#### Post-force-close polling and logs

No post-force-close polling evidence or coordinator/server cleanup logs were captured, because the pre-force-close Kubernetes reads and authenticated force-close invocation were blocked as shown above. The documented log command also could not run because `kubectl` is absent:

```console
$ NS=djinn TASK_RUN_ID=019eb9a2-bc65-7e71-a61e-026dc12e1516 \
  kubectl logs -n "$NS" deploy/djinn-server --since=10m | \
  grep -E "task-run Job backstop|backstop reaped orphaned task-run Job|task_run_id=$TASK_RUN_ID"
bash: kubectl: command not found
```

### Follow-up needed for passing force-close evidence

Run the force-close section of `docs/TASKRUN_BACKSTOP_VERIFICATION.md` from an operator environment that has:

- `kubectl` installed and configured for the `djinn` namespace/context;
- RBAC to list/get Pods and Jobs and read `deploy/djinn-server` logs;
- an authenticated Djinn operator/admin/control-plane session allowed to force-close a deliberately long-running worker task or abort a proposal in a way that force-closes its active task run.

Only that environment can produce the required before/after proof that `djinn-taskrun-$TASK_RUN_ID` resources disappear within roughly 60 seconds after force-close/operator-close.
