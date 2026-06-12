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
