#!/bin/sh
# Operator preflight for task-run backstop E2E evidence.
#
# This script checks only whether the operator/admin shell has the prerequisites
# needed to run docs/TASKRUN_BACKSTOP_VERIFICATION.md. It does not kill or
# force-close any task and does not prove task-run cleanup success.

set -eu

NS=${NS:-djinn}
DJINN_SERVER_DEPLOY=${DJINN_SERVER_DEPLOY:-deploy/djinn-server}
DJINN_MCP_URL=${DJINN_MCP_URL:-}
DJINN_OPERATOR_BEARER_TOKEN=${DJINN_OPERATOR_BEARER_TOKEN:-}
SINCE=${SINCE:-10m}
EXIT_STATUS=0

usage() {
    cat <<EOF
Usage: NS=djinn DJINN_MCP_URL=https://.../mcp DJINN_OPERATOR_BEARER_TOKEN=... $0

Environment:
  NS                           Kubernetes namespace to check (default: djinn).
  DJINN_SERVER_DEPLOY           Server deployment log target (default: deploy/djinn-server).
  SINCE                         Log lookback for server log probe (default: 10m).
  DJINN_MCP_URL                 Djinn MCP/control-plane endpoint to authenticate against.
  DJINN_OPERATOR_BEARER_TOKEN   Operator/admin bearer token for the MCP endpoint.

The script prints a Markdown evidence bundle suitable for pasting into
  docs/TASKRUN_BACKSTOP_E2E_EVIDENCE.md
before the kill and force-close evidence sections. It redacts bearer tokens and
only sends a JSON-RPC initialize request to /mcp.
EOF
}

if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
    usage
    exit 0
fi

escape_md_fence() {
    sed 's/```/` ` `/g'
}

run_probe() {
    title=$1
    shift

    printf '\n#### %s\n\n' "$title"
    printf '```console\n'
    printf '$ %s\n' "$*"

    tmp=${TMPDIR:-/var/tmp}/taskrun-backstop-preflight.$$.out
    if "$@" >"$tmp" 2>&1; then
        status=0
    else
        status=$?
    fi
    cat "$tmp" | escape_md_fence
    rm -f "$tmp"
    printf '\n# exit=%s\n' "$status"
    printf '```\n'

    if [ "$status" -ne 0 ]; then
        EXIT_STATUS=1
    fi
}

run_probe_sh() {
    title=$1
    command=$2

    printf '\n#### %s\n\n' "$title"
    printf '```console\n'
    printf '$ %s\n' "$command"

    tmp=${TMPDIR:-/var/tmp}/taskrun-backstop-preflight.$$.out
    if sh -c "$command" >"$tmp" 2>&1; then
        status=0
    else
        status=$?
    fi
    cat "$tmp" | escape_md_fence
    rm -f "$tmp"
    printf '\n# exit=%s\n' "$status"
    printf '```\n'

    if [ "$status" -ne 0 ]; then
        EXIT_STATUS=1
    fi
}

printf '# Task-run backstop operator preflight evidence\n\n'
printf -- '- **Captured at (UTC):** `%s`\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
printf -- '- **Namespace checked:** `%s`\n' "$NS"
printf -- '- **Server log target:** `%s`\n' "$DJINN_SERVER_DEPLOY"
if [ -n "$DJINN_MCP_URL" ]; then
    printf -- '- **Djinn MCP endpoint:** `%s`\n' "$DJINN_MCP_URL"
else
    printf -- '- **Djinn MCP endpoint:** `<unset DJINN_MCP_URL>`\n'
fi
cat <<'EOF'
- **Scope:** preflight only. This bundle proves the operator shell has the ingredients required for the real kill/force-close cleanup checks; it does **not** claim task-run cleanup success.
- **Redaction:** do not paste bearer tokens, kubeconfig client keys/certificates, cookies, database URLs, or full health payloads containing credentials. This script redacts the MCP bearer token by design.
EOF

run_probe "kubectl client availability" kubectl version --client=true
run_probe "current Kubernetes context" kubectl config current-context
run_probe "configured namespace for current context" kubectl config view --minify --output 'jsonpath={..namespace}'
run_probe "target namespace exists" kubectl get namespace "$NS" -o name

run_probe_sh "RBAC: list Pods in target namespace" "answer=\$(kubectl auth can-i list pods -n '$NS'); printf '%s\\n' \"\$answer\"; test \"\$answer\" = yes"
run_probe_sh "RBAC: get Pods in target namespace" "answer=\$(kubectl auth can-i get pods -n '$NS'); printf '%s\\n' \"\$answer\"; test \"\$answer\" = yes"
run_probe_sh "RBAC: list Jobs in target namespace" "answer=\$(kubectl auth can-i list jobs.batch -n '$NS'); printf '%s\\n' \"\$answer\"; test \"\$answer\" = yes"
run_probe_sh "RBAC: get Jobs in target namespace" "answer=\$(kubectl auth can-i get jobs.batch -n '$NS'); printf '%s\\n' \"\$answer\"; test \"\$answer\" = yes"

run_probe "Pods read smoke test" kubectl get pods -n "$NS" -o name --request-timeout=10s
run_probe "Jobs read smoke test" kubectl get jobs -n "$NS" -o name --request-timeout=10s
run_probe "djinn-server log access smoke test" kubectl logs -n "$NS" "$DJINN_SERVER_DEPLOY" --since="$SINCE" --tail=20

printf '\n#### Djinn MCP/control-plane authentication smoke test\n\n'
printf '```console\n'
printf '$ curl -fsS -H "Authorization: Bearer <redacted operator token>" -H "Content-Type: application/json" -d <initialize-json> "$DJINN_MCP_URL"\n'
if [ -z "$DJINN_MCP_URL" ]; then
    printf 'DJINN_MCP_URL is not set. Set it to the operator-accessible /mcp endpoint.\n'
    mcp_status=2
elif [ -z "$DJINN_OPERATOR_BEARER_TOKEN" ]; then
    printf 'DJINN_OPERATOR_BEARER_TOKEN is not set. Set it to an operator/admin token allowed to call kill/force-close tools.\n'
    mcp_status=2
elif ! command -v curl >/dev/null 2>&1; then
    printf 'curl is not installed; cannot probe MCP authentication.\n'
    mcp_status=127
else
    mcp_tmp=${TMPDIR:-/var/tmp}/taskrun-backstop-preflight.$$.mcp
    if curl -fsS \
        -H "Authorization: Bearer $DJINN_OPERATOR_BEARER_TOKEN" \
        -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"taskrun-backstop-operator-preflight","version":"0"}}}' \
        "$DJINN_MCP_URL" >"$mcp_tmp" 2>&1; then
        mcp_status=0
    else
        mcp_status=$?
    fi
    cat "$mcp_tmp" | escape_md_fence
    rm -f "$mcp_tmp"
fi
printf '\n# exit=%s\n' "$mcp_status"
printf '```\n'
if [ "$mcp_status" -ne 0 ]; then
    EXIT_STATUS=1
fi

cat <<'EOF'

## Preflight interpretation

PASS requires every command above to exit 0, including:

- `kubectl` is installed and points at the intended cluster/context.
- The checked namespace is the Djinn runtime namespace that owns `djinn-taskrun-*` Jobs/Pods.
- RBAC allows list/get for Pods and Jobs in that namespace.
- `kubectl logs` can read `deploy/djinn-server` logs.
- The Djinn MCP/control-plane endpoint accepts the operator/admin credential that will be used for `execution_kill_task` and force-close/operator-close actions.

If any item fails, stop and fix the operator environment before running the real cleanup verification. Do not treat this preflight as cleanup proof.
EOF

if [ "$EXIT_STATUS" -eq 0 ]; then
    printf '\nPRECHECK PASS: operator environment is ready for task-run cleanup evidence capture.\n'
else
    printf '\nPRECHECK FAIL: operator environment is missing at least one required prerequisite.\n'
fi

exit "$EXIT_STATUS"
