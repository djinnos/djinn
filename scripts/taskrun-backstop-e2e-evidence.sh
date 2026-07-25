#!/bin/sh
# Operator evidence runner for task-run Kubernetes cleanup checks.
#
# This script emits an auditable Markdown bundle suitable for pasting into
# docs/TASKRUN_BACKSTOP_E2E_EVIDENCE.md under a Wave 3 kill or force-close
# evidence section. It does not perform the kill or force-close action
# itself; the operator is expected to issue the action from the same shell
# (or to capture an action transcript from another channel) and then mark
# the action window in the bundle via the action_* env vars.
#
# The runner is intentionally conservative:
#
# - It reuses scripts/taskrun-backstop-preflight.sh for the prerequisite
#   checks (kubectl, context/namespace, Pod/Job RBAC, server log access,
#   authenticated MCP initialize) and embeds the preflight output as a
#   subsection of the bundle.
# - It captures before-action Kubernetes resources (`kubectl get
#   jobs,pods` filtered by the task-run label and the canonical
#   `djinn-taskrun-$TASK_RUN_ID` prefix) and after-action polling for up
#   to ~60 seconds.
# - It captures `kubectl logs` for deploy/djinn-server filtered around
#   task-run/backstop markers.
# - It redacts the bearer token and never prints the live token value.
# - It fails closed (non-zero exit) when any required input is missing or
#   the operator action window was not acknowledged, and it does **not**
#   claim cleanup success unless the operator marks the bundle as
#   "action=executed" and the post-action poll converges.
#
# Usage:
#
#   NS=djinn \
#   TASK_ID=<long-running-task-id> \
#   TASK_RUN_ID=<active-task-run-id> \
#   DJINN_MCP_URL="https://<operator-accessible-djinn-host>/mcp" \
#   DJINN_OPERATOR_BEARER_TOKEN=<operator/admin token> \
#   MODE=kill \
#     ./scripts/taskrun-backstop-e2e-evidence.sh | tee taskrun-backstop-e2e-kill.md
#
#   NS=djinn \
#   TASK_ID=<long-running-task-id> \
#   TASK_RUN_ID=<active-task-run-id> \
#   DJINN_MCP_URL="https://<operator-accessible-djinn-host>/mcp" \
#   DJINN_OPERATOR_BEARER_TOKEN=<operator/admin token> \
#   MODE=force-close \
#   ACTION_RESULT="proposal abort 4711 closed task" \
#     ./scripts/taskrun-backstop-e2e-evidence.sh | tee taskrun-backstop-e2e-force-close.md
#
# The MODE env var must be `kill` or `force-close`. Setting DRY_RUN=1
# prints the action placeholder + commands and exits 0 without doing
# anything destructive; the produced bundle is then explicitly marked as
# a dry run and is not a cleanup claim.

set -eu

# -------- defaults & argument parsing ---------------------------------

NS=${NS:-djinn}
DJINN_SERVER_DEPLOY=${DJINN_SERVER_DEPLOY:-deploy/djinn-server}
DJINN_MCP_URL=${DJINN_MCP_URL:-}
DJINN_OPERATOR_BEARER_TOKEN=${DJINN_OPERATOR_BEARER_TOKEN:-}
TASK_ID=${TASK_ID:-}
TASK_RUN_ID=${TASK_RUN_ID:-}
MODE=${MODE:-}
ACTION_RESULT=${ACTION_RESULT:-}
ACTION_INVOKED_AT=${ACTION_INVOKED_AT:-}
DRY_RUN=${DRY_RUN:-0}
SINCE=${SINCE:-10m}
POLL_INTERVAL=${POLL_INTERVAL:-5}
POLL_TIMEOUT=${POLL_TIMEOUT:-60}
LOG_TAIL=${LOG_TAIL:-200}
LABEL_SELECTOR_KEY=${LABEL_SELECTOR_KEY:-djinn.app/task-run-id}
CANONICAL_JOB_PREFIX=${CANONICAL_JOB_PREFIX:-djinn-taskrun-}

EXIT_STATUS=0
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)

usage() {
    cat <<EOF
Usage: NS=djinn TASK_ID=... TASK_RUN_ID=... MODE=kill|force-close [DRY_RUN=1] \\
            DJINN_MCP_URL=... DJINN_OPERATOR_BEARER_TOKEN=... $0

Required environment:
  NS                           Kubernetes namespace that owns the
                               djinn-taskrun-* Jobs/Pods (default: djinn).
  TASK_ID                      Djinn task id passed to execution_kill_task
                               (or that the force-close targets).
  TASK_RUN_ID                  Active task-run id whose Kubernetes
                               resources the bundle should capture.
  MODE                         kill or force-close.

Optional environment:
  DJINN_SERVER_DEPLOY          Server log target (default: deploy/djinn-server).
  DJINN_MCP_URL                Operator-accessible Djinn MCP/control-plane
                               endpoint, used for the preflight MCP
                               initialize probe.
  DJINN_OPERATOR_BEARER_TOKEN  Operator/admin bearer token (redacted in
                               the bundle). Required for the preflight
                               MCP probe unless the operator wants the
                               preflight to fail closed.
  ACTION_RESULT                Free-form record of the operator action
                               result (e.g. "execution_kill_task returned
                               ok", "proposal abort 4711 closed task").
  ACTION_INVOKED_AT            UTC timestamp when the operator action
                               was issued. If unset and the bundle is
                               not in DRY_RUN mode, the runner still
                               records the action window using
                               ACTION_INVOKED_AT=<start-of-poll>.
  SINCE                        Log lookback for the server log probe
                               (default: 10m).
  POLL_INTERVAL                Seconds between post-action polls
                               (default: 5).
  POLL_TIMEOUT                 Total post-action poll window in seconds
                               (default: 60).
  LOG_TAIL                     Tail length for the post-action server
                               log capture (default: 200).
  LABEL_SELECTOR_KEY           Label key used to select the
                               task-run's Pods and Jobs (default:
                               djinn.app/task-run-id).
  CANONICAL_JOB_PREFIX         Prefix used for canonical Djinn
                               task-run Job/Pod names (default:
                               djinn-taskrun-).
  DRY_RUN                      1 to skip the live action and only emit
                               the action placeholder + commands; the
                               bundle is then marked DRY_RUN and is
                               not a cleanup claim.

The script prints a Markdown evidence bundle suitable for pasting into
docs/TASKRUN_BACKSTOP_E2E_EVIDENCE.md under the Wave 3 kill or
force-close evidence subsection. Secrets are redacted; the bundle
records task id, task_run_id, namespace/context, UTC timestamps, exact
commands, and exit statuses.
EOF
}

if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
    usage
    exit 0
fi

# -------- input validation -------------------------------------------

if [ -z "$TASK_ID" ]; then
    printf 'ERROR: TASK_ID is required.\n' >&2
    usage >&2
    exit 2
fi

if [ -z "$TASK_RUN_ID" ]; then
    printf 'ERROR: TASK_RUN_ID is required.\n' >&2
    usage >&2
    exit 2
fi

case "$MODE" in
    kill|force-close) ;;
    *)
        printf 'ERROR: MODE must be "kill" or "force-close" (got: %s).\n' "$MODE" >&2
        usage >&2
        exit 2
        ;;
esac

# -------- helpers -----------------------------------------------------

escape_md_fence() {
    sed 's/```/` ` `/g'
}

now_utc() {
    date -u +%Y-%m-%dT%H:%M:%SZ
}

section_header() {
    printf '\n## %s\n\n' "$1"
}

subsection_header() {
    printf '\n#### %s\n\n' "$1"
}

# record_sh FILE TITLE COMMAND_STRING
#
# Runs the shell command string with `sh -c`, writes its combined
# output to FILE, and emits a Markdown subsection showing the command,
# the captured output, and the exit status. Using `sh -c` keeps quoting
# intact and matches the pattern already established in
# taskrun-backstop-preflight.sh for commands that need shell expansion.
record_sh() {
    out_basename=$1
    title=$2
    command_string=$3

    subsection_header "$title"
    printf '```console\n'
    printf '$ %s\n' "$command_string"

    tmp=${TMPDIR:-/var/tmp}/"${out_basename}".out
    if sh -c "$command_string" >"$tmp" 2>&1; then
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
    return "$status"
}

# emit_action_placeholder MODE
#
# Prints the placeholder JSON / command lines for the operator action
# so the bundle always contains an auditable record of what was supposed
# to be invoked. In DRY_RUN=1 mode we explicitly mark the bundle as a
# dry run; otherwise the operator is expected to run the action from the
# same shell and set ACTION_RESULT/ACTION_INVOKED_AT for the runner to
# include.
emit_action_placeholder() {
    mode=$1
    subsection_header "Operator action invocation"
    if [ "$mode" = "kill" ]; then
        printf -- '- **Action tool:** `execution_kill_task`\n'
        printf -- '- **Action arguments:** `task_id="%s"`, `reason="task-run backstop e2e verification"`\n' "$TASK_ID"
        cat <<'EOF'

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
EOF
    else
        printf -- '- **Action mechanism:** force-close/operator-close (proposal abort, task-admin force-close, or equivalent operator MCP tool)\n'
        printf -- '- **Action target:** `task_id="%s"`\n' "$TASK_ID"
        cat <<'EOF'

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
EOF
    fi
    printf '\n'
}

# capture_before_action
#
# Captures the Kubernetes resources that exist for the target task run
# before the operator action. Records the canonical Job name, Pods
# selected by the task-run label, and Jobs selected by the same label.
capture_before_action() {
    section_header "Before-action Kubernetes evidence"
    printf -- '- **Action captured at (UTC):** `%s`\n' "$(now_utc)"
    cat <<EOF
- **Namespace:** \`$NS\`
- **Task id:** \`$TASK_ID\`
- **Task run id:** \`$TASK_RUN_ID\`
- **Label selector key:** \`$LABEL_SELECTOR_KEY\`
- **Canonical job/pod prefix:** \`$CANONICAL_JOB_PREFIX\`
EOF

    if ! command -v kubectl >/dev/null 2>&1; then
        subsection_header "kubectl client availability"
        printf '```console\n$ command -v kubectl || true\n\nkubectl not found; before-action evidence cannot be captured.\n```\n'
        EXIT_STATUS=1
        return
    fi

    record_sh before-context \
        "Current Kubernetes context" \
        "kubectl config current-context"

    record_sh before-jobs-pods-label \
        "Pods + Jobs by task-run label" \
        "kubectl get jobs,pods -n '$NS' -l '$LABEL_SELECTOR_KEY=$TASK_RUN_ID' -o wide"

    record_sh before-running-pods \
        "Running Pods by task-run label" \
        "kubectl get pods -n '$NS' --field-selector=status.phase=Running -l '$LABEL_SELECTOR_KEY=$TASK_RUN_ID' -o name"

    record_sh before-canonical-all \
        "All djinn-taskrun-* Jobs and Pods in the namespace" \
        "kubectl get jobs,pods -n '$NS' -o name"

    record_sh before-canonical-grep \
        "Canonical djinn-taskrun-\$TASK_RUN_ID resources (filtered)" \
        "kubectl get jobs,pods -n '$NS' -o name | grep -E '(^|/)${CANONICAL_JOB_PREFIX}${TASK_RUN_ID}(\$|-)' || true"
}

# poll_for_cleanup
#
# Polls the cluster for up to POLL_TIMEOUT seconds, checking both Pods
# selected by the task-run label and the canonical
# `djinn-taskrun-$TASK_RUN_ID` Job/Pod names. Exits early when both
# selectors return empty.
poll_for_cleanup() {
    section_header "Post-action 60-second cleanup polling"
    printf -- '- **Poll started at (UTC):** `%s`\n' "$(now_utc)"
    printf -- '- **Poll timeout (s):** `%s`\n' "$POLL_TIMEOUT"
    printf -- '- **Poll interval (s):** `%s`\n' "$POLL_INTERVAL"
    printf -- '- **Namespace:** `%s`\n' "$NS"
    printf -- '- **Task run id:** `%s`\n' "$TASK_RUN_ID"

    if ! command -v kubectl >/dev/null 2>&1; then
        subsection_header "kubectl client availability"
        printf '```console\n$ command -v kubectl || true\n\nkubectl not found; post-action polling cannot be performed.\n```\n'
        EXIT_STATUS=1
        return
    fi

    # Render the polling as a single fenced block that contains all
    # iterations the script performed. Each iteration shows the
    # selectors and the result, so the bundle is auditable even if the
    # script is interrupted.
    printf '\n```console\n'
    printf '$ # deadline = NOW + %s seconds; sleep %s between iterations\n' \
        "$POLL_TIMEOUT" "$POLL_INTERVAL"
    printf -- '$ # selectors: pods by label, jobs by label, canonical name match for %s%s\n' \
        "$CANONICAL_JOB_PREFIX" "$TASK_RUN_ID"

    # shellcheck disable=SC2034
    started_epoch=$(date +%s)
    deadline=$((started_epoch + POLL_TIMEOUT))
    iter=0
    converged=0
    final_pods=
    final_jobs=
    final_canonical=
    while :; do
        iter=$((iter + 1))
        now_epoch=$(date +%s)
        now_utc_str=$(now_utc)

        # Always run at least one poll iteration so POLL_TIMEOUT=0 still
        # captures a single probe and reports its result rather than
        # silently failing. After that, stop when the deadline is met.
        if [ "$iter" -gt 1 ] && [ "$now_epoch" -ge "$deadline" ]; then
            printf '\n[%s] iter=%s deadline reached after %s seconds\n' \
                "$now_utc_str" "$iter" "$POLL_TIMEOUT"
            break
        fi

        running_pods=$(kubectl get pods -n "$NS" \
            --field-selector=status.phase=Running \
            -l "$LABEL_SELECTOR_KEY=$TASK_RUN_ID" \
            -o name 2>/dev/null || true)
        jobs=$(kubectl get jobs -n "$NS" \
            -l "$LABEL_SELECTOR_KEY=$TASK_RUN_ID" \
            -o name 2>/dev/null || true)
        canonical=$(kubectl get jobs,pods -n "$NS" \
            -o name 2>/dev/null \
            | grep -E "(^|/)${CANONICAL_JOB_PREFIX}${TASK_RUN_ID}(\$|-)" \
            || true)

        printf '\n[%s] iter=%s\n' "$now_utc_str" "$iter"
        printf '  running_pods=%s\n' "$running_pods"
        printf '  jobs=%s\n' "$jobs"
        printf '  canonical=%s\n' "$canonical"

        final_pods=$running_pods
        final_jobs=$jobs
        final_canonical=$canonical

        if [ -z "$running_pods" ] && [ -z "$jobs" ] && [ -z "$canonical" ]; then
            printf -- '  -> PASS: no running task-run pod/job remains for %s\n' "$TASK_RUN_ID"
            converged=1
            break
        fi

        # Don't sleep after the final iteration that hit the deadline.
        if [ "$now_epoch" -lt "$deadline" ]; then
            sleep "$POLL_INTERVAL" || true
        fi
    done

    if [ "$converged" -ne 1 ]; then
        printf '\n  -> FAIL: task-run resources still visible after ~%s seconds\n' "$POLL_TIMEOUT"
        printf '\n# final poll: running_pods=[%s] jobs=[%s] canonical=[%s]\n' \
            "$final_pods" "$final_jobs" "$final_canonical"
        printf '# exit=%s\n' "1"
        printf '```\n'
        EXIT_STATUS=1
        return
    fi

    printf '\n# final poll: running_pods= jobs= canonical=\n'
    printf '# exit=%s\n' "0"
    printf '```\n'
}

# capture_server_logs
#
# Captures `kubectl logs` for the djinn-server deployment filtered
# around task-run/backstop markers. Never includes raw tokens or
# credentials; the substring filter discards most health payloads.
capture_server_logs() {
    section_header "Server/coordinator log capture"
    printf -- '- **Log target:** `%s`\n' "$DJINN_SERVER_DEPLOY"
    printf -- '- **Log since:** `%s`\n' "$SINCE"
    printf -- '- **Tail length:** `%s`\n' "$LOG_TAIL"
    cat <<EOF
- **Log filter:** task-run Job backstop markers, task_run_id=\`$TASK_RUN_ID\`, and job_name references.

The expected backstop log markers are described in
\`docs/TASKRUN_BACKSTOP_VERIFICATION.md\` and include \`reason\`
(\`startup\` / \`periodic\` / explicit backstop test reason),
\`job_name\`, \`task_run_id\`, and \`outcome\`
(\`Live\` / \`Success\` / \`Failure\`, from the shared
\`djinn_core::job_retention\` classifier).
EOF

    if ! command -v kubectl >/dev/null 2>&1; then
        subsection_header "kubectl client availability"
        printf '```console\n$ command -v kubectl || true\n\nkubectl not found; server log capture cannot be performed.\n```\n'
        EXIT_STATUS=1
        return
    fi

    record_sh logs-since \
        "deploy/djinn-server --since=${SINCE} --tail=${LOG_TAIL}" \
        "kubectl logs -n '$NS' '$DJINN_SERVER_DEPLOY' --since='$SINCE' --tail='$LOG_TAIL'"

    record_sh logs-filter \
        "Backstop + task_run_id filtered server logs" \
        "kubectl logs -n '$NS' '$DJINN_SERVER_DEPLOY' --since='$SINCE' --tail='$LOG_TAIL' | grep -E 'task-run Job backstop|backstop reaped orphaned task-run Job|task_run_id=$TASK_RUN_ID|job_name' || true"
}

# run_preflight
#
# Reuses scripts/taskrun-backstop-preflight.sh to validate the operator
# shell has kubectl, RBAC, and authenticated MCP access. The preflight
# output is embedded in the bundle as a subsection, but the preflight
# is required to PASS before the runner is allowed to mark the bundle
# as a real evidence bundle.
run_preflight() {
    section_header "Operator preflight (reused from taskrun-backstop-preflight.sh)"

    preflight="$SCRIPT_DIR/taskrun-backstop-preflight.sh"
    if [ ! -x "$preflight" ]; then
        printf 'ERROR: %s is missing or not executable; cannot reuse preflight.\n' "$preflight" >&2
        EXIT_STATUS=1
        subsection_header "Preflight status"
        cat <<EOF
\`$preflight\` is missing or not executable. The preflight cannot be
reused, so the bundle below the preflight is marked **NOT PASS** and
the runner did not perform the post-action 60-second polling or claim
cleanup success.
EOF
        return 1
    fi

    subsection_header "Preflight output"
    cat <<EOF
The preflight helper was invoked with NS=\`$NS\`,
DJINN_SERVER_DEPLOY=\`$DJINN_SERVER_DEPLOY\`,
DJINN_MCP_URL=\`$DJINN_MCP_URL\`, and the operator token redacted. The
helper's complete Markdown bundle is embedded below verbatim (no outer
code fence) so the inner \`console\` blocks stay readable.
EOF

    tmp=${TMPDIR:-/var/tmp}/taskrun-backstop-e2e-preflight.out
    if NS="$NS" \
       DJINN_SERVER_DEPLOY="$DJINN_SERVER_DEPLOY" \
       DJINN_MCP_URL="$DJINN_MCP_URL" \
       DJINN_OPERATOR_BEARER_TOKEN="$DJINN_OPERATOR_BEARER_TOKEN" \
       SINCE="$SINCE" \
       sh "$preflight" >"$tmp" 2>&1; then
        preflight_status=0
    else
        preflight_status=$?
    fi

    # The preflight output is itself a complete Markdown document with
    # its own code fences, so we embed it verbatim (no outer code
    # fence) and tag the start/end so reviewers can copy it out cleanly.
    printf '\n<!-- taskrun-backstop-e2e-evidence.sh: preflight bundle begin -->\n\n'
    cat "$tmp"
    rm -f "$tmp"
    printf '\n<!-- taskrun-backstop-e2e-evidence.sh: preflight bundle end (exit=%s) -->\n' "$preflight_status"

    if [ "$preflight_status" -ne 0 ]; then
        printf '\n> Preflight did not pass; the operator environment is missing at least one required prerequisite. The bundle below the preflight is therefore marked **NOT PASS** and the runner did not perform the post-action 60-second polling or claim cleanup success.\n'
        EXIT_STATUS=1
        return 1
    fi

    printf '\n> Preflight passed; the operator environment has kubectl, context/namespace, Pod/Job RBAC, server log access, and authenticated MCP access for the kill/force-close action.\n'
    return 0
}

# -------- main bundle assembly ---------------------------------------

ACTION_MODE_DISPLAY=$MODE
case "$MODE" in
    kill) ACTION_MODE_DISPLAY="execution_kill_task" ;;
    force-close) ACTION_MODE_DISPLAY="force-close/operator-close" ;;
esac

if [ "$DRY_RUN" = "1" ]; then
    printf '# Task-run backstop operator evidence (DRY RUN)\n\n'
else
    printf -- '# Task-run backstop operator evidence — %s\n\n' "$ACTION_MODE_DISPLAY"
fi

printf -- '- **Captured at (UTC):** `%s`\n' "$(now_utc)"
printf -- '- **Mode:** `%s`\n' "$MODE"
printf -- '- **Action mode display:** `%s`\n' "$ACTION_MODE_DISPLAY"
printf -- '- **Namespace:** `%s`\n' "$NS"
printf -- '- **Task id:** `%s`\n' "$TASK_ID"
printf -- '- **Task run id:** `%s`\n' "$TASK_RUN_ID"
printf -- '- **Server log target:** `%s`\n' "$DJINN_SERVER_DEPLOY"
if [ -n "$DJINN_MCP_URL" ]; then
    printf -- '- **Djinn MCP endpoint:** `%s`\n' "$DJINN_MCP_URL"
else
    printf -- '- **Djinn MCP endpoint:** `<unset DJINN_MCP_URL>`\n'
fi
if [ "$DRY_RUN" = "1" ]; then
    printf -- '- **DRY_RUN:** `1` (no action was performed; the bundle is a placeholder, not a cleanup claim)\n'
else
    printf -- '- **DRY_RUN:** `0`\n'
fi
cat <<'EOF'
- **Redaction:** do not paste bearer tokens, kubeconfig client keys/certificates, cookies, database URLs, or full health payloads containing credentials. This script redacts the MCP bearer token by design.
- **Scope:** this bundle captures the operator-side evidence for one Wave 3 cleanup attempt. It does **not** claim cleanup success unless the post-action polling converges and the operator records the action result.
EOF

if [ -n "$ACTION_INVOKED_AT" ]; then
    printf -- '- **Operator action invoked at (UTC):** `%s`\n' "$ACTION_INVOKED_AT"
fi
if [ -n "$ACTION_RESULT" ]; then
    printf -- '- **Operator action result:** `%s`\n' "$ACTION_RESULT"
fi

# 1. Run the preflight. If it fails, the bundle still contains the
#    before-action evidence and the action placeholder so reviewers can
#    see exactly which prerequisite was missing, but the post-action
#    polling and log capture are skipped to avoid false success.
preflight_passed=0
if run_preflight; then
    preflight_passed=1
fi

# 2. Capture before-action Kubernetes evidence. We always capture this
#    so the bundle has an auditable record of what existed immediately
#    before the action, even if the action was blocked.
capture_before_action

# 3. Emit the action placeholder. The operator is expected to invoke
#    the action from the same shell (or to record an action transcript)
#    and then re-run the script with ACTION_RESULT/ACTION_INVOKED_AT.
emit_action_placeholder "$MODE"

# 4. If preflight passed and we are not in DRY_RUN, run the 60-second
#    post-action polling and capture the server logs. Otherwise skip
#    those steps and explicitly mark the bundle as not claiming
#    success.
if [ "$preflight_passed" -eq 1 ] && [ "$DRY_RUN" != "1" ]; then
    if [ -z "$ACTION_INVOKED_AT" ]; then
        ACTION_INVOKED_AT=$(now_utc)
        printf -- '\n- **Operator action invoked at (UTC, auto-recorded at poll start):** `%s`\n' "$ACTION_INVOKED_AT"
    fi
    if [ -z "$ACTION_RESULT" ]; then
        printf -- '- **Operator action result:** `<unset ACTION_RESULT; set this when the action was issued, even if the preflight passed>`\n'
        EXIT_STATUS=1
    fi
    poll_for_cleanup
    capture_server_logs
else
    section_header "Post-action 60-second cleanup polling"
    if [ "$DRY_RUN" = "1" ]; then
        printf '> DRY_RUN=1: post-action polling was skipped. Re-run the script without DRY_RUN after the operator action has been issued and ACTION_RESULT/ACTION_INVOKED_AT have been recorded.\n'
    else
        printf '> Preflight did not pass: post-action polling was skipped. Fix the operator environment, rerun the preflight until it passes, and then re-run this script with ACTION_RESULT/ACTION_INVOKED_AT set.\n'
    fi

    section_header "Server/coordinator log capture"
    if [ "$DRY_RUN" = "1" ]; then
        printf '> DRY_RUN=1: server log capture was skipped. Re-run the script without DRY_RUN after the operator action has been issued.\n'
    else
        printf '> Preflight did not pass: server log capture was skipped. Fix the operator environment and re-run this script with ACTION_RESULT/ACTION_INVOKED_AT set.\n'
    fi
fi

# 5. Final reconciliation block.
section_header "Bundle interpretation"
cat <<'EOF'
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
EOF

if [ "$EXIT_STATUS" -eq 0 ] && [ "$preflight_passed" -eq 1 ] && [ "$DRY_RUN" != "1" ]; then
    printf '\nEVIDENCE RUNNER PASS: preflight, before-action capture, and post-action polling all completed cleanly. Review the action result and the backstop log markers to confirm whether cleanup success should be claimed for this attempt.\n'
else
    printf '\nEVIDENCE RUNNER FAIL: at least one prerequisite is missing or the post-action poll did not converge. The bundle above records the exact failure; do not claim cleanup success for this attempt.\n'
fi

exit "$EXIT_STATUS"
