#!/bin/sh
# Emit a local, operator-fillable Markdown evidence bundle for the
# JIT pitfall cohort effectiveness read.
#
# Safety model:
# - This helper never connects to production and never queries telemetry.
# - It emits placeholders plus operator-supplied safe scalar values/references.
# - It must not ingest prompt text, patch/source contents, or full rendered
#   JIT hint bodies. Do not pass raw logs or source/patch files to it.
# - Accepted inputs are limited to counts, rates, ids, rollout metadata,
#   bounded path summaries, safe note metadata, and references/paths to
#   already-redacted summaries.

set -eu

DRY_RUN=${DRY_RUN:-1}
READOUT_ID=${READOUT_ID:-}
ENVIRONMENT=${ENVIRONMENT:-}
COHORT_RULE=${COHORT_RULE:-}
ROLLOUT_WINDOW_UTC=${ROLLOUT_WINDOW_UTC:-}
CONFIG_APPLIED=${CONFIG_APPLIED:-}
KILL_SWITCH_REF=${KILL_SWITCH_REF:-}
KILL_SWITCH_TESTED=${KILL_SWITCH_TESTED:-}
OPERATOR=${OPERATOR:-}
TELEMETRY_COUNTERS_REF=${TELEMETRY_COUNTERS_REF:-}
EFFECTIVENESS_REF=${EFFECTIVENESS_REF:-}
NOISE_SAMPLE_REF=${NOISE_SAMPLE_REF:-}
PROMPT_BUDGET_REF=${PROMPT_BUDGET_REF:-}
NOTE_DISTRIBUTION_REF=${NOTE_DISTRIBUTION_REF:-}
EMPTY_ERROR_DISABLED_REF=${EMPTY_ERROR_DISABLED_REF:-}

usage() {
    cat <<'EOF'
Usage: [safe env vars] ./scripts/jit-pitfall-readout-bundle.sh > jit-readout.md

Emits a Markdown skeleton for docs/JIT_PITFALL_EFFECTIVENESS_READ.md.
Default mode and DRY_RUN=1 are local template-only modes; the helper does not
connect to production telemetry or read raw logs.

Optional safe scalar/reference environment:
  READOUT_ID                 Dated readout id, ticket id, or artifact slug.
  ENVIRONMENT                Cluster/environment name or staging namespace.
  COHORT_RULE                Short cohort selection rule.
  ROLLOUT_WINDOW_UTC         Start/end UTC window.
  CONFIG_APPLIED             Safe rollout config summary.
  KILL_SWITCH_REF            Runbook link, command reference, or redacted path.
  KILL_SWITCH_TESTED         Short result of kill-switch test/safety proof.
  OPERATOR                   Operator name/handle/date metadata.
  TELEMETRY_COUNTERS_REF     Redacted summary path/link for counter outputs.
  NOTE_DISTRIBUTION_REF      Redacted summary path/link for note metadata read.
  EFFECTIVENESS_REF          Redacted summary path/link for outcome comparison.
  EMPTY_ERROR_DISABLED_REF   Redacted summary path/link for empty/error read.
  NOISE_SAMPLE_REF           Redacted summary path/link for noise sampling.
  PROMPT_BUDGET_REF          Redacted summary path/link for prompt-budget proof.

Allowed input content:
  Safe counts, rates, ids, rollout metadata, note ids/permalinks/types/ranks,
  confidence buckets, bounded path summaries, and already-redacted summary
  paths/links. Do not pass raw prompt logs, patch/source contents, source file
  paths as evidence payloads, or full rendered <relevant-pitfalls> hint bodies.

Options:
  -h, --help     Show this help.
  --self-test    Verify that the emitted bundle contains the required sections.
EOF
}

now_utc() {
    date -u +%Y-%m-%dT%H:%M:%SZ
}

redact_token_like() {
    # Preserve shape while masking common bearer/API token or signed URL values.
    sed -E \
        -e 's/(Authorization:[[:space:]]*Bearer[[:space:]]+)[^[:space:]]+/\1<redacted>/Ig' \
        -e 's/(bearer[ _-]?token=)[^&[:space:]]+/\1<redacted>/Ig' \
        -e 's/((api|access|auth|id|refresh|session)[_-]?token=)[^&[:space:]]+/\1<redacted>/Ig' \
        -e 's/((token|signature|X-Amz-Signature)=)[^&[:space:]]+/\1<redacted>/Ig' \
        -e 's/(sk-[A-Za-z0-9_-]{12})[A-Za-z0-9_-]+/\1<redacted>/g'
}

sanitize_scalar() {
    name=$1
    value=$2
    if [ -z "$value" ]; then
        printf 'TBD'
        return 0
    fi

    case "$value" in
        *'```'*|*'<relevant-pitfalls>'*|*'</relevant-pitfalls>'*)
            printf 'ERROR: %s appears to contain raw fenced content or rendered JIT hint body. Pass only a redacted summary reference.\n' "$name" >&2
            exit 2
            ;;
    esac

    if [ "$(printf '%s' "$value" | wc -l | tr -d ' ')" -gt 0 ] || printf '%s' "$value" | grep -q "$(printf '\r')"; then
        printf 'ERROR: %s must be a single safe scalar/reference, not raw multi-line content.\n' "$name" >&2
        exit 2
    fi

    # Refuse obvious raw prompt-log, patch, source, or rendered hint payload paths.
    # Keep this narrow enough to allow safe references such as prompt-budget summaries.
    lower=$(printf '%s' "$value" | tr '[:upper:]' '[:lower:]')
    case "$lower" in
        *raw-prompt*|*raw_prompt*|*prompt-log*|*prompt_log*|*prompts.log*|*patch*|*source*|*rendered-hint*|*rendered_hint*|*hint-body*|*hint_body*|*relevant-pitfalls*)
            printf 'ERROR: %s looks like a raw prompt/patch/source/hint-body reference. Use an already-redacted safe summary instead.\n' "$name" >&2
            exit 2
            ;;
        *.patch|*.diff|*.rs|*.py|*.js|*.jsx|*.ts|*.tsx|*.go|*.java|*.kt|*.c|*.cc|*.cpp|*.h|*.hpp|*.swift|*.rb|*.php)
            printf 'ERROR: %s points at a source/patch-like file. Pass only redacted summary artifacts.\n' "$name" >&2
            exit 2
            ;;
    esac

    if [ "$(printf '%s' "$value" | wc -c | tr -d ' ')" -gt 500 ]; then
        printf 'ERROR: %s is too long for a safe scalar/reference. Use a redacted summary path/link.\n' "$name" >&2
        exit 2
    fi

    printf '%s' "$value" | redact_token_like
}

validate_inputs() {
    # Run validation before the large here-doc is emitted. Command substitutions
    # inside a here-doc execute in subshells, so fail-closed checks must happen
    # here to avoid producing a partial bundle after rejecting an unsafe value.
    sanitize_scalar READOUT_ID "$READOUT_ID" >/dev/null
    sanitize_scalar ENVIRONMENT "$ENVIRONMENT" >/dev/null
    sanitize_scalar COHORT_RULE "$COHORT_RULE" >/dev/null
    sanitize_scalar ROLLOUT_WINDOW_UTC "$ROLLOUT_WINDOW_UTC" >/dev/null
    sanitize_scalar CONFIG_APPLIED "$CONFIG_APPLIED" >/dev/null
    sanitize_scalar KILL_SWITCH_REF "$KILL_SWITCH_REF" >/dev/null
    sanitize_scalar KILL_SWITCH_TESTED "$KILL_SWITCH_TESTED" >/dev/null
    sanitize_scalar OPERATOR "$OPERATOR" >/dev/null
    sanitize_scalar TELEMETRY_COUNTERS_REF "$TELEMETRY_COUNTERS_REF" >/dev/null
    sanitize_scalar EFFECTIVENESS_REF "$EFFECTIVENESS_REF" >/dev/null
    sanitize_scalar NOISE_SAMPLE_REF "$NOISE_SAMPLE_REF" >/dev/null
    sanitize_scalar PROMPT_BUDGET_REF "$PROMPT_BUDGET_REF" >/dev/null
    sanitize_scalar NOTE_DISTRIBUTION_REF "$NOTE_DISTRIBUTION_REF" >/dev/null
    sanitize_scalar EMPTY_ERROR_DISABLED_REF "$EMPTY_ERROR_DISABLED_REF" >/dev/null
}

emit_bundle() {
    generated_at=$(now_utc)
    cat <<EOF
# JIT pitfall cohort effectiveness readout bundle

- Generated at UTC: $generated_at
- Generator: \`scripts/jit-pitfall-readout-bundle.sh\`
- Mode: local template / DRY_RUN=${DRY_RUN}
- Readout id: $(sanitize_scalar READOUT_ID "$READOUT_ID")

> Paste or adapt this bundle into a dated copy of
> \`docs/JIT_PITFALL_EFFECTIVENESS_READ.md\`. This helper does not require
> production access and does not collect telemetry itself; operators fill the
> placeholders from safe, already-redacted telemetry outputs.
>
> **Safety rule:** do not paste raw prompt text, patch/source contents, source
> file contents, full rendered hint bodies, or the transient
> \`<relevant-pitfalls>...</relevant-pitfalls>\` block. Allowed evidence is
> limited to counts, rates, operational ids, rollout metadata, bounded path
> summaries, safe note metadata, short classifications, and already-redacted
> summary links/paths.

## Rollout record

| Field | Value |
| --- | --- |
| Environment / cluster | $(sanitize_scalar ENVIRONMENT "$ENVIRONMENT") |
| Cohort selection rule | $(sanitize_scalar COHORT_RULE "$COHORT_RULE") |
| Rollout start / end UTC | $(sanitize_scalar ROLLOUT_WINDOW_UTC "$ROLLOUT_WINDOW_UTC") |
| Config applied | $(sanitize_scalar CONFIG_APPLIED "$CONFIG_APPLIED") |
| Kill-switch command/runbook link or exact revert | $(sanitize_scalar KILL_SWITCH_REF "$KILL_SWITCH_REF") |
| Operator who can execute kill switch | $(sanitize_scalar OPERATOR "$OPERATOR") |
| Evidence that kill switch was tested or is already known safe | $(sanitize_scalar KILL_SWITCH_TESTED "$KILL_SWITCH_TESTED") |

## Telemetry counter outcomes

Redacted counter source/reference: $(sanitize_scalar TELEMETRY_COUNTERS_REF "$TELEMETRY_COUNTERS_REF")

| Metric | Count | Rate / denominator | Notes |
| --- | ---: | ---: | --- |
| Eligible search (\`eligible_search\`) |  | eligible / first modifications |  |
| Injected (\`injected\`) |  | injected / eligible |  |
| Empty/miss (\`empty\`) |  | empty / eligible |  |
| Error (\`error\`) |  | error / eligible |  |
| Disabled default-off (\`disabled_default_off\`) |  | disabled / modifications |  |
| Disabled kill-switch (\`disabled_kill_switch\`) |  | kill-switch / modifications |  |
| Non-first skipped (\`non_first_modification\`) |  | non-first / modifications |  |

## Note distribution metadata

Redacted note metadata source/reference: $(sanitize_scalar NOTE_DISTRIBUTION_REF "$NOTE_DISTRIBUTION_REF")

| Dimension | Summary |
| --- | --- |
| Note types (\`pitfall\`, \`pattern\`) |  |
| Rank distribution (1/2) |  |
| Confidence buckets (for example 0.30-0.49, 0.50-0.74, 0.75+) |  |
| Top safe note permalinks/ids by injection count |  |
| Projects/path-summary buckets with high empty rate |  |

## Injected-vs-control outcome comparison

Redacted effectiveness source/reference: $(sanitize_scalar EFFECTIVENESS_REF "$EFFECTIVENESS_REF")

| Cohort | Tasks/sessions | Reopen rate | Avg total reopens | Rework/continuation rate | Verification-failure rate | Avg total verification failures | Notes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| Injected |  |  |  |  |  |  |  |
| Eligible but empty |  |  |  |  |  |  |  |
| Default-off/control window |  |  |  |  |  |  |  |

Checklist:

- [ ] Injected traffic is comparable to control traffic by project/task type/priority mix.
- [ ] Reopen/rework/verification-failure rates for injected traffic are lower than or not worse than comparable non-injected traffic.
- [ ] Results are not explained solely by low sample size or one unusually easy project.
- [ ] Empty/error/disabled rates are understood and do not indicate the feature is usually unavailable.
- [ ] No production incident, latency spike, or operator complaint is attributed to the rollout.

## Empty/error/disabled read

Redacted source/reference: $(sanitize_scalar EMPTY_ERROR_DISABLED_REF "$EMPTY_ERROR_DISABLED_REF")

| Check | Result | Follow-up needed? |
| --- | --- | --- |
| Empty rate acceptable or explained by missing scoped notes? |  |  |
| Error rate near zero? If not, top error classes from safe \`error\` field? |  |  |
| Kill-switch outcome absent except deliberate tests/incidents? |  |  |
| Default-off outcomes only from non-cohort/control traffic? |  |  |
| Non-first skipped count consistent with once-per-session design? |  |  |

## False-positive / noise sampling

Redacted sample source/reference: $(sanitize_scalar NOISE_SAMPLE_REF "$NOISE_SAMPLE_REF")

Sampling plan:

- Sample size:
- Selection method (random, stratified by project/path summary/note type/confidence bucket):
- Reviewer/operator:
- Date:

Per-sample allowed fields only:

| Sample id | Task/session id | Project | Path summary | Note safe metadata (id/permalink/type/rank/confidence) | Operator classification | Action |
| --- | --- | --- | --- | --- | --- | --- |
|  |  |  |  |  | useful / neutral / noisy / false-positive | keep / edit note metadata / archive note / tune scope |

Noise summary:

| Classification | Count | Rate | Notes / follow-up |
| --- | ---: | ---: | --- |
| Useful |  |  |  |
| Neutral |  |  |  |
| Noisy |  |  |  |
| False-positive |  |  |  |

## Prompt-budget evidence

Redacted prompt-budget source/reference: $(sanitize_scalar PROMPT_BUDGET_REF "$PROMPT_BUDGET_REF")

| Evidence item | Result |
| --- | --- |
| Session-start prompt/token sample before rollout |  |
| Session-start prompt/token sample during rollout |  |
| Difference (unchanged/reduced required) |  |
| Tool response sample confirms transient \`jit_pitfalls\` only |  |
| Telemetry/log sample confirms no hint body text persisted |  |

Confirm:

- [ ] Session-start note injection remains the existing \`knowledge_context\` / \`Relevant Knowledge\` prompt section and is unchanged or reduced during the rollout.
- [ ] JIT hints appear only on the first modification tool result as the transient JSON field \`jit_pitfalls\`.
- [ ] The rendered hint is not stored in telemetry, durable memory, task comments, activity logs, or this readout.
- [ ] Later write/edit/apply_patch responses in the same session do not append another hint (\`non_first_modification\` outcome accounts for skips).
- [ ] No code path has moved the JIT block into system/developer prompts or session-start context.

## Positive-read recommendation / default-on gate

**Gate status:** \`UNKNOWN\`

A planner may create the default-on flip task only when all of these are true:

- **Gate status** is exactly \`PASS\`.
- Every required evidence row below is completed with a \`PASS\`/\`DONE\` status and a link or short summary.
- \`Recommendation:\` is exactly \`create default-on flip task\`.

\`UNKNOWN\` or \`FAIL\` must not produce a default-on flip task; leave the feature default-off and create follow-up work only if the operator recommendation asks for it.

| Required evidence | Status | Link / short summary |
| --- | --- | --- |
| Controlled rollout/cohort was enabled with documented kill switch |  |  |
| Telemetry counts collected for eligible, injected, empty, error, disabled, and non-first skipped outcomes |  |  |
| Injected vs non-injected outcome comparison completed using reopen/rework/verification-failure measures |  |  |
| Empty/error/disabled rates acceptable or follow-up tasks created |  |  |
| False-positive/noise sampling completed without storing prompt/patch/hint body text |  |  |
| Prompt-budget check confirms session-start note injection unchanged or reduced and JIT hints remain transient response fields |  |  |
| Operator recommendation recorded |  |  |

Decision:

- Recommendation: \`extend cohort\`
- Rationale:
- Required follow-up before default-on:
- Operator/date: $(sanitize_scalar OPERATOR "$OPERATOR")
EOF
}

self_test() {
    output=$(DRY_RUN=1 READOUT_ID=self-test "$0")
    for needle in \
        '## Rollout record' \
        '## Telemetry counter outcomes' \
        '## Injected-vs-control outcome comparison' \
        '## False-positive / noise sampling' \
        '## Prompt-budget evidence' \
        '## Positive-read recommendation / default-on gate' \
        'do not paste raw prompt text'; do
        case "$output" in
            *"$needle"*) ;;
            *)
                printf 'self-test failed: missing %s\n' "$needle" >&2
                exit 1
                ;;
        esac
    done
    printf 'jit-pitfall-readout-bundle self-test passed\n'
}

case "${1:-}" in
    -h|--help)
        usage
        exit 0
        ;;
    --self-test)
        self_test
        exit 0
        ;;
    '') ;;
    *)
        printf 'ERROR: unknown argument: %s\n' "$1" >&2
        usage >&2
        exit 2
        ;;
esac

validate_inputs
emit_bundle
