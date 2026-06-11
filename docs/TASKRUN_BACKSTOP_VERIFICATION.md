# Task-run backstop verification runbook

This runbook verifies the operational safety criterion for the Kubernetes task-run backstop: after a worker task is killed or force-closed, no running `djinn-taskrun-*` Pod/Job for that `task_run_id` remains after roughly one minute.

## Preconditions

- A real Kubernetes-backed Djinn deployment is running.
- `kubectl` points at the cluster/namespace that runs Djinn. Set `NS=djinn` below if your namespace differs.
- You can call the Djinn control-plane tools (for example through MCP) including `execution_kill_task`. For proposal abort verification, you can call the force-close/abort path that closes the task.
- The target task should be long-running enough that its worker Pod is still active before the kill/force-close is issued.

```bash
export NS=djinn
```

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
