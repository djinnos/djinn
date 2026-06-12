# Task-run backstop verification runbook

This runbook verifies the operational safety criterion for the Kubernetes task-run backstop: after a worker task is killed or force-closed, no running `djinn-taskrun-*` Pod/Job for that `task_run_id` remains after roughly one minute.

The manual steps in §1–§4 are still the source of truth for the procedure. The recommended way to capture a Wave 3 evidence bundle from an operator/admin environment is `scripts/taskrun-backstop-e2e-evidence.sh`, which wraps §0–§4 in a single Markdown bundle and pastes cleanly into `docs/TASKRUN_BACKSTOP_E2E_EVIDENCE.md`. See [Wave 3 operator evidence runner](#wave-3-operator-evidence-runner) for the runner usage and paste point.

## Preconditions

- A real Kubernetes-backed Djinn deployment is running.
- Run this from an operator/admin shell, not from a normal task-run worker. Wave 1 evidence showed workers can be missing `kubectl`/`KUBECONFIG`, can lack RBAC to read Pods/Jobs/logs, and can have projected tokens that `/mcp` rejects for operator actions.
- `kubectl` points at the cluster/namespace that runs Djinn. Set `NS=djinn` below if your namespace differs.
- Your Kubernetes identity can list/get Pods and Jobs in the Djinn namespace and can read `deploy/djinn-server` logs.
- You can call the Djinn control-plane tools (for example through MCP) including `execution_kill_task`. For proposal abort verification, you can call the force-close/abort path that closes the task.
- The target task should be long-running enough that its worker Pod is still active before the kill/force-close is issued.

```bash
export NS=djinn
```

## Wave 3 operator evidence runner

The Wave 3 evidence runner, `scripts/taskrun-backstop-e2e-evidence.sh`, captures preflight, before-action Kubernetes resources, an action invocation placeholder, the 60-second post-action polling loop, and the filtered server log capture described in §0–§4, and emits a single auditable Markdown bundle. It supports two modes:

- `MODE=kill` for the `execution_kill_task` verification path.
- `MODE=force-close` for the operator/admin force-close or proposal-abort verification path.

The runner reuses `scripts/taskrun-backstop-preflight.sh` and refuses to perform the post-action 60-second polling or claim cleanup success unless the preflight passes. It redacts the operator bearer token, records the task id, task run id, namespace/context, UTC timestamps, exact commands, and exit statuses needed for review, and fails closed (non-zero exit) when required inputs/access are missing. `DRY_RUN=1` produces the same bundle marked as a dry run, so operators can rehearse the paste workflow without invoking any action.

Run the kill bundle (in an operator/admin shell with the preflight prerequisites):

```bash
export NS=djinn
export TASK_ID="<long-running-task-id>"
export TASK_RUN_ID="<active-task-run-id>"
export DJINN_MCP_URL="https://<operator-accessible-djinn-host>/mcp"
export DJINN_OPERATOR_BEARER_TOKEN="<operator/admin token>"

# Dry-run first to confirm the bundle shape and field substitution:
MODE=kill DRY_RUN=1 ./scripts/taskrun-backstop-e2e-evidence.sh | tee /tmp/taskrun-backstop-e2e-kill.dryrun.md

# After invoking execution_kill_task from the same shell, record the
# action and re-run for the real bundle:
export ACTION_INVOKED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
export ACTION_RESULT="execution_kill_task returned ok"
MODE=kill ./scripts/taskrun-backstop-e2e-evidence.sh | tee taskrun-backstop-e2e-kill.md
```

Run the force-close bundle (in an operator/admin shell with the preflight prerequisites):

```bash
export NS=djinn
export TASK_ID="<long-running-task-id>"
export TASK_RUN_ID="<active-task-run-id>"
export DJINN_MCP_URL="https://<operator-accessible-djinn-host>/mcp"
export DJINN_OPERATOR_BEARER_TOKEN="<operator/admin token>"

# Dry-run:
MODE=force-close DRY_RUN=1 ./scripts/taskrun-backstop-e2e-evidence.sh | tee /tmp/taskrun-backstop-e2e-force-close.dryrun.md

# After invoking the operator/admin force-close or proposal abort from
# the same shell, record the action and re-run for the real bundle:
export ACTION_INVOKED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
export ACTION_RESULT="proposal abort 4711 closed task"
MODE=force-close ./scripts/taskrun-backstop-e2e-evidence.sh | tee taskrun-backstop-e2e-force-close.md
```

After redacting secrets, paste the generated bundle into `docs/TASKRUN_BACKSTOP_E2E_EVIDENCE.md` under the Wave 3 kill or force-close evidence section. The bundle does not claim cleanup success unless all of the following are true: the embedded preflight shows `PRECHECK PASS`, the action was actually issued (or its transcript is recorded) and `ACTION_RESULT` is set, and the post-action 60-second polling subsection ends with `exit=0` and empty `running_pods`, `jobs`, and `canonical` lines for the final iteration.

For the rest of this runbook, §0–§4 remain the source of truth for what the runner is doing and for any operator who needs to capture the evidence by hand.

## 0. Capture operator preflight evidence

Before creating the long-running evidence task, prove that the operator shell has the required Kubernetes and Djinn control-plane access. This preflight does **not** kill or force-close anything and must not be cited as cleanup success by itself.

```bash
export NS=djinn
export DJINN_MCP_URL="https://<operator-accessible-djinn-host>/mcp"
export DJINN_OPERATOR_BEARER_TOKEN="<operator/admin token>"
./scripts/taskrun-backstop-preflight.sh | tee taskrun-backstop-preflight.md
```

Paste the complete Markdown output into `docs/TASKRUN_BACKSTOP_E2E_EVIDENCE.md` under the operator preflight section before running the kill and force-close checks. Redact secrets before pasting: bearer tokens, kubeconfig client keys/certificates, cookies, database URLs, and full health payloads containing credentials. The helper redacts the MCP bearer token in the displayed command, but operators remain responsible for reviewing the captured output.

The preflight must show:

- `kubectl version --client=true` exits 0;
- current context and namespace are the intended Djinn cluster/namespace;
- `kubectl auth can-i` allows list/get for Pods and Jobs in `$NS`;
- `kubectl get pods` and `kubectl get jobs` smoke tests succeed;
- `kubectl logs -n "$NS" deploy/djinn-server ...` succeeds;
- the MCP `initialize` request to `$DJINN_MCP_URL` succeeds with the same operator/admin credential that will be used for `execution_kill_task` and force-close/operator-close.

If any preflight item fails, stop and fix the operator environment before continuing. Do not grant broad runtime permissions to task-run workers as a workaround unless a separate, narrowly scoped, reviewed deployment change already exists for that purpose.

## 1. Start and identify a long-running task run

1. Create or select a worker task that will run for several minutes.
2. Wait until the task has an active session/task run.
3. Record both IDs:
   - `TASK_ID`: Djinn task id or short id used by `execution_kill_task`.
   - `TASK_RUN_ID`: the active task-run id from the session/task-run record, UI, activity log, or server logs.

Confirm the Kubernetes resources exist before killing the task:

```bash
kubectl get jobs,pods -n "$NS" -l "djinn.app/task-run-id=$TASK_RUN_ID" -o wide
kubectl get pods -n "$NS" --field-selector=status.phase=Running \
  -l "djinn.app/task-run-id=$TASK_RUN_ID" -o name
```

At least one `djinn-taskrun-$TASK_RUN_ID` Job/Pod should be visible while the worker is running.

## 2. Kill or force-close the task

### Kill path

Invoke the control-plane tool:

```json
{
  "tool": "execution_kill_task",
  "arguments": {
    "task_id": "$TASK_ID",
    "reason": "task-run backstop e2e verification"
  }
}
```

### Force-close path

Use the operator/admin path that force-closes the task (for example a proposal abort or task-admin force-close). Record the same `TASK_RUN_ID` that was active immediately before the close.

## 3. Assert cleanup within about 60 seconds

Poll for up to 60 seconds. This checks both Pods selected by the task-run label and the canonical Job name prefix.

```bash
deadline=$((SECONDS + 60))
while [ "$SECONDS" -lt "$deadline" ]; do
  running_pods=$(kubectl get pods -n "$NS" \
    --field-selector=status.phase=Running \
    -l "djinn.app/task-run-id=$TASK_RUN_ID" \
    -o name)
  jobs=$(kubectl get jobs -n "$NS" \
    -l "djinn.app/task-run-id=$TASK_RUN_ID" \
    -o name)
  canonical=$(kubectl get jobs,pods -n "$NS" \
    -o name | grep -E "(^|/)djinn-taskrun-$TASK_RUN_ID($|-)" || true)

  if [ -z "$running_pods" ] && [ -z "$jobs" ] && [ -z "$canonical" ]; then
    echo "PASS: no running task-run pod/job remains for $TASK_RUN_ID"
    exit 0
  fi

  echo "waiting for task-run cleanup: pods=[$running_pods] jobs=[$jobs] canonical=[$canonical]"
  sleep 5
done

echo "FAIL: task-run resources still visible after ~60s"
kubectl get jobs,pods -n "$NS" -l "djinn.app/task-run-id=$TASK_RUN_ID" -o wide
kubectl get jobs,pods -n "$NS" -o name | grep "djinn-taskrun-$TASK_RUN_ID" || true
exit 1
```

## 4. Log evidence to capture on failure

If the assertion fails, capture server/coordinator logs around the kill time. Backstop cleanup logs are distinguishable from inline teardown logs and include:

- `reason` (`startup`, `periodic`, or an explicit backstop test reason)
- `job_name`
- `task_run_id`
- `db_classification` (`absent`, `session_interrupted`, `task_run_completed`, `task_run_interrupted`, etc.)

```bash
kubectl logs -n "$NS" deploy/djinn-server --since=10m | \
  grep -E "task-run Job backstop|backstop reaped orphaned task-run Job|task_run_id=$TASK_RUN_ID"
```

A passing run demonstrates that inline teardown plus the startup/periodic backstop leave no running `djinn-taskrun-*` resources for killed or force-closed task runs within the expected operational window.
