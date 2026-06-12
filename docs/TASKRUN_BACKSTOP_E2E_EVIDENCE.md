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
| Wave 3 follow-up: same `execution_kill_task` Kubernetes proof, run through the new operator evidence runner from task `7283`. | `8cd0` (`019ebb90-db0c-7fd3-9fe4-0cd707bb5d94`), sections [Wave 3 paste point: `execution_kill_task` operator evidence](#wave-3-paste-point-execution_kill_task-operator-evidence) and [Wave 3 reconciliation against the acceptance criteria](#wave-3-reconciliation-against-the-acceptance-criteria). | **Not satisfied:** Wave 3 ran the `scripts/taskrun-backstop-e2e-evidence.sh` operator runner with `MODE=kill`, `TASK_ID=019ebb90-db0c-7fd3-9fe4-0cd707bb5d94`, `TASK_RUN_ID=019ebbaa-e748-7462-9c05-0dd0e356d50f`, `NS=djinn`, and `DRY_RUN=0`, but the same operator-access blocker documented in `9eps` is still in effect: `kubectl` is not installed in the worker image, `KUBECONFIG` is unset, the in-cluster service-account token is rejected with HTTP 401 by the Kubernetes API (and the only Djinn token returned HTTP 401 from `/mcp`), so the embedded preflight ended with `PRECHECK FAIL` and the runner footer is `EVIDENCE RUNNER FAIL`. The 60-second post-action polling, the `execution_kill_task` invocation, and the `deploy/djinn-server` log capture were intentionally skipped by the runner; no cleanup success is claimed. Wave 3 closes only the runner-and-bundle-format half of the follow-up: the operator-runner + dry-run + inline-bundle machinery from `7jbf`/`7283` is exercised end-to-end and the bundle is recorded verbatim in this doc. The remaining `execution_kill_task` Kubernetes cleanup proof is still blocked by the missing operator/admin environment, not by a teardown code defect. |
| Wave 3 follow-up: force-close/operator-close Kubernetes proof, run through the new operator evidence runner from task `7283`. | `wu8h` (`019ebb91-0d17-7d10-932e-718354962d11`), section [Wave 3 paste point: force-close/operator-close evidence](#wave-3-paste-point-force-closeoperator-close-evidence) and [Wave 3 force-close reconciliation against the acceptance criteria](#wave-3-force-close-reconciliation-against-the-acceptance-criteria). | **Not satisfied:** Wave 3 ran the `scripts/taskrun-backstop-e2e-evidence.sh` operator runner with `MODE=force-close`, `TASK_ID=019ebb91-0d17-7d10-932e-718354962d11`, `TASK_RUN_ID=019ebbc0-ddfe-7211-aa82-f31ad032fb49`, `NS=djinn`, and `DRY_RUN=0`, but the same operator-access blocker remains: `kubectl` is not installed, `KUBECONFIG` is unset, no operator MCP endpoint/token is configured, and the worker service account is forbidden from listing Pods/Jobs or getting the canonical task-run Job. The embedded preflight ended with `PRECHECK FAIL`; the safe force-close/operator-close mechanism was not invoked; post-action polling and server log capture were skipped; no cleanup success is claimed. The remaining force-close Kubernetes cleanup proof is blocked by the missing operator/admin environment, not by a teardown code defect. |

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

### Wave 3 reconciliation outcome from `8cd0`

- **Evidence task:** `8cd0` / `019ebb90-db0c-7fd3-9fe4-0cd707bb5d94`
- **Active task_run_id:** `019ebbaa-e748-7462-9c05-0dd0e356d50f`
- **Attempt window (UTC):** 2026-06-12T11:51:42Z through 2026-06-12T11:53:03Z
- **Runner invocation:** `MODE=kill DRY_RUN=0 NS=djinn TASK_ID=019ebb90-db0c-7fd3-9fe4-0cd707bb5d94 TASK_RUN_ID=019ebbaa-e748-7462-9c05-0dd0e356d50f ./scripts/taskrun-backstop-e2e-evidence.sh`
- **Result:** the runner emitted a complete bundle (header → preflight → before-action → action placeholder → post-action → server log → bundle interpretation) but the embedded preflight ended with `PRECHECK FAIL`, the runner footer is `EVIDENCE RUNNER FAIL`, and the 60-second post-action polling + `deploy/djinn-server` log capture were intentionally skipped. No `execution_kill_task` invocation, no before/after Kubernetes cleanup polling, and no inline teardown/backstop markers were captured.
- **Operator environment available:** **no** — this is a normal task-run worker shell, not an operator/admin environment. `kubectl` is not installed, `KUBECONFIG` is unset, the in-cluster service-account token is rejected with HTTP 401 by the Kubernetes API server (even for the unauthenticated `/api` discovery endpoint), and the only Djinn token returned HTTP 401 from `/mcp`. The Wave 3 attempt therefore records the same operator-access blocker that the Wave 2 attempts (`9eps`, `mqt8`) and the Wave 1 attempts (`9l2v`, `6eg3`) recorded, just rendered through the new `7283` evidence runner.
- **Reconciliation:** the Wave 3 `execution_kill_task` Kubernetes proof is **still not satisfied**, but the operator evidence runner + inline bundle format from `7283` has now been exercised end-to-end against this operator-blocker scenario and the bundle is recorded verbatim in this doc (see the [Wave 3 paste point: `execution_kill_task` operator evidence](#wave-3-paste-point-execution_kill_task-operator-evidence) section). The remaining gap is the missing operator/admin environment, not a teardown code defect and not a runner-shape defect.
- **Wave 3 follow-up actions:**
  1. The corresponding force-close/operator-close runner invocation is now recorded under the [Wave 3 paste point: force-close/operator-close evidence](#wave-3-paste-point-force-closeoperator-close-evidence) section below; the Wave 3 final consolidation remains the sibling task `dzcv`.
  2. The companion Postgres-backed full validation entrypoint is now reproducible via `make validate-taskrun-backstop` (or `./scripts/validate-taskrun-backstop.sh`) and is summarized in the [Wave 3 Postgres-backed full validation entrypoint](#wave-3-postgres-backed-full-validation-entrypoint) section above.
  3. The implementation evidence (inline `RuntimeOps::teardown_taskrun_job`, pool `teardown_taskrun_jobs_for_task`, zombie-reaper `teardown_taskrun_job`, periodic + startup `reap_orphaned_taskrun_jobs` backstop from `ld18` / `o17b`) and the runbook + operator evidence runner (from `7jbf` / `7283`) are unchanged; this attempt does not modify teardown code and explicitly leaves the architecture intact, per the task's "Do not modify teardown implementation unless this run exposes a concrete code defect; fix narrowly if so" guidance.

### Wave 3 reconciliation outcome from `wu8h`

- **Evidence task:** `wu8h` / `019ebb91-0d17-7d10-932e-718354962d11`
- **Active task_run_id:** `019ebbc0-ddfe-7211-aa82-f31ad032fb49`
- **Attempt window (UTC):** 2026-06-12T12:14:36Z through 2026-06-12T12:15:01Z
- **Runner invocation:** `MODE=force-close DRY_RUN=0 NS=djinn TASK_ID=019ebb91-0d17-7d10-932e-718354962d11 TASK_RUN_ID=019ebbc0-ddfe-7211-aa82-f31ad032fb49 ./scripts/taskrun-backstop-e2e-evidence.sh`
- **Result:** the runner emitted a complete force-close/operator-close bundle, but the embedded preflight ended with `PRECHECK FAIL`, the runner footer is `EVIDENCE RUNNER FAIL`, and the 60-second post-action polling + `deploy/djinn-server` log capture were intentionally skipped. No force-close/operator-close invocation, no before/after Kubernetes cleanup polling, and no inline teardown/backstop markers were captured.
- **Operator environment available:** **no** — this is a normal task-run worker shell, not an operator/admin environment. `kubectl` is not installed, `KUBECONFIG` is unset, no operator `DJINN_MCP_URL` or `DJINN_OPERATOR_BEARER_TOKEN` is configured, and direct Kubernetes API probes using the mounted service-account token show RBAC 403 denials for listing Pods, listing Jobs, and getting `jobs.batch/djinn-taskrun-019ebbc0-ddfe-7211-aa82-f31ad032fb49`.
- **Reconciliation:** the Wave 3 force-close/operator-close Kubernetes proof is **still not satisfied**. This is blocker evidence only, not cleanup success. The remaining gap is the missing operator/admin environment, not a teardown code defect and not a runner-shape defect.


## Wave 3 operator evidence runner usage and paste points

The Wave 3 follow-up work in task `7283` added an operator/admin evidence runner, `scripts/taskrun-backstop-e2e-evidence.sh`, so the remaining kill and force-close evidence does not have to be hand-typed. The runner reuses `scripts/taskrun-backstop-preflight.sh`, captures the before-action Kubernetes resources, emits the `execution_kill_task` or force-close/operator-close action placeholder, runs the 60-second post-action polling loop, captures the filtered `deploy/djinn-server` log capture, redacts the operator bearer token, and fails closed when any prerequisite is missing. The full usage and required env vars are documented in `docs/TASKRUN_BACKSTOP_VERIFICATION.md` (Wave 3 operator evidence runner section) and `scripts/README.md`.

Operator workflow for the Wave 3 kill and force-close proofs (`8cd0`, `wu8h`):

1. Run the dry-run form first to confirm the bundle shape and field substitution:

   ```sh
   NS=djinn \
   TASK_ID="<long-running-task-id>" \
   TASK_RUN_ID="<active-task-run-id>" \
   DJINN_MCP_URL="https://<operator-accessible-djinn-host>/mcp" \
   DJINN_OPERATOR_BEARER_TOKEN="<operator/admin token>" \
   MODE=kill \
   DRY_RUN=1 \
     ./scripts/taskrun-backstop-e2e-evidence.sh | tee /tmp/taskrun-backstop-e2e-kill.dryrun.md
   ```

2. After issuing the action (`execution_kill_task` or force-close/operator-close) from the same shell, record the action window and re-run for the real bundle:

   ```sh
   export ACTION_INVOKED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
   export ACTION_RESULT="execution_kill_task returned ok"   # or "proposal abort <id> closed task"
   MODE=kill \
     ./scripts/taskrun-backstop-e2e-evidence.sh | tee taskrun-backstop-e2e-kill.md
   ```

3. Redact secrets, then paste the bundle into the matching Wave 3 paste point below. The paste points live directly under this section so the Wave 3 evidence is grouped together and easy for the next Planner to reconcile.

### Wave 3 paste point: `execution_kill_task` operator evidence

- **Evidence task:** `8cd0` / `019ebb90-db0c-7fd3-9fe4-0cd707bb5d94`
- **Active task_run_id for the verification attempt:** `019ebbaa-e748-7462-9c05-0dd0e356d50f`
- **Operator environment available:** **no** — this attempt executed in a normal task-run worker shell, not an operator/admin environment.
- **Attempt window (UTC):** 2026-06-12T11:51:42Z through 2026-06-12T11:53:03Z
- **Namespace:** `djinn`
- **DRY_RUN:** `0` (the runner was invoked without `DRY_RUN` so the bundle includes the full preflight, before-action, and skipped polling/log sections).
- **Bundle artifact:** `wave3-8cd0-kill-bundle.md` (also embedded inline below).
- **Direct-API fallback artifact:** `wave3-8cd0-direct-api.md` (also embedded inline below).
- **Runner footer:** `EVIDENCE RUNNER FAIL: at least one prerequisite is missing or the post-action poll did not converge. The bundle above records the exact failure; do not claim cleanup success for this attempt.`
- **Cleanup success claimed:** **no** — per the runner guidance and the task description, the failed preflight blocks the kill action and the 60-second post-action poll; this section is blocker evidence only.

#### Wave 3 evidence runner bundle (`MODE=kill`)

The full operator evidence runner output is embedded below verbatim. The runner header records the captured UTC timestamp, namespace, task id, task run id, and action invocation; the embedded preflight ends with `PRECHECK FAIL`; the before-action subsection is recorded but `kubectl` was absent, so the target `djinn-taskrun-*` Job/Pod could not be proven present before kill; and the post-action 60-second polling + server log capture were intentionally skipped because the preflight did not pass.

````md
# Task-run backstop operator evidence — execution_kill_task

- **Captured at (UTC):** `2026-06-12T11:51:42Z`
- **Mode:** `kill`
- **Action mode display:** `execution_kill_task`
- **Namespace:** `djinn`
- **Task id:** `019ebb90-db0c-7fd3-9fe4-0cd707bb5d94`
- **Task run id:** `019ebbaa-e748-7462-9c05-0dd0e356d50f`
- **Server log target:** `deploy/djinn-server`
- **Djinn MCP endpoint:** `<unset DJINN_MCP_URL>`
- **DRY_RUN:** `0`
- **Redaction:** do not paste bearer tokens, kubeconfig client keys/certificates, cookies, database URLs, or full health payloads containing credentials. This script redacts the MCP bearer token by design.
- **Scope:** this bundle captures the operator-side evidence for one Wave 3 cleanup attempt. It does **not** claim cleanup success unless the post-action polling converges and the operator records the action result.
- **Operator action invoked at (UTC):** `2026-06-12T11:51:42Z`
- **Operator action result:** `<unset ACTION_RESULT; runner marked this as the Wave 3 capture for 8cd0 - see preflight fail; kill was NOT invoked>`

## Operator preflight (reused from taskrun-backstop-preflight.sh)


#### Preflight output

The preflight helper was invoked with NS=`djinn`,
DJINN_SERVER_DEPLOY=`deploy/djinn-server`,
DJINN_MCP_URL=``, and the operator token redacted. The
helper's complete Markdown bundle is embedded below verbatim (no outer
code fence) so the inner `console` blocks stay readable.

<!-- taskrun-backstop-e2e-evidence.sh: preflight bundle begin -->

# Task-run backstop operator preflight evidence

- **Captured at (UTC):** `2026-06-12T11:51:42Z`
- **Namespace checked:** `djinn`
- **Server log target:** `deploy/djinn-server`
- **Djinn MCP endpoint:** `<unset DJINN_MCP_URL>`
- **Scope:** preflight only. This bundle proves the operator shell has the ingredients required for the real kill/force-close cleanup checks; it does **not** claim task-run cleanup success.
- **Redaction:** do not paste bearer tokens, kubeconfig client keys/certificates, cookies, database URLs, or full health payloads containing credentials. This script redacts the MCP bearer token by design.

#### kubectl client availability

```console
$ kubectl version --client=true
/workspace/.tmplPiR2E/scripts/taskrun-backstop-preflight.sh: 53: kubectl: not found

# exit=127
```

#### current Kubernetes context

```console
$ kubectl config current-context
/workspace/.tmplPiR2E/scripts/taskrun-backstop-preflight.sh: 53: kubectl: not found

# exit=127
```

#### configured namespace for current context

```console
$ kubectl config view --minify --output jsonpath={..namespace}
/workspace/.tmplPiR2E/scripts/taskrun-backstop-preflight.sh: 53: kubectl: not found

# exit=127
```

#### target namespace exists

```console
$ kubectl get namespace djinn -o name
/workspace/.tmplPiR2E/scripts/taskrun-backstop-preflight.sh: 53: kubectl: not found

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
/workspace/.tmplPiR2E/scripts/taskrun-backstop-preflight.sh: 53: kubectl: not found

# exit=127
```

#### Jobs read smoke test

```console
$ kubectl get jobs -n djinn -o name --request-timeout=10s
/workspace/.tmplPiR2E/scripts/taskrun-backstop-preflight.sh: 53: kubectl: not found

# exit=127
```

#### djinn-server log access smoke test

```console
$ kubectl logs -n djinn deploy/djinn-server --since=10m --tail=20
/workspace/.tmplPiR2E/scripts/taskrun-backstop-preflight.sh: 53: kubectl: not found

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

<!-- taskrun-backstop-e2e-evidence.sh: preflight bundle end (exit=1) -->

> Preflight did not pass; the operator environment is missing at least one required prerequisite. The bundle below the preflight is therefore marked **NOT PASS** and the runner did not perform the post-action 60-second polling or claim cleanup success.

## Before-action Kubernetes evidence

- **Action captured at (UTC):** `2026-06-12T11:51:43Z`
- **Namespace:** `djinn`
- **Task id:** `019ebb90-db0c-7fd3-9fe4-0cd707bb5d94`
- **Task run id:** `019ebbaa-e748-7462-9c05-0dd0e356d50f`
- **Label selector key:** `djinn.app/task-run-id`
- **Canonical job/pod prefix:** `djinn-taskrun-`

#### kubectl client availability

```console
$ command -v kubectl || true

kubectl not found; before-action evidence cannot be captured.
```

#### Operator action invocation

- **Action tool:** `execution_kill_task`
- **Action arguments:** `task_id="019ebb90-db0c-7fd3-9fe4-0cd707bb5d94"`, `reason="task-run backstop e2e verification"`

The operator must invoke the `execution_kill_task` MCP tool from the
same shell (or record the equivalent transcript) and set
`ACTION_RESULT` / `ACTION_INVOKED_AT` before re-running this script.
The placeholder text below shows the exact shell-template an operator
can copy/paste, with `$TASK_ID` and `$DJINN_MCP_URL` as the only
substitutions required.

```json
{
  "tool": "execution_kill_task",
  "arguments": {
    "task_id": "$TASK_ID",
    "reason": "task-run backstop e2e verification"
  }
}
```

Equivalent `curl` invocation (operator token redacted in the bundle):

```console
$ curl -sS -H "Authorization: Bearer <redacted operator token>" \
    -H "Content-Type: application/json" \
    -H "Accept: application/json, text/event-stream" \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"execution_kill_task","arguments":{"task_id":"$TASK_ID","reason":"task-run backstop e2e verification"}}}' \
    "$DJINN_MCP_URL"
```


## Post-action 60-second cleanup polling

> Preflight did not pass: post-action polling was skipped. Fix the operator environment, rerun the preflight until it passes, and then re-run this script with ACTION_RESULT/ACTION_INVOKED_AT set.

## Server/coordinator log capture

> Preflight did not pass: server log capture was skipped. Fix the operator environment and re-run this script with ACTION_RESULT/ACTION_INVOKED_AT set.

## Bundle interpretation

This bundle is a single Wave 3 evidence attempt. The exact fields
required for review are:

- `TASK_ID` and `TASK_RUN_ID` recorded in the header.
- `NS` and the Kubernetes context (captured inside the preflight
  subsection).
- UTC timestamps for preflight, before-action capture, action
  invocation, and each polling iteration.
- Exact commands, raw output, and exit statuses for every probe.
- A redaction note reminding reviewers that bearer tokens, kubeconfig
  client keys/certificates, cookies, database URLs, and full health
  payloads containing credentials must be removed before committing the
  bundle.

The bundle does **not** claim cleanup success unless all of the
following are true:

- The preflight subsection shows `PRECHECK PASS`.
- The `Operator action invocation` subsection is followed by an
  operator-issued action (or a recorded transcript) and the
  `ACTION_RESULT` header field is populated.
- The `Post-action 60-second cleanup polling` subsection ends with
  `exit=0` and shows `running_pods= jobs= canonical=` for the final
  iteration.

If any of these conditions is false, the bundle records the exact
blocker (operator access, missing MCP credential, missing RBAC, or
remaining Pods/Jobs) and explicitly states that no cleanup success is
claimed.

EVIDENCE RUNNER FAIL: at least one prerequisite is missing or the post-action poll did not converge. The bundle above records the exact failure; do not claim cleanup success for this attempt.

````

#### Wave 3 direct in-cluster API + `/mcp` fallback

The worker also tried the same direct Kubernetes API and `/mcp` fallback that the Wave 2 attempts (`9eps`, `mqt8`) used, to confirm the operator-access blocker is still in effect in this session. The bearer token is intentionally not pasted.

````md
## Wave 3 (`8cd0`) direct in-cluster API fallback — worker shell

- **Evidence task:** `8cd0` / `019ebb90-db0c-7fd3-9fe4-0cd707bb5d94`
- **Active task_run_id:** `019ebbaa-e748-7462-9c05-0dd0e356d50f`
- **In-cluster namespace (from `/var/run/secrets/kubernetes.io/serviceaccount/namespace`):** `djinn`
- **Kubernetes API endpoint:** `https://kubernetes.default.svc:443` (`KUBERNETES_SERVICE_HOST=10.43.0.1`, `KUBERNETES_SERVICE_PORT=443`)
- **Captured at (UTC):** `2026-06-12T11:53:02Z`

The operator evidence runner is unable to claim cleanup success from this shell.
To complement the runner output captured in `wave3-8cd0-kill-bundle.md`,
the worker also tried the same direct Kubernetes API and `/mcp`
fallback used in the Wave 2 attempts (`9eps`, `mqt8`) so reviewers can
see that the same operator-access blocker is in effect. The bearer
token is intentionally not pasted.

```console
$ date -u +%Y-%m-%dT%H:%M:%SZ
2026-06-12T11:53:02Z

$ wc -c /var/run/secrets/tokens/djinn
1162 /var/run/secrets/tokens/djinn

$ curl --cacert "$CA_FILE" https://kubernetes.default.svc:443/api
{
  "kind": "Status",
  "apiVersion": "v1",
  "metadata": {},
  "status": "Failure",
  "message": "Unauthorized",
  "reason": "Unauthorized",
  "code": 401
}
$ curl --cacert "$CA_FILE" -H "Authorization: Bearer <redacted service-account token>" \
  "https://kubernetes.default.svc:443/api/v1/namespaces/djinn/pods?labelSelector=djinn.app%2Ftask-run-id%3D019ebbaa-e748-7462-9c05-0dd0e356d50f"
{
  "kind": "Status",
  "apiVersion": "v1",
  "metadata": {},
  "status": "Failure",
  "message": "Unauthorized",
  "reason": "Unauthorized",
  "code": 401
}
$ curl --cacert "$CA_FILE" -H "Authorization: Bearer <redacted service-account token>" \
  "https://kubernetes.default.svc:443/apis/batch/v1/namespaces/djinn/jobs?labelSelector=djinn.app%2Ftask-run-id%3D019ebbaa-e748-7462-9c05-0dd0e356d50f"
{
  "kind": "Status",
  "apiVersion": "v1",
  "metadata": {},
  "status": "Failure",
  "message": "Unauthorized",
  "reason": "Unauthorized",
  "code": 401
}
$ curl --cacert "$CA_FILE" -H "Authorization: Bearer <redacted service-account token>" \
  "https://kubernetes.default.svc:443/apis/batch/v1/namespaces/djinn/jobs/djinn-taskrun-019ebbaa-e748-7462-9c05-0dd0e356d50f"
{
  "kind": "Status",
  "apiVersion": "v1",
  "metadata": {},
  "status": "Failure",
  "message": "Unauthorized",
  "reason": "Unauthorized",
  "code": 401
}
$ curl -i -H "Authorization: Bearer <redacted /var/run/secrets/tokens/djinn token>" \
  -H "Content-Type: application/json" -d <initialize-json> \
  http://djinn-server.djinn.svc.cluster.local:3000/mcp
HTTP/1.1 401 Unauthorized
content-type: text/plain; charset=utf-8
www-authenticate: Bearer resource_metadata="https://code.djinnai.io/.well-known/oauth-protected-resource/mcp"
vary: origin, access-control-request-method, access-control-request-headers
access-control-allow-credentials: true
content-length: 23
date: Fri, 12 Jun 2026 11:53:03 GMT

authentication required
$ date -u +%Y-%m-%dT%H:%M:%SZ
2026-06-12T11:53:03Z

```

> The Kubernetes API server returned HTTP 401 `Unauthorized` for the worker-projected service-account token in this session, even on the unauthenticated `/api` discovery endpoint. The same token is rejected by `/mcp` with HTTP 401 `authentication required`. This is the same operator-access blocker documented in `9eps`/`mqt8`: the worker shell is not an operator/admin environment, and there is no operator MCP endpoint or token configured. The runner is therefore recorded as `EVIDENCE RUNNER FAIL`, and no cleanup success is claimed.

````

#### Wave 3 reconciliation against the acceptance criteria

- **Acceptance criterion 1 (run attempt with evidence runner after preflight, recording task id, task run id, namespace/context, UTC timestamps):** **satisfied as an attempt only.** The runner was invoked with `TASK_ID=019ebb90-db0c-7fd3-9fe4-0cd707bb5d94`, `TASK_RUN_ID=019ebbaa-e748-7462-9c05-0dd0e356d50f`, `NS=djinn`, and `MODE=kill` at `2026-06-12T11:51:42Z`. The header, preflight, before-action, action placeholder, post-action, server log, and bundle interpretation sections were all emitted, and the UTC timestamps are recorded at every step. The preflight failed (`PRECHECK FAIL`), so this attempt is a runner invocation + failed preflight, not a passing operator verification.
- **Acceptance criterion 2 (before-kill Kubernetes evidence shows the target `djinn-taskrun-*` Job/Pod exists, or the exact operator-access blocker is documented without claiming success):** **operator-access blocker documented.** The preflight subsection shows that `kubectl` is not installed in this worker image (`kubectl: not found`, exit 127 on every `kubectl` probe); the runner therefore emitted `kubectl not found; before-action evidence cannot be captured` in the before-action subsection. The direct Kubernetes API fallback confirmed the worker-projected service-account token is rejected with HTTP 401 `Unauthorized`, so the runner cannot read Pods/Jobs even via the API. No `djinn-taskrun-*` Job/Pod is proven present; the operator-access blocker is recorded explicitly and no success is claimed.
- **Acceptance criterion 3 (after-kill polling shows no running Pods/Jobs/canonical `djinn-taskrun-$TASK_RUN_ID` resources within ~60 s, or the exact failure/blocker is documented):** **failure/blocker documented.** The runner intentionally skipped the 60-second post-action polling because the preflight did not pass; the bundle subsection states `Preflight did not pass: post-action polling was skipped. Fix the operator environment, rerun the preflight until it passes, and then re-run this script with ACTION_RESULT/ACTION_INVOKED_AT set.` No after-kill polling iteration, no convergence, and no `execution_kill_task` invocation is recorded; the runner footer is `EVIDENCE RUNNER FAIL`. The direct Kubernetes API fallback confirms the worker cannot read Pods/Jobs at all, so polling would not have produced meaningful evidence even if it had run.
- **Acceptance criterion 4 (relevant server/coordinator logs around the kill time are captured, including inline teardown/backstop markers where available):** **blocker documented; no logs captured.** The runner subsection states `Preflight did not pass: server log capture was skipped. Fix the operator environment and re-run this script with ACTION_RESULT/ACTION_INVOKED_AT set.` No `deploy/djinn-server` log capture and no inline teardown/backstop markers were produced, because the worker has no `kubectl` and the API token is rejected. The expected markers (`task-run Job backstop`, `backstop reaped orphaned task-run Job`, `task_run_id=…`, `job_name`, `reason`, `db_classification`) are still documented in the runner header but no log lines were captured for this attempt.
- **Acceptance criterion 5 (this doc is updated with a reconciliation statement for whether the `execution_kill_task` Kubernetes proof is now satisfied):** **satisfied by this subsection.** The `execution_kill_task` Kubernetes proof is **still not satisfied**: the same operator-access blocker documented in `9eps` (and previously in `9l2v`) is in effect, the Wave 3 runner was only able to emit a `PRECHECK FAIL` bundle, and no kill action, no before/after Kubernetes polling, and no server/coordinator logs were captured. The implementation evidence (inline `RuntimeOps::teardown_taskrun_job`, pool `teardown_taskrun_jobs_for_task`, zombie-reaper `teardown_taskrun_job`, and the `reap_orphaned_taskrun_jobs` / `sweep_stale_resources` periodic + startup backstop) is in place from `ld18` / `o17b`, and the runbook and runner are in place from `7jbf` / `7283`; the remaining gap is purely the missing operator/admin environment, not a teardown code defect.

### Wave 3 paste point: force-close/operator-close evidence

- **Evidence task:** `wu8h` / `019ebb91-0d17-7d10-932e-718354962d11`
- **Active task_run_id for the verification attempt:** `019ebbc0-ddfe-7211-aa82-f31ad032fb49`
- **Operator environment available:** **no** — this attempt executed in a normal task-run worker shell, not an operator/admin environment.
- **Attempt window (UTC):** 2026-06-12T12:14:36Z through 2026-06-12T12:15:01Z
- **Namespace:** `djinn` (from the mounted service-account namespace file; no kube context was available because `kubectl` is absent and `KUBECONFIG` is unset).
- **DRY_RUN:** `0` (the runner was invoked without `DRY_RUN` so the bundle includes the full preflight, before-action, and skipped polling/log sections).
- **Bundle artifact:** `wave3-wu8h-force-close-bundle.md` (also embedded inline below).
- **Direct-API fallback artifact:** `wave3-wu8h-direct-api.md` (also embedded inline below).
- **Runner footer:** `EVIDENCE RUNNER FAIL: at least one prerequisite is missing or the post-action poll did not converge. The bundle above records the exact failure; do not claim cleanup success for this attempt.`
- **Force-close mechanism/result:** **not invoked**. The safe operator/admin force-close mechanism could not be exercised from this worker because the runner preflight failed before action: no `kubectl`, no kube context, no Pod/Job/log RBAC, and no `DJINN_MCP_URL`/`DJINN_OPERATOR_BEARER_TOKEN`.
- **Cleanup success claimed:** **no** — per the runner guidance and the task description, the failed preflight blocks the force-close action and the 60-second post-action poll; this section is blocker evidence only.

#### Wave 3 evidence runner bundle (`MODE=force-close`)

The full operator evidence runner output is embedded below verbatim. The runner header records the captured UTC timestamp, namespace, task id, task run id, and force-close/operator-close mode; the embedded preflight ends with `PRECHECK FAIL`; the before-action subsection is recorded but `kubectl` was absent, so the target `djinn-taskrun-*` Job/Pod could not be proven present before force-close; and the post-action 60-second polling + server log capture were intentionally skipped because the preflight did not pass.

````md
# Task-run backstop operator evidence — force-close/operator-close

- **Captured at (UTC):** `2026-06-12T12:14:36Z`
- **Mode:** `force-close`
- **Action mode display:** `force-close/operator-close`
- **Namespace:** `djinn`
- **Task id:** `019ebb91-0d17-7d10-932e-718354962d11`
- **Task run id:** `019ebbc0-ddfe-7211-aa82-f31ad032fb49`
- **Server log target:** `deploy/djinn-server`
- **Djinn MCP endpoint:** `<unset DJINN_MCP_URL>`
- **DRY_RUN:** `0`
- **Redaction:** do not paste bearer tokens, kubeconfig client keys/certificates, cookies, database URLs, or full health payloads containing credentials. This script redacts the MCP bearer token by design.
- **Scope:** this bundle captures the operator-side evidence for one Wave 3 cleanup attempt. It does **not** claim cleanup success unless the post-action polling converges and the operator records the action result.

## Operator preflight (reused from taskrun-backstop-preflight.sh)


#### Preflight output

The preflight helper was invoked with NS=`djinn`,
DJINN_SERVER_DEPLOY=`deploy/djinn-server`,
DJINN_MCP_URL=``, and the operator token redacted. The
helper's complete Markdown bundle is embedded below verbatim (no outer
code fence) so the inner `console` blocks stay readable.

<!-- taskrun-backstop-e2e-evidence.sh: preflight bundle begin -->

# Task-run backstop operator preflight evidence

- **Captured at (UTC):** `2026-06-12T12:14:36Z`
- **Namespace checked:** `djinn`
- **Server log target:** `deploy/djinn-server`
- **Djinn MCP endpoint:** `<unset DJINN_MCP_URL>`
- **Scope:** preflight only. This bundle proves the operator shell has the ingredients required for the real kill/force-close cleanup checks; it does **not** claim task-run cleanup success.
- **Redaction:** do not paste bearer tokens, kubeconfig client keys/certificates, cookies, database URLs, or full health payloads containing credentials. This script redacts the MCP bearer token by design.

#### kubectl client availability

```console
$ kubectl version --client=true
/workspace/.tmpV2Rm1c/scripts/taskrun-backstop-preflight.sh: 53: kubectl: not found

# exit=127
```

#### current Kubernetes context

```console
$ kubectl config current-context
/workspace/.tmpV2Rm1c/scripts/taskrun-backstop-preflight.sh: 53: kubectl: not found

# exit=127
```

#### configured namespace for current context

```console
$ kubectl config view --minify --output jsonpath={..namespace}
/workspace/.tmpV2Rm1c/scripts/taskrun-backstop-preflight.sh: 53: kubectl: not found

# exit=127
```

#### target namespace exists

```console
$ kubectl get namespace djinn -o name
/workspace/.tmpV2Rm1c/scripts/taskrun-backstop-preflight.sh: 53: kubectl: not found

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
/workspace/.tmpV2Rm1c/scripts/taskrun-backstop-preflight.sh: 53: kubectl: not found

# exit=127
```

#### Jobs read smoke test

```console
$ kubectl get jobs -n djinn -o name --request-timeout=10s
/workspace/.tmpV2Rm1c/scripts/taskrun-backstop-preflight.sh: 53: kubectl: not found

# exit=127
```

#### djinn-server log access smoke test

```console
$ kubectl logs -n djinn deploy/djinn-server --since=10m --tail=20
/workspace/.tmpV2Rm1c/scripts/taskrun-backstop-preflight.sh: 53: kubectl: not found

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

<!-- taskrun-backstop-e2e-evidence.sh: preflight bundle end (exit=1) -->

> Preflight did not pass; the operator environment is missing at least one required prerequisite. The bundle below the preflight is therefore marked **NOT PASS** and the runner did not perform the post-action 60-second polling or claim cleanup success.

## Before-action Kubernetes evidence

- **Action captured at (UTC):** `2026-06-12T12:14:36Z`
- **Namespace:** `djinn`
- **Task id:** `019ebb91-0d17-7d10-932e-718354962d11`
- **Task run id:** `019ebbc0-ddfe-7211-aa82-f31ad032fb49`
- **Label selector key:** `djinn.app/task-run-id`
- **Canonical job/pod prefix:** `djinn-taskrun-`

#### kubectl client availability

```console
$ command -v kubectl || true

kubectl not found; before-action evidence cannot be captured.
```

#### Operator action invocation

- **Action mechanism:** force-close/operator-close (proposal abort, task-admin force-close, or equivalent operator MCP tool)
- **Action target:** `task_id="019ebb91-0d17-7d10-932e-718354962d11"`

The operator must invoke the safe force-close/operator-close mechanism
from the same shell (or record the equivalent transcript) and set
`ACTION_RESULT` / `ACTION_INVOKED_AT` before re-running this script.

The bundle does **not** prescribe a single tool name; record the exact
mechanism used in the bundle via `ACTION_RESULT`. Example safe
operator actions for the runner:

```console
$ curl -sS -H "Authorization: Bearer <redacted operator token>" \
    -H "Content-Type: application/json" \
    -H "Accept: application/json, text/event-stream" \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"task_admin_close","arguments":{"task_id":"$TASK_ID","reason":"task-run backstop e2e verification"}}}' \
    "$DJINN_MCP_URL"
```

or, for a proposal-abort flow, the equivalent of
`proposals <id> abort --reason "task-run backstop e2e verification"`
against the operator/admin endpoint.


## Post-action 60-second cleanup polling

> Preflight did not pass: post-action polling was skipped. Fix the operator environment, rerun the preflight until it passes, and then re-run this script with ACTION_RESULT/ACTION_INVOKED_AT set.

## Server/coordinator log capture

> Preflight did not pass: server log capture was skipped. Fix the operator environment and re-run this script with ACTION_RESULT/ACTION_INVOKED_AT set.

## Bundle interpretation

This bundle is a single Wave 3 evidence attempt. The exact fields
required for review are:

- `TASK_ID` and `TASK_RUN_ID` recorded in the header.
- `NS` and the Kubernetes context (captured inside the preflight
  subsection).
- UTC timestamps for preflight, before-action capture, action
  invocation, and each polling iteration.
- Exact commands, raw output, and exit statuses for every probe.
- A redaction note reminding reviewers that bearer tokens, kubeconfig
  client keys/certificates, cookies, database URLs, and full health
  payloads containing credentials must be removed before committing the
  bundle.

The bundle does **not** claim cleanup success unless all of the
following are true:

- The preflight subsection shows `PRECHECK PASS`.
- The `Operator action invocation` subsection is followed by an
  operator-issued action (or a recorded transcript) and the
  `ACTION_RESULT` header field is populated.
- The `Post-action 60-second cleanup polling` subsection ends with
  `exit=0` and shows `running_pods= jobs= canonical=` for the final
  iteration.

If any of these conditions is false, the bundle records the exact
blocker (operator access, missing MCP credential, missing RBAC, or
remaining Pods/Jobs) and explicitly states that no cleanup success is
claimed.

EVIDENCE RUNNER FAIL: at least one prerequisite is missing or the post-action poll did not converge. The bundle above records the exact failure; do not claim cleanup success for this attempt.
````

#### Wave 3 direct in-cluster API fallback for force-close

The worker also tried a direct Kubernetes API fallback to confirm the operator-access blocker is still in effect in this session. The service-account token is intentionally not pasted.

````md
# Wave 3 wu8h direct API blocker probes

- **Captured at (UTC):** `2026-06-12T12:15:01Z`
- **Task id:** `019ebb91-0d17-7d10-932e-718354962d11`
- **Task run id:** `019ebbc0-ddfe-7211-aa82-f31ad032fb49`
- **Namespace from service account:** `djinn`
- **Purpose:** extra blocker evidence only. These probes do not replace the Wave 3 operator evidence runner and do not claim cleanup success. Tokens are redacted.

## Environment summary

```console
$ command -v kubectl || true
$ printf KUBECONFIG=%s "${KUBECONFIG:-<unset>}"
KUBECONFIG=<unset>
$ ls -l /var/run/secrets/kubernetes.io/serviceaccount/{namespace,token,ca.crt}
lrwxrwxrwx 1 root root 13 Jun 12 12:14 /var/run/secrets/kubernetes.io/serviceaccount/ca.crt -> ..data/ca.crt
lrwxrwxrwx 1 root root 16 Jun 12 12:14 /var/run/secrets/kubernetes.io/serviceaccount/namespace -> ..data/namespace
lrwxrwxrwx 1 root root 12 Jun 12 12:14 /var/run/secrets/kubernetes.io/serviceaccount/token -> ..data/token
```

## Kubernetes API discovery with mounted service-account token

```console
$ curl -sS --cacert <service-account-ca> -H "Authorization: Bearer <redacted>" https://kubernetes.default.svc/api
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

## Kubernetes Pods by task-run label

```console
$ curl -sS --cacert <service-account-ca> -H "Authorization: Bearer <redacted>" "https://kubernetes.default.svc/api/v1/namespaces/djinn/pods?labelSelector=djinn.app/task-run-id%3D019ebbc0-ddfe-7211-aa82-f31ad032fb49"
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
```

## Kubernetes Jobs by task-run label

```console
$ curl -sS --cacert <service-account-ca> -H "Authorization: Bearer <redacted>" "https://kubernetes.default.svc/apis/batch/v1/namespaces/djinn/jobs?labelSelector=djinn.app/task-run-id%3D019ebbc0-ddfe-7211-aa82-f31ad032fb49"
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
```

## Kubernetes canonical Job by name

```console
$ curl -sS --cacert <service-account-ca> -H "Authorization: Bearer <redacted>" "https://kubernetes.default.svc/apis/batch/v1/namespaces/djinn/jobs/djinn-taskrun-019ebbc0-ddfe-7211-aa82-f31ad032fb49"
{
  "kind": "Status",
  "apiVersion": "v1",
  "metadata": {},
  "status": "Failure",
  "message": "jobs.batch \"djinn-taskrun-019ebbc0-ddfe-7211-aa82-f31ad032fb49\" is forbidden: User \"system:serviceaccount:djinn:djinn-djinn-taskrun\" cannot get resource \"jobs\" in API group \"batch\" in the namespace \"djinn\"",
  "reason": "Forbidden",
  "details": {
    "name": "djinn-taskrun-019ebbc0-ddfe-7211-aa82-f31ad032fb49",
    "group": "batch",
    "kind": "jobs"
  },
  "code": 403
}
```

## Djinn MCP endpoint probe

```console
$ env | grep -E "^(DJINN_MCP_URL|DJINN_OPERATOR_BEARER_TOKEN)="
No operator/admin Djinn MCP endpoint or bearer token is configured in this worker. The evidence runner therefore reports <unset DJINN_MCP_URL> and did not invoke force-close.
```
````

#### Wave 3 force-close reconciliation against the acceptance criteria

- **Acceptance criterion 1 (run attempt with evidence runner after preflight, recording task id, task run id, namespace/context, UTC timestamps, and force-close mechanism):** **satisfied as an attempt only.** The runner was invoked with `TASK_ID=019ebb91-0d17-7d10-932e-718354962d11`, `TASK_RUN_ID=019ebbc0-ddfe-7211-aa82-f31ad032fb49`, `NS=djinn`, and `MODE=force-close` at `2026-06-12T12:14:36Z`. The header, preflight, before-action, force-close/operator-close action placeholder, post-action, server log, and bundle interpretation sections were all emitted. The intended safe mechanism is the runner's documented operator/admin force-close path (for example `task_admin_close` or proposal abort), but it was **not invoked** because preflight failed.
- **Acceptance criterion 2 (before-force-close Kubernetes evidence shows the target `djinn-taskrun-*` Job/Pod exists, or the exact safety/access blocker is documented without claiming success):** **operator-access blocker documented.** The preflight subsection shows `kubectl` is not installed (`kubectl: not found`, exit 127 on every `kubectl` probe); the runner therefore emitted `kubectl not found; before-action evidence cannot be captured`. The direct Kubernetes API fallback reached the API but returned 403 for listing Pods, listing Jobs, and getting `jobs.batch/djinn-taskrun-019ebbc0-ddfe-7211-aa82-f31ad032fb49` as `system:serviceaccount:djinn:djinn-djinn-taskrun`. No `djinn-taskrun-*` Job/Pod is proven present; the blocker is recorded explicitly and no success is claimed.
- **Acceptance criterion 3 (after-force-close polling shows no running Pods/Jobs/canonical `djinn-taskrun-$TASK_RUN_ID` resources within ~60 s, or the exact failure/blocker is documented):** **failure/blocker documented.** The runner intentionally skipped the 60-second post-action polling because the preflight did not pass; the bundle states `Preflight did not pass: post-action polling was skipped. Fix the operator environment, rerun the preflight until it passes, and then re-run this script with ACTION_RESULT/ACTION_INVOKED_AT set.` No force-close action was issued, no polling iteration was recorded, and no cleanup success is claimed.
- **Acceptance criterion 4 (relevant server/coordinator logs around the force-close time are captured, including inline teardown/backstop markers where available):** **blocker documented; no logs captured.** The runner states `Preflight did not pass: server log capture was skipped. Fix the operator environment and re-run this script with ACTION_RESULT/ACTION_INVOKED_AT set.` No `deploy/djinn-server` log capture and no inline teardown/backstop markers were produced because this worker has neither `kubectl` nor log RBAC.
- **Acceptance criterion 5 (this doc is updated with a reconciliation statement for whether the force-close/operator-close Kubernetes proof is now satisfied):** **satisfied by this subsection.** The force-close/operator-close Kubernetes proof is **still not satisfied**: Wave 3 only produced a failed-preflight evidence bundle and direct API blocker probes. The remaining gap is the missing operator/admin environment, not a teardown code defect and not an evidence-runner defect.


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
