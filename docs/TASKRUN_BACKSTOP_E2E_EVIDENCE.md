# Task-run backstop E2E evidence

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
