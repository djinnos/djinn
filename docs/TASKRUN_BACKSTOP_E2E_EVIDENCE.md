# Task-run backstop E2E evidence

## Wave 3 Postgres-backed full validation entrypoint

A reproducible manual/CI entrypoint now exists for the remaining proposal 4369 / epic 8451 Rust validation blocker:

```bash
make validate-taskrun-backstop
# equivalent direct form:
./scripts/validate-taskrun-backstop.sh
```

The entrypoint provisions the repository's `docker-compose.yml` `postgres-test` service on `127.0.0.1:5433`, applies `djinn-db` Postgres migrations, rebuilds `djinn_test_template`, creates the test vault key at `/var/tmp/djinn-test-vault/vault.key`, configures a test git identity, then runs the full validation command sequence:

```bash
docker compose -f docker-compose.yml up -d postgres-test
# reset postgres-test, apply djinn-db migrations, and rebuild djinn_test_template
# create /var/tmp/djinn-test-vault/vault.key
cd server && cargo build
cd server && cargo clippy --workspace --all-targets --all-features -- -D warnings
cd server && cargo nextest run --workspace --all-targets --all-features
```

- **Evidence timestamp (UTC):** 2026-06-12T11:26:21Z
- **Attempted command:** `sh -n scripts/validate-taskrun-backstop.sh && ./scripts/validate-taskrun-backstop.sh`
- **Local result:** **blocked by environment before Postgres provisioning**. The script syntax check passed, but this task-run worker does not have Docker installed, so the validation entrypoint stopped before starting `postgres-test`, building `djinn_test_template`, or running Cargo validation. A separate dry-run attempt of the Makefile target also showed `make: command not found` in this worker. These are environmental blockers, not task-run teardown code/test defects.
- **Proposal validation criterion:** **not yet satisfied by a passing run**. It is now reproducible: rerun `make validate-taskrun-backstop` or `./scripts/validate-taskrun-backstop.sh` on a host with Docker Compose, `sqlx-cli`, `cargo-nextest`, Cargo, and OpenSSL available. The Makefile form additionally requires Make. A passing run of either command satisfies the full Rust validation portion of epic 8451.

Captured local output:

```console
$ sh -n scripts/validate-taskrun-backstop.sh && ./scripts/validate-taskrun-backstop.sh

[2026-06-12T11:26:21Z] Task-run backstop validation started

[2026-06-12T11:26:21Z] Repository: /workspace/.tmp5yxXK4

[2026-06-12T11:26:21Z] Log file: /workspace/.tmp5yxXK4/.taskrun-backstop-validation/validation-20260612T112621Z.log
ERROR: required command not found: docker

[2026-06-12T11:26:21Z] Task-run backstop validation failed with exit=127; see /workspace/.tmp5yxXK4/.taskrun-backstop-validation/validation-20260612T112621Z.log
```

## Wave 2 final consolidation status

Wave 2 did **not** clear all residual proposal 4369 / epic 8451 criteria. The operator preflight helper from `7jbf` is present and documents the exact evidence bundle required from a real operator/admin shell, but the subsequent Wave 2 runs still executed in worker environments that lacked the required infrastructure and access. The residual status is:

- **Nextest validation (`t2cs`): not satisfied; infrastructure blocker.** Exact command `cd server && cargo nextest run` was attempted at 2026-06-12T03:38:48Z. The run compiled tests and started nextest, but the worker lacked Docker/`postgres-test`, `psql`, and `pg_isready`; TCP `127.0.0.1:5433` refused connections. Result: `97/247 tests run: 92 passed, 5 failed, 2 skipped`, with all failures showing Postgres `Connection refused`. No teardown code defect was identified.
- **`execution_kill_task` Kubernetes cleanup proof (`9eps`): not satisfied; operator access blocker.** Attempt window 2026-06-12T04:07:22Z through 2026-06-12T04:07:55Z for task `019eb9c5-d90f-78e0-95aa-edca757567c0`, active task_run_id `019eba02-e846-76f1-874e-35d1985a2a36`, namespace `djinn`. `kubectl` was absent, `KUBECONFIG` was unset, no operator MCP endpoint/token was configured, and the only worker token returned HTTP 401 from `/mcp`; therefore no before-kill Kubernetes listing, kill invocation, after-kill polling, or server/coordinator logs could be captured.
- **Force-close/operator-close Kubernetes cleanup proof (`mqt8`): not satisfied; operator access blocker.** Attempt window 2026-06-12T04:33:26Z through 2026-06-12T04:33:28Z for task `019eb9c6-04bd-74c0-90e1-7e4b4ea6e23c`, active task_run_id `019eba1a-4dbe-7692-9269-82d234a888a8`, namespace `djinn`. `kubectl` was absent, `KUBECONFIG` was unset, the worker service account was forbidden from reading Pods/Jobs/canonical Jobs, no operator MCP endpoint/token was configured, and both projected tokens returned HTTP 401 from `/mcp`; therefore no force-close action, before/after polling, or server/coordinator logs could be captured.

**Epic 8451 closure recommendation:** epic 8451 should **remain open**. The exact residual blockers are infrastructure/access blockers, not known task-run teardown code defects: provide Postgres on `127.0.0.1:5433` (or Docker to start `postgres-test`) for nextest, and rerun both cleanup checks from an operator/admin environment with `kubectl`, Pods/Jobs/log RBAC in namespace `djinn`, and authenticated Djinn MCP/control-plane credentials.

## Operator preflight evidence required before Wave 2 cleanup checks

Wave 1 kill and force-close evidence attempts were blocked by the normal task-run worker environment: `kubectl` was not installed, `KUBECONFIG` was unset, the worker service account lacked RBAC to list/get Pods and Jobs or read server logs, and projected worker tokens were rejected by `/mcp` for operator/admin actions. Passing proof therefore requires an operator/admin environment rather than another normal task-run worker.

Before running the real `execution_kill_task` or force-close cleanup checks, run the helper from the repository root in the operator/admin shell:

```bash
export NS=djinn
export DJINN_MCP_URL="https://<operator-accessible-djinn-host>/mcp"
export DJINN_OPERATOR_BEARER_TOKEN="<operator/admin token>"
./scripts/taskrun-backstop-preflight.sh | tee taskrun-backstop-preflight.md
```

Paste the full generated Markdown bundle below this paragraph, before the kill/force-close evidence subsections. The bundle must include the exact command output for:

- `kubectl version --client=true`;
- `kubectl config current-context` and the current-context namespace;
- `kubectl get namespace "$NS" -o name`;
- RBAC checks proving list/get access for Pods and Jobs in `$NS`;
- `kubectl get pods -n "$NS" -o name --request-timeout=10s`;
- `kubectl get jobs -n "$NS" -o name --request-timeout=10s`;
- `kubectl logs -n "$NS" deploy/djinn-server --since=10m --tail=20` (or the configured `DJINN_SERVER_DEPLOY` target);
- an authenticated MCP `initialize` response from the same operator/admin credential that will be used for kill and force-close actions.

Redact secrets before committing or pasting the bundle: bearer tokens, kubeconfig client keys/certificates, cookies, database URLs, and full health payloads containing credentials. Keep non-secret context, namespace, RBAC, HTTP status/error text, and Kubernetes resource names intact so reviewers can distinguish an environment blocker from a cleanup defect.

> Paste Wave 2 operator preflight output here. A preflight pass only proves the evidence environment is ready; it does **not** claim task-run cleanup success.

### Wave 2 operator preflight attempt from `9eps` — failed before cleanup action

- **Evidence task:** `9eps` / `019eb9c5-d90f-78e0-95aa-edca757567c0`
- **Active worker task_run_id:** `019eba02-e846-76f1-874e-35d1985a2a36`
- **Attempt window (UTC):** 2026-06-12T04:07:22Z through 2026-06-12T04:07:55Z
- **Namespace/context:** intended namespace `djinn`; no Kubernetes context could be read because `kubectl` is not installed and `KUBECONFIG` is unset in this worker environment. The in-cluster namespace file reports `djinn`.
- **Preflight result:** **failed**. This session did not have an operator/admin shell: `kubectl` was absent, no operator `DJINN_MCP_URL` or `DJINN_OPERATOR_BEARER_TOKEN` was configured, and the worker-projected Djinn token was rejected by `/mcp` with HTTP 401.
- **Cleanup action:** **not invoked**. Per the runbook, the real `execution_kill_task` check must stop when the Wave 2 operator preflight fails; killing from this worker would not be an operator/admin verification and would not allow the required before/after Kubernetes polling or server-log capture.
- **Reconciliation:** the `execution_kill_task` Kubernetes proof criterion remains **not satisfied**. This is blocker evidence only; no cleanup success is claimed.

Complete preflight output captured from this session:

````console
$ date -u +%Y-%m-%dT%H:%M:%SZ; pwd; env | grep -E '^(NS|DJINN|KUBE|KUBECONFIG)=' | sed -E 's/(TOKEN|SECRET|PASSWORD)=.*/\1=<redacted>/' || true; ./scripts/taskrun-backstop-preflight.sh
2026-06-12T04:07:22Z
/workspace/.tmpSptaoa
# Task-run backstop operator preflight evidence

- **Captured at (UTC):** `2026-06-12T04:07:22Z`
- **Namespace checked:** `djinn`
- **Server log target:** `deploy/djinn-server`
- **Djinn MCP endpoint:** `<unset DJINN_MCP_URL>`
- **Scope:** preflight only. This bundle proves the operator shell has the ingredients required for the real kill/force-close cleanup checks; it does **not** claim task-run cleanup success.
- **Redaction:** do not paste bearer tokens, kubeconfig client keys/certificates, cookies, database URLs, or full health payloads containing credentials. This script redacts the MCP bearer token by design.

#### kubectl client availability

```console
$ kubectl version --client=true
./scripts/taskrun-backstop-preflight.sh: 53: kubectl: not found

# exit=127
```

#### current Kubernetes context

```console
$ kubectl config current-context
./scripts/taskrun-backstop-preflight.sh: 53: kubectl: not found

# exit=127
```

#### configured namespace for current context

```console
$ kubectl config view --minify --output jsonpath={..namespace}
./scripts/taskrun-backstop-preflight.sh: 53: kubectl: not found

# exit=127
```

#### target namespace exists

```console
$ kubectl get namespace djinn -o name
./scripts/taskrun-backstop-preflight.sh: 53: kubectl: not found

# exit=127
```

#### RBAC: list Pods in target namespace

```console
$ answer=$(kubectl auth can-i list pods -n 'djinn'); printf '%s\n' "$answer"; test "$answer" = yes
sh: 1: kubectl: not found


# exit=1
```

#### RBAC: get Pods in target namespace

```console
$ answer=$(kubectl auth can-i get pods -n 'djinn'); printf '%s\n' "$answer"; test "$answer" = yes
sh: 1: kubectl: not found


# exit=1
```

#### RBAC: list Jobs in target namespace

```console
$ answer=$(kubectl auth can-i list jobs.batch -n 'djinn'); printf '%s\n' "$answer"; test "$answer" = yes
sh: 1: kubectl: not found


# exit=1
```

#### RBAC: get Jobs in target namespace

```console
$ answer=$(kubectl auth can-i get jobs.batch -n 'djinn'); printf '%s\n' "$answer"; test "$answer" = yes
sh: 1: kubectl: not found


# exit=1
```

#### Pods read smoke test

```console
$ kubectl get pods -n djinn -o name --request-timeout=10s
./scripts/taskrun-backstop-preflight.sh: 53: kubectl: not found

# exit=127
```

#### Jobs read smoke test

```console
$ kubectl get jobs -n djinn -o name --request-timeout=10s
./scripts/taskrun-backstop-preflight.sh: 53: kubectl: not found

# exit=127
```

#### djinn-server log access smoke test

```console
$ kubectl logs -n djinn deploy/djinn-server --since=10m --tail=20
./scripts/taskrun-backstop-preflight.sh: 53: kubectl: not found

# exit=127
```

#### Djinn MCP/control-plane authentication smoke test

```console
$ curl -fsS -H "Authorization: Bearer <redacted operator token>" -H "Content-Type: application/json" -d <initialize-json> "$DJINN_MCP_URL"
DJINN_MCP_URL is not set. Set it to the operator-accessible /mcp endpoint.

# exit=2
```

## Preflight interpretation

PASS requires every command above to exit 0, including:

- `kubectl` is installed and points at the intended cluster/context.
- The checked namespace is the Djinn runtime namespace that owns `djinn-taskrun-*` Jobs/Pods.
- RBAC allows list/get for Pods and Jobs in that namespace.
- `kubectl logs` can read `deploy/djinn-server` logs.
- The Djinn MCP/control-plane endpoint accepts the operator/admin credential that will be used for `execution_kill_task` and force-close/operator-close actions.

If any item fails, stop and fix the operator environment before running the real cleanup verification. Do not treat this preflight as cleanup proof.

PRECHECK FAIL: operator environment is missing at least one required prerequisite.
````

The follow-up action-specific checks also failed in the same way, so no before-kill Kubernetes resource listing, post-kill polling, or server/coordinator log capture was possible:

```console
$ date -u +%Y-%m-%dT%H:%M:%SZ
2026-06-12T04:07:54Z

$ printf task_id/task_run_id/ns
TASK_ID=019eb9c5-d90f-78e0-95aa-edca757567c0
TASK_RUN_ID=019eba02-e846-76f1-874e-35d1985a2a36
NS=djinn
KUBECONFIG=<unset>

$ command -v kubectl || true

$ kubectl get jobs,pods -n "$NS" -l "djinn.app/task-run-id=$TASK_RUN_ID" -o wide
bash: line 9: kubectl: command not found

$ kubectl get pods -n "$NS" --field-selector=status.phase=Running -l "djinn.app/task-run-id=$TASK_RUN_ID" -o name
bash: line 10: kubectl: command not found

$ curl MCP initialize with worker-projected Djinn token (redacted)
HTTP/1.1 401 Unauthorized
content-type: text/plain; charset=utf-8
www-authenticate: Bearer resource_metadata="https://code.djinnai.io/.well-known/oauth-protected-resource/mcp"
vary: origin, access-control-request-method, access-control-request-headers
access-control-allow-credentials: true
content-length: 23
date: Fri, 12 Jun 2026 04:07:55 GMT

authentication required

$ kubectl logs -n "$NS" deploy/djinn-server --since=10m --tail=20
bash: line 19: kubectl: command not found
```

Because the preflight did not pass and the MCP `initialize` probe with the only available worker credential returned 401, the exact `execution_kill_task` invocation/result for this attempt is: **not executed; blocked before action by missing operator/admin Kubernetes and MCP access**. Epic 8451 remains blocked for the `execution_kill_task` Kubernetes cleanup proof until this runbook is executed from a real operator/admin environment with passing preflight output.

### Wave 2 operator preflight attempt from `mqt8` — failed before force-close action

- **Evidence task:** `mqt8` / `019eb9c6-04bd-74c0-90e1-7e4b4ea6e23c`
- **Active worker task_run_id:** `019eba1a-4dbe-7692-9269-82d234a888a8`
- **Attempt window (UTC):** 2026-06-12T04:33:26Z through 2026-06-12T04:33:28Z
- **Namespace/context:** intended namespace `djinn`; no Kubernetes context could be read because `kubectl` is not installed and `KUBECONFIG` is unset in this worker environment. The mounted in-cluster Kubernetes endpoint was reachable as `https://kubernetes.default.svc:443`.
- **Preflight result:** **failed**. This session did not have an operator/admin shell: `kubectl` was absent, no operator `DJINN_MCP_URL` or `DJINN_OPERATOR_BEARER_TOKEN` was configured, the in-cluster service account is forbidden from listing Pods/Jobs or getting the canonical task-run Job, and both available projected tokens were rejected by `/mcp` with HTTP 401.
- **Force-close mechanism/result:** **not invoked**. Per the runbook, the force-close/operator-close verification must stop when the operator preflight fails; this worker cannot safely exercise proposal abort/task-admin force-close and cannot capture the required before/after Kubernetes polling or server logs.
- **Reconciliation:** the force-close/operator-close Kubernetes proof criterion remains **not satisfied**. This is blocker evidence only; no cleanup success is claimed.

Complete preflight and action-specific output captured from this session:

````console
### mqt8 force-close/operator-close evidence command output
$ date -u +%Y-%m-%dT%H:%M:%SZ
2026-06-12T04:33:26Z

$ pwd
/workspace/.tmpqktXav

$ env | grep -E ... (redacted)

$ TASK_ID/TASK_RUN_ID/NS
TASK_ID=019eb9c6-04bd-74c0-90e1-7e4b4ea6e23c
TASK_RUN_ID=019eba1a-4dbe-7692-9269-82d234a888a8
NS=djinn

$ ./scripts/taskrun-backstop-preflight.sh
# Task-run backstop operator preflight evidence

- **Captured at (UTC):** `2026-06-12T04:33:26Z`
- **Namespace checked:** `djinn`
- **Server log target:** `deploy/djinn-server`
- **Djinn MCP endpoint:** `<unset DJINN_MCP_URL>`
- **Scope:** preflight only. This bundle proves the operator shell has the ingredients required for the real kill/force-close cleanup checks; it does **not** claim task-run cleanup success.
- **Redaction:** do not paste bearer tokens, kubeconfig client keys/certificates, cookies, database URLs, or full health payloads containing credentials. This script redacts the MCP bearer token by design.

#### kubectl client availability

```console
$ kubectl version --client=true
./scripts/taskrun-backstop-preflight.sh: 53: kubectl: not found

# exit=127
```

#### current Kubernetes context

```console
$ kubectl config current-context
./scripts/taskrun-backstop-preflight.sh: 53: kubectl: not found

# exit=127
```

#### configured namespace for current context

```console
$ kubectl config view --minify --output jsonpath={..namespace}
./scripts/taskrun-backstop-preflight.sh: 53: kubectl: not found

# exit=127
```

#### target namespace exists

```console
$ kubectl get namespace djinn -o name
./scripts/taskrun-backstop-preflight.sh: 53: kubectl: not found

# exit=127
```

#### RBAC: list Pods in target namespace

```console
$ answer=$(kubectl auth can-i list pods -n 'djinn'); printf '%s\n' "$answer"; test "$answer" = yes
sh: 1: kubectl: not found


# exit=1
```

#### RBAC: get Pods in target namespace

```console
$ answer=$(kubectl auth can-i get pods -n 'djinn'); printf '%s\n' "$answer"; test "$answer" = yes
sh: 1: kubectl: not found


# exit=1
```

#### RBAC: list Jobs in target namespace

```console
$ answer=$(kubectl auth can-i list jobs.batch -n 'djinn'); printf '%s\n' "$answer"; test "$answer" = yes
sh: 1: kubectl: not found


# exit=1
```

#### RBAC: get Jobs in target namespace

```console
$ answer=$(kubectl auth can-i get jobs.batch -n 'djinn'); printf '%s\n' "$answer"; test "$answer" = yes
sh: 1: kubectl: not found


# exit=1
```

#### Pods read smoke test

```console
$ kubectl get pods -n djinn -o name --request-timeout=10s
./scripts/taskrun-backstop-preflight.sh: 53: kubectl: not found

# exit=127
```

#### Jobs read smoke test

```console
$ kubectl get jobs -n djinn -o name --request-timeout=10s
./scripts/taskrun-backstop-preflight.sh: 53: kubectl: not found

# exit=127
```

#### djinn-server log access smoke test

```console
$ kubectl logs -n djinn deploy/djinn-server --since=10m --tail=20
./scripts/taskrun-backstop-preflight.sh: 53: kubectl: not found

# exit=127
```

#### Djinn MCP/control-plane authentication smoke test

```console
$ curl -fsS -H "Authorization: Bearer <redacted operator token>" -H "Content-Type: application/json" -d <initialize-json> "$DJINN_MCP_URL"
DJINN_MCP_URL is not set. Set it to the operator-accessible /mcp endpoint.

# exit=2
```

## Preflight interpretation

PASS requires every command above to exit 0, including:

- `kubectl` is installed and points at the intended cluster/context.
- The checked namespace is the Djinn runtime namespace that owns `djinn-taskrun-*` Jobs/Pods.
- RBAC allows list/get for Pods and Jobs in that namespace.
- `kubectl logs` can read `deploy/djinn-server` logs.
- The Djinn MCP/control-plane endpoint accepts the operator/admin credential that will be used for `execution_kill_task` and force-close/operator-close actions.

If any item fails, stop and fix the operator environment before running the real cleanup verification. Do not treat this preflight as cleanup proof.

PRECHECK FAIL: operator environment is missing at least one required prerequisite.

### Follow-up action-specific force-close checks
$ date -u +%Y-%m-%dT%H:%M:%SZ
2026-06-12T04:33:27Z

$ printf task_id/task_run_id/ns
TASK_ID=019eb9c6-04bd-74c0-90e1-7e4b4ea6e23c
TASK_RUN_ID=019eba1a-4dbe-7692-9269-82d234a888a8
NS=djinn
KUBECONFIG=<unset>

$ command -v kubectl || true

$ kubectl get jobs,pods -n "$NS" -l "djinn.app/task-run-id=$TASK_RUN_ID" -o wide
bash: line 29: kubectl: command not found

$ kubectl get pods -n "$NS" --field-selector=status.phase=Running -l "djinn.app/task-run-id=$TASK_RUN_ID" -o name
bash: line 33: kubectl: command not found

$ curl Kubernetes API discovery using mounted service-account token (redacted)
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
$ curl Kubernetes pods by task-run label (redacted token)
{
  "kind": "Status",
  "apiVersion": "v1",
  "metadata": {},
  "status": "Failure",
  "message": "pods is forbidden: User \"system:serviceaccount:djinn:djinn-djinn-taskrun\" cannot list resource \"pods\" in API group \"\" in the namespace \"djinn\"",
  "reason": "Forbidden",
  "details": {
    "kind": "pods"
  },
  "code": 403
}
$ curl Kubernetes jobs by task-run label (redacted token)
{
  "kind": "Status",
  "apiVersion": "v1",
  "metadata": {},
  "status": "Failure",
  "message": "jobs.batch is forbidden: User \"system:serviceaccount:djinn:djinn-djinn-taskrun\" cannot list resource \"jobs\" in API group \"batch\" in the namespace \"djinn\"",
  "reason": "Forbidden",
  "details": {
    "group": "batch",
    "kind": "jobs"
  },
  "code": 403
}
$ curl canonical Kubernetes job by name (redacted token)
{
  "kind": "Status",
  "apiVersion": "v1",
  "metadata": {},
  "status": "Failure",
  "message": "jobs.batch \"djinn-taskrun-019eba1a-4dbe-7692-9269-82d234a888a8\" is forbidden: User \"system:serviceaccount:djinn:djinn-djinn-taskrun\" cannot get resource \"jobs\" in API group \"batch\" in the namespace \"djinn\"",
  "reason": "Forbidden",
  "details": {
    "name": "djinn-taskrun-019eba1a-4dbe-7692-9269-82d234a888a8",
    "group": "batch",
    "kind": "jobs"
  },
  "code": 403
}
$ curl MCP initialize with projected Djinn token (redacted)
HTTP/1.1 401 Unauthorized
content-type: text/plain; charset=utf-8
www-authenticate: Bearer resource_metadata="https://code.djinnai.io/.well-known/oauth-protected-resource/mcp"
vary: origin, access-control-request-method, access-control-request-headers
access-control-allow-credentials: true
content-length: 23
date: Fri, 12 Jun 2026 04:33:28 GMT

authentication required
$ curl MCP initialize with Kubernetes service-account token (redacted)
HTTP/1.1 401 Unauthorized
content-type: text/plain; charset=utf-8
www-authenticate: Bearer resource_metadata="https://code.djinnai.io/.well-known/oauth-protected-resource/mcp"
vary: origin, access-control-request-method, access-control-request-headers
access-control-allow-credentials: true
content-length: 23
date: Fri, 12 Jun 2026 04:33:28 GMT

authentication required
$ kubectl logs -n "$NS" deploy/djinn-server --since=10m --tail=50
bash: line 90: kubectl: command not found

$ date -u +%Y-%m-%dT%H:%M:%SZ
2026-06-12T04:33:28Z

````

Because the preflight did not pass and both MCP `initialize` probes returned 401, the exact operator/admin/proposal-abort/force-close action result for this attempt is: **not executed; blocked before action by missing operator/admin Kubernetes and MCP access**. Epic 8451 remains blocked for the force-close/operator-close Kubernetes cleanup proof until this runbook is executed from a real operator/admin environment with passing preflight output.

## Final proposal 4369 residual-criteria reconciliation — 2026-06-12

This evidence pack reconciles the remaining proposal 4369 / epic 8451 criteria against the completed evidence tasks:

| Residual criterion | Evidence source | Reconciliation status |
| --- | --- | --- |
| Full Rust validation for the landed task-run teardown/backstop implementation. | `de12` (`019eb930-2164-7a23-ae8f-96d630fe1146`), `t2cs` (`019eb9c5-82a9-7e93-8ae5-cb2860fd792c`) | **Partially satisfied with an environmental blocker:** `cargo build` and strict `cargo clippy --all-features -- -D warnings` passed in `de12`. `t2cs` reran `cargo nextest run` after explicitly checking for the required Postgres path; Docker, `psql`, and `pg_isready` were absent, TCP `127.0.0.1:5433` refused connections, and nextest reached test execution then failed only in DB-backed tests with `Connection refused`. No teardown code defect was identified. |
| Real Kubernetes proof that `execution_kill_task` removes `djinn-taskrun-*` Pods/Jobs within roughly 60 seconds. | `9l2v` (`019eb930-5068-7213-b348-2c2c2d2d75f7`), `9eps` (`019eb9c5-d90f-78e0-95aa-edca757567c0`), sections [Wave 2 operator preflight attempt from `9eps`](#wave-2-operator-preflight-attempt-from-9eps--failed-before-cleanup-action) and [`execution_kill_task` cleanup verification attempt](#execution_kill_task-cleanup-verification-attempt--blocked-by-worker-cluster-access). | **Not satisfied:** the Wave 2 run did not reach a passing operator/admin preflight. In `9eps`, `kubectl` was absent, `KUBECONFIG` was unset, no operator MCP endpoint/token was configured, and the worker-projected token returned HTTP 401 from `/mcp`; therefore no kill action, before/after Kubernetes cleanup polling, or server/coordinator log capture could be performed. No cleanup success is claimed. |
| Real Kubernetes proof that force-close/operator-close removes `djinn-taskrun-*` Pods/Jobs within roughly 60 seconds. | `6eg3` (`019eb930-803a-7033-9b7a-42678aac97a3`), `mqt8` (`019eb9c6-04bd-74c0-90e1-7e4b4ea6e23c`), sections [Wave 2 operator preflight attempt from `mqt8`](#wave-2-operator-preflight-attempt-from-mqt8--failed-before-force-close-action), [Force-close/operator-close cleanup verification attempt](#force-closeoperator-close-cleanup-verification-attempt--blocked-by-worker-cluster-access). | **Not satisfied:** the Wave 2 force-close run still did not reach a passing operator/admin preflight. In `mqt8`, `kubectl` was absent, `KUBECONFIG` was unset, no operator MCP endpoint/token was configured, the worker service account was forbidden from reading Pods/Jobs/canonical Jobs, and both projected tokens returned HTTP 401 from `/mcp`; therefore no force-close/operator-close action, before/after Kubernetes cleanup polling, or server/coordinator log capture could be performed. No cleanup success is claimed. |

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

### Kubernetes evidence outcome from `9eps` (Wave 2 `execution_kill_task`)

- **Evidence task:** `9eps` / `019eb9c5-d90f-78e0-95aa-edca757567c0`
- **Worker task_run_id:** `019eba02-e846-76f1-874e-35d1985a2a36`
- **Attempt window:** 2026-06-12T04:07:22Z through 2026-06-12T04:07:55Z
- **Namespace/context evidence:** intended namespace `djinn`; no current Kubernetes context could be read because `kubectl` is absent and `KUBECONFIG` is unset. The in-cluster namespace file reported `djinn`.
- **Pre-kill evidence:** documented above under [Wave 2 operator preflight attempt from `9eps`](#wave-2-operator-preflight-attempt-from-9eps--failed-before-cleanup-action). Intended `kubectl get jobs,pods -n "$NS" -l "djinn.app/task-run-id=$TASK_RUN_ID" -o wide` and running-Pod polling commands failed with `kubectl: command not found`, so the target `djinn-taskrun-*` resource could not be proven present before kill.
- **Kill invocation evidence:** no operator `DJINN_MCP_URL` or `DJINN_OPERATOR_BEARER_TOKEN` was configured, and MCP `initialize` with the worker-projected token returned HTTP 401 `authentication required`; therefore `execution_kill_task` was **not executed**.
- **Post-kill/log evidence:** no post-kill Kubernetes polling or `deploy/djinn-server` server/coordinator log capture was possible because `kubectl` was absent and there was no authenticated operator/admin control-plane session.
- **Reconciliation:** the Wave 2 `execution_kill_task` proposal criterion is **not satisfied**. This is access-blocker evidence only; no cleanup success is claimed.

### Kubernetes evidence outcome from `6eg3` (force-close/operator-close)

- **Evidence task:** `6eg3` / `019eb930-803a-7033-9b7a-42678aac97a3`
- **Worker task_run_id:** `019eb9a2-bc65-7e71-a61e-026dc12e1516`
- **Attempt window:** 2026-06-12T02:22:27Z through 2026-06-12T02:22:28Z
- **Namespace/context evidence:** namespace `djinn`; Kubernetes API endpoint `https://kubernetes.default.svc:443` (`KUBERNETES_SERVICE_HOST=10.43.0.1`, `KUBERNETES_SERVICE_PORT=443`); identity was `system:serviceaccount:djinn:djinn-djinn-taskrun`; Kubernetes API discovery returned `APIVersions` for `v1`.
- **Pre-force-close evidence:** documented below under [Documented pre-force-close Kubernetes checks](#documented-pre-force-close-kubernetes-checks) and the force-close [Direct in-cluster Kubernetes API fallback](#direct-in-cluster-kubernetes-api-fallback-1). Intended `kubectl get jobs,pods ... -l djinn.app/task-run-id=019eb9a2-bc65-7e71-a61e-026dc12e1516` commands failed because `kubectl` was not installed. Direct API fallback returned 403 for listing Pods, listing Jobs, and getting `jobs.batch/djinn-taskrun-019eb9a2-bc65-7e71-a61e-026dc12e1516`.
- **Force-close action evidence:** documented below under [Force-close/operator-close action path](#force-closeoperator-close-action-path). Both the projected Djinn token and Kubernetes service-account token returned HTTP 401 `authentication required` from `/mcp`, so no authenticated operator/admin/proposal-abort/force-close action was executed.
- **Post-force-close/log evidence:** documented below under [Post-force-close polling and logs](#post-force-close-polling-and-logs). No post-force-close polling or server/coordinator logs were captured because resource reads, logs, and authenticated force-close invocation were blocked.
- **Reconciliation:** the attempt is valid blocker evidence, but the proposal criterion requiring actual before/after cleanup proof is **not satisfied**.

### Kubernetes evidence outcome from `mqt8` (Wave 2 force-close/operator-close)

- **Evidence task:** `mqt8` / `019eb9c6-04bd-74c0-90e1-7e4b4ea6e23c`
- **Worker task_run_id:** `019eba1a-4dbe-7692-9269-82d234a888a8`
- **Attempt window:** 2026-06-12T04:33:26Z through 2026-06-12T04:33:28Z
- **Namespace/context evidence:** intended namespace `djinn`; no current context could be read because `kubectl` is absent and `KUBECONFIG` is unset. Direct Kubernetes API discovery via the mounted service-account token succeeded, but the identity `system:serviceaccount:djinn:djinn-djinn-taskrun` was RBAC-forbidden from listing Pods, listing Jobs, or getting `jobs.batch/djinn-taskrun-019eba1a-4dbe-7692-9269-82d234a888a8`.
- **Pre-force-close evidence:** documented above under [Wave 2 operator preflight attempt from `mqt8`](#wave-2-operator-preflight-attempt-from-mqt8--failed-before-force-close-action). Intended `kubectl get jobs,pods ... -l djinn.app/task-run-id=019eba1a-4dbe-7692-9269-82d234a888a8` commands failed because `kubectl` was not installed; direct API fallback showed 403 RBAC denials rather than resource listings.
- **Force-close action evidence:** both the projected Djinn token and Kubernetes service-account token returned HTTP 401 `authentication required` from `/mcp`, so no authenticated operator/admin/proposal-abort/force-close action was executed.
- **Post-force-close/log evidence:** no post-force-close polling or server/coordinator logs were captured because resource reads, logs, and authenticated force-close invocation were blocked.
- **Reconciliation:** the Wave 2 force-close/operator-close proposal criterion is **not satisfied**. This is access-blocker evidence only; no cleanup success is claimed.

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
