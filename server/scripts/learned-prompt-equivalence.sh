#!/usr/bin/env bash
#
# learned-prompt-equivalence.sh — pre/post assembled-prompt byte-comparison
# evidence helper for the learned-prompt harvest
# (server/docs/learned-prompt-harvest.md §6).
#
# PURPOSE
#   Compare a pre-cutover assembled system-prompt artifact against a
#   post-cutover assembled system-prompt artifact for a given
#   (project_id, agent_id) / disposition, and emit structured
#   byte-comparison evidence that an operator pastes into §6.1 or §6.2 of
#   the harvest artifact.
#
#   This helper compares **captured files**. It does not connect to any
#   database, render prompts, or reconstruct assembly. The operator is
#   responsible for capturing the pre and post assembled prompts using the
#   production code path (see "Capture instructions" below and §6.1 of the
#   harvest artifact). A worker environment must NOT capture prompts from
#   production/staging to populate the artifact (§1.1 / §7).
#
# ASSEMBLY SEMANTICS (grounded in the runtime)
#
#   The pre-cutover assembled system prompt is produced by
#     apply_role_extensions(base, system_prompt_extensions, learned_prompt)
#   in server/crates/djinn-agent/src/prompts.rs:
#
#     assembled = base
#     if system_prompt_extensions is non-blank:
#         assembled += "\n\n" + system_prompt_extensions.trim()
#     if learned_prompt is non-blank:
#         assembled += "\n\n" + learned_prompt.trim()
#
#   Order:  base → system_prompt_extensions → learned_prompt.
#
#   The learned_prompt value itself is derived in
#   server/crates/djinn-db/src/repositories/agent.rs as:
#
#     string_agg(h.proposed_text, E'\n\n---\n\n' ORDER BY h.created_at ASC)
#
#   over rows in learned_prompt_history with action IN ('keep','confirmed').
#
#   When reconstructing preserved learned text for comparison, the active
#   amendments MUST be joined with the literal separator "\n\n---\n\n" in
#   created_at ASC order. Multiple amendments produce:
#
#     amendment_1 + "\n\n---\n\n" + amendment_2 + "\n\n---\n\n" + ...
#
#   This separator and ordering are the single source of truth for what the
#   runtime appends. Any reconstruction that deviates (different separator,
#   different order, missing amendments, extra whitespace) will produce a
#   false non-match and invalidate the evidence.
#
# DISPOSITION RULES (what byte identity means for each case)
#
#   fold into project/role system_prompt_extensions:
#     Byte identity IS required and expected. Moving the learned text into
#     system_prompt_extensions at the same trailing position preserves the
#     exact byte sequence the model saw pre-cutover. Use --mode byte-identity.
#     A non-match means the move is NOT equivalent and must be re-classified.
#
#   fold into base prompt:
#     Byte identity is NOT expected and NOT required. The text is
#     reworded/repositioned as a global prompt-engineering edit. Use
#     --mode semantic. The helper still prints the byte comparison for
#     informational purposes, but the verdict is "semantic-rationale-required"
#     rather than PASS/FAIL. A semantic rationale must be recorded in §6.2.
#
#   convert to memory note / discard:
#     The text is intentionally removed from the assembled prompt. Byte
#     identity is not applicable. Use --mode removed. The helper prints
#     the byte diff for informational purposes; the verdict is "removed".
#     The rationale is recorded in §6.3.
#
# CAPTURE INSTRUCTIONS (operator, against target environment)
#
#   Pre-cutover capture:
#     For the affected (project_id, agent_id), render the assembled system
#     prompt using the production code path that calls
#     apply_role_extensions(base, system_prompt_extensions, learned_prompt)
#     with learned_prompt set to the exact string_agg result for that agent.
#     Save the rendered prompt to a file, e.g.:
#       pre-<project_id>-<agent_id>.prompt
#
#   Post-cutover capture:
#     For the same (project_id, agent_id), render the assembled prompt with
#     learned_prompt set to None and the moved amendment text appended to
#     system_prompt_extensions in the same trailing position previously held
#     by learned_prompt (for byte-identity dispositions). Save to:
#       post-<project_id>-<agent_id>.prompt
#
#   Then run:
#     server/scripts/learned-prompt-equivalence.sh \
#       --pre  pre-<project_id>-<agent_id>.prompt \
#       --post post-<project_id>-<agent_id>.prompt \
#       --mode byte-identity \
#       --label "<project_id>/<agent_id>"
#
# USAGE
#   server/scripts/learned-prompt-equivalence.sh [OPTIONS]
#
# OPTIONS
#   --pre PATH       Pre-cutover assembled-prompt file (required for compare).
#   --post PATH      Post-cutover assembled-prompt file (required for compare).
#   --mode MODE      Comparison mode (default: byte-identity):
#                      byte-identity — PASS/FAIL on exact byte match
#                                     (for system_prompt_extensions moves).
#                      semantic      — byte diff is informational only;
#                                     verdict is semantic-rationale-required
#                                     (for base-prompt promotions).
#                      removed       — byte diff is informational only;
#                                     verdict is removed
#                                     (for memory/discard dispositions).
#   --label LABEL    Identifier for the row being compared, e.g.
#                    "<project_id>/<agent_id>" or a disposition-table row
#                    number. Printed in the evidence block.
#   --disposition D  Human-readable disposition string printed in evidence
#                    (e.g. "fold into project/role system_prompt_extensions").
#   --out PATH       Write the evidence block to PATH instead of stdout.
#   --selftest       Run the helper against bundled fixtures and report
#                    PASS/FAIL for each. Ignores --pre/--post.
#   -h, --help       Show this help.
#
# EVIDENCE OUTPUT
#   After a successful compare, the script prints a block suitable for §6.1:
#
#     --- learned-prompt prompt-equivalence evidence ---
#     label:             <label>
#     disposition:       <disposition>
#     mode:              <mode>
#     pre_path:          <path>
#     post_path:         <path>
#     pre_sha256:        <lowercase hex>
#     post_sha256:       <lowercase hex>
#     pre_bytes:         <integer>
#     post_bytes:        <integer>
#     pre_chars:         <integer>
#     post_chars:        <integer>
#     byte_identical:    yes|no
#     verdict:           PASS|FAIL|semantic-rationale-required|removed
#     first_diff_offset: <byte offset or n/a>
#     tool:              learned-prompt-equivalence.sh
#     --- end evidence ---
#
# EXIT CODES
#   0 success (evidence emitted; for byte-identity mode, PASS or FAIL both
#     exit 0 — the verdict is in the output, not the exit code, so operators
#     can capture evidence for non-matches without a non-zero exit).
#   2 usage error (missing --pre/--post, unknown mode, file not found).
#   3 internal tool failure (sha256sum/cmp/wc unavailable).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIXTURE_DIR="${SCRIPT_DIR}/fixtures/learned-prompt-equivalence"

PRE_PATH=""
POST_PATH=""
MODE="byte-identity"
LABEL=""
DISPOSITION=""
OUT_PATH=""
SELFTEST=0

usage() {
    sed -n '2,/^EVIDENCE OUTPUT/p' "${BASH_SOURCE[0]}" \
        | sed 's/^# \{0,1\}//' >&2
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --pre)
            PRE_PATH="${2:?--pre requires a value}"; shift 2 ;;
        --post)
            POST_PATH="${2:?--post requires a value}"; shift 2 ;;
        --mode)
            MODE="${2:?--mode requires a value}"; shift 2 ;;
        --label)
            LABEL="${2:?--label requires a value}"; shift 2 ;;
        --disposition)
            DISPOSITION="${2:?--disposition requires a value}"; shift 2 ;;
        --out)
            OUT_PATH="${2:?--out requires a value}"; shift 2 ;;
        --selftest)
            SELFTEST=1; shift ;;
        -h|--help)
            usage; exit 0 ;;
        *)
            echo "unknown option: $1" >&2; usage; exit 2 ;;
    esac
done

# --- precondition checks -------------------------------------------------------
need_cmd() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "missing required command: $1" >&2; exit 3
    }
}

need_cmd sha256sum
need_cmd cmp
need_cmd wc
need_cmd awk

# --- validate mode -------------------------------------------------------------
case "$MODE" in
    byte-identity|semantic|removed) ;;
    *)
        echo "invalid --mode '$MODE' (expected: byte-identity|semantic|removed)" >&2
        exit 2
        ;;
esac

# --- core comparison function --------------------------------------------------
# Emits the evidence block to stdout. Sets global COMPARE_VERDICT.
emit_evidence() {
    local pre="$1" post="$2" mode="$3" label="$4" disp="$5"

    [[ -f "$pre" ]]  || { echo "pre file not found: $pre" >&2; exit 2; }
    [[ -f "$post" ]] || { echo "post file not found: $post" >&2; exit 2; }

    local pre_sha post_sha pre_bytes post_bytes pre_chars post_chars
    local byte_identical verdict first_diff

    pre_sha="$(sha256sum "$pre"  | awk '{print $1}')"
    post_sha="$(sha256sum "$post" | awk '{print $1}')"
    pre_bytes="$(wc -c < "$pre"  | tr -d ' ')"
    post_bytes="$(wc -c < "$post" | tr -d ' ')"
    # Character count: count all characters using wc -m. This is locale-aware
    # (UTF-8 multi-byte) and reflects the number of Unicode code points the
    # model effectively sees. Byte count is the byte-exact comparison metric.
    pre_chars="$(LC_ALL=C.UTF-8 wc -m < "$pre"  | tr -d ' ')"
    post_chars="$(LC_ALL=C.UTF-8 wc -m < "$post" | tr -d ' ')"

    if cmp -s "$pre" "$post"; then
        byte_identical="yes"
        first_diff="n/a"
    else
        byte_identical="no"
        # cmp prints the first differing byte offset; capture it.
        local cmp_out
        cmp_out="$(cmp "$pre" "$post" 2>&1 || true)"
        first_diff="$(printf '%s\n' "$cmp_out" | head -1)"
    fi

    case "$mode" in
        byte-identity)
            if [[ "$byte_identical" == "yes" ]]; then
                verdict="PASS"
            else
                verdict="FAIL"
            fi
            ;;
        semantic)
            verdict="semantic-rationale-required"
            ;;
        removed)
            verdict="removed"
            ;;
    esac

    COMPARE_VERDICT="$verdict"

    cat <<EOF
--- learned-prompt prompt-equivalence evidence ---
label:             ${label:-<unspecified>}
disposition:       ${disp:-<unspecified>}
mode:              ${mode}
pre_path:          ${pre}
post_path:         ${post}
pre_sha256:        ${pre_sha}
post_sha256:       ${post_sha}
pre_bytes:         ${pre_bytes}
post_bytes:        ${post_bytes}
pre_chars:         ${pre_chars}
post_chars:        ${post_chars}
byte_identical:    ${byte_identical}
verdict:           ${verdict}
first_diff_offset: ${first_diff}
tool:              learned-prompt-equivalence.sh
--- end evidence ---
EOF
}

# --- selftest ------------------------------------------------------------------
run_selftest() {
    local failures=0

    echo "=== learned-prompt-equivalence selftest ==="
    echo

    # Test 1: byte-identity fixture — pre and post MUST match.
    local pre1="$FIXTURE_DIR/byte-identity/pre-assembled.prompt"
    local post1="$FIXTURE_DIR/byte-identity/post-assembled.prompt"
    if [[ ! -f "$pre1" || ! -f "$post1" ]]; then
        echo "FAIL [byte-identity]: fixture files missing ($pre1 / $post1)"
        failures=$((failures + 1))
    else
        local out1
        out1="$(emit_evidence "$pre1" "$post1" "byte-identity" \
            "fixture/byte-identity" \
            "fold into project/role system_prompt_extensions")"
        local v1
        v1="$(printf '%s\n' "$out1" | awk -F': ' '/^verdict:/{gsub(/^ +/,"",$2); print $2}')"
        if [[ "$v1" == "PASS" ]]; then
            echo "PASS [byte-identity]: pre/post assembled prompts are byte-identical"
        else
            echo "FAIL [byte-identity]: expected PASS, got '$v1'"
            printf '%s\n' "$out1" | sed 's/^/    /'
            failures=$((failures + 1))
        fi
    fi
    echo

    # Test 2: semantic-drift fixture — pre and post MUST differ (intentionally).
    local pre2="$FIXTURE_DIR/semantic-drift/pre-assembled.prompt"
    local post2="$FIXTURE_DIR/semantic-drift/post-assembled.prompt"
    if [[ ! -f "$pre2" || ! -f "$post2" ]]; then
        echo "FAIL [semantic-drift]: fixture files missing ($pre2 / $post2)"
        failures=$((failures + 1))
    else
        local out2
        out2="$(emit_evidence "$pre2" "$post2" "semantic" \
            "fixture/semantic-drift" \
            "fold into base prompt")"
        local v2 bi2
        v2="$(printf '%s\n' "$out2" | awk -F': ' '/^verdict:/{gsub(/^ +/,"",$2); print $2}')"
        bi2="$(printf '%s\n' "$out2" | awk -F': ' '/^byte_identical:/{gsub(/^ +/,"",$2); print $2}')"
        if [[ "$v2" == "semantic-rationale-required" && "$bi2" == "no" ]]; then
            echo "PASS [semantic-drift]: byte-identical=no, verdict=semantic-rationale-required"
        else
            echo "FAIL [semantic-drift]: expected byte_identical=no + verdict=semantic-rationale-required"
            printf '%s\n' "$out2" | sed 's/^/    /'
            failures=$((failures + 1))
        fi
    fi
    echo

    # Test 3: removed fixture — pre and post differ; verdict is "removed".
    local pre3="$FIXTURE_DIR/removed/pre-assembled.prompt"
    local post3="$FIXTURE_DIR/removed/post-assembled.prompt"
    if [[ ! -f "$pre3" || ! -f "$post3" ]]; then
        echo "FAIL [removed]: fixture files missing ($pre3 / $post3)"
        failures=$((failures + 1))
    else
        local out3
        out3="$(emit_evidence "$pre3" "$post3" "removed" \
            "fixture/removed" \
            "discard")"
        local v3
        v3="$(printf '%s\n' "$out3" | awk -F': ' '/^verdict:/{gsub(/^ +/,"",$2); print $2}')"
        if [[ "$v3" == "removed" ]]; then
            echo "PASS [removed]: verdict=removed"
        else
            echo "FAIL [removed]: expected verdict=removed, got '$v3'"
            printf '%s\n' "$out3" | sed 's/^/    /'
            failures=$((failures + 1))
        fi
    fi
    echo

    echo "=== selftest complete: $failures failure(s) ==="
    if [[ "$failures" -gt 0 ]]; then
        exit 1
    fi
    exit 0
}

# --- dispatch ------------------------------------------------------------------
if [[ "$SELFTEST" -eq 1 ]]; then
    run_selftest
fi

# Compare mode requires both files.
if [[ -z "$PRE_PATH" || -z "$POST_PATH" ]]; then
    echo "ERROR: --pre and --post are required for compare mode." >&2
    echo "       Use --selftest to validate the helper against bundled fixtures." >&2
    exit 2
fi

if [[ -n "$OUT_PATH" ]]; then
    emit_evidence "$PRE_PATH" "$POST_PATH" "$MODE" "$LABEL" "$DISPOSITION" > "$OUT_PATH"
    cat "$OUT_PATH"
else
    emit_evidence "$PRE_PATH" "$POST_PATH" "$MODE" "$LABEL" "$DISPOSITION"
fi
