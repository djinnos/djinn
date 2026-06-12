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
