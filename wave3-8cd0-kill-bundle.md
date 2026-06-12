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
