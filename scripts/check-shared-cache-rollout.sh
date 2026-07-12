#!/bin/sh
# Shared-cache cleanup rollout artifact validation guard.
#
# Validates that the checked-in operator runbook
# (docs/SHARED_CACHE_CLEANUP_ROLLOUT.md), the confirmation checklist
# (docs/SHARED_CACHE_CLEANUP_CONFIRMATION_CHECKLIST.md), and the referenced
# per-task-run directory guide (docs/CARGO_TARGET_RUN_DIR_VALIDATION.md) stay
# structurally consistent with the landed coordinator/telemetry stable names and
# the embedded Helm/PromQL/kubectl config examples.
#
# This guard is intentionally repository-local: it reads only checked-in docs
# and never contacts Kubernetes, Zot, Prometheus, or the production shared PVC.
# It has no credentials and no network dependency. A failure means a runbook or
# checklist section, link, stable component name, or embedded config example has
# drifted from landed code and must be reconciled.
#
# Usage:
#   ./scripts/check-shared-cache-rollout.sh
#
# Exit codes:
#   0  All structural assertions passed.
#   1  One or more assertions failed (drift detected).
#   2  Configuration error (missing required files).
#
# Run the self-test harness for fixture-based coverage:
#   sh scripts/test-check-shared-cache-rollout.sh

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
cd "$REPO_ROOT"

# Paths are overridable via environment so the self-test harness can point the
# guard at synthetic fixture files without touching the checked-in docs.
RUNBOOK="${SHARED_CACHE_ROLLOUT_RUNBOOK:-docs/SHARED_CACHE_CLEANUP_ROLLOUT.md}"
CHECKLIST="${SHARED_CACHE_ROLLOUT_CHECKLIST:-docs/SHARED_CACHE_CLEANUP_CONFIRMATION_CHECKLIST.md}"
RUN_DIR_GUIDE="${SHARED_CACHE_ROLLOUT_RUN_DIR_GUIDE:-docs/CARGO_TARGET_RUN_DIR_VALIDATION.md}"
ZOT_OBSERVATION="${SHARED_CACHE_ROLLOUT_ZOT_OBSERVATION:-server/docs/operational/zot-retention-gc-observation.md}"

# Stable names landed in the coordinator and telemetry crates. These are the
# source of truth; the docs must match them exactly.
RUNBOOK_REQUIRED_HEADINGS='
## Required rollout order
## Scope, invariants, and ownership
## Repository-defined controls and bounded evidence
## Stage 0 — Zot dry-run and selected-image preflight
## Stage 1 — prove build pods do not rely on sccache
## Stage 2 — operator-owned one-time `/cache/sccache` deletion
## Stage 3 — recurring sccache guard and run-root debris cleanup
## Stage 4 — warm-base idle eviction, then pressure eviction
## Fingerprint-last hold
## Completion checklist
'

# Stable coordinator cache-cleanup component labels (telemetry crate).
TELEMETRY_COMPONENTS='sccache cargo_target_runs cargo_warm_base'
# Stable coordinator cache-cleanup modes.
TELEMETRY_MODES='dry_run delete'
# Stable coordinator cache-cleanup outcomes (bounded set).
TELEMETRY_OUTCOMES='
deleted
skipped
retained
error
dry_run
uuid_orphan_deleted
malformed_dir_deleted
loose_file_deleted
retained_fresh_malformed
retained_non_utf8
retained_young
retained_active
retained_lock_busy
'

# Stable Zot retention preflight mode/outcome labels (image-controller crate).
ZOT_PREFLIGHT_MODES='disabled dry_run destructive'
ZOT_PREFLIGHT_OUTCOMES='disabled advisory destructive_safe destructive_blocked fetch_error'

# Stable coordinator env-var names (context.rs).
COORDINATOR_ENV_VARS='
DJINN_CACHE_CLEANUP_MODE
DJINN_CACHE_CLEANUP_SCCACHE_ENABLED
DJINN_CACHE_CLEANUP_SCCACHE_MAX_AGE_HOURS
DJINN_CACHE_CLEANUP_CARGO_DEBRIS_ENABLED
DJINN_CACHE_CLEANUP_CARGO_DEBRIS_MAX_AGE_DAYS
DJINN_CACHE_CLEANUP_WARM_BASE_IDLE_RETENTION_DAYS
DJINN_CACHE_CLEANUP_WARM_BASE_GRACE_PERIOD_SECS
DJINN_CACHE_CLEANUP_WARM_BASE_LOW_FREE_RATIO
DJINN_CACHE_CLEANUP_WARM_BASE_HIGH_FREE_RATIO
'

# Stable Prometheus metric names (telemetry crate).
TELEMETRY_METRICS='
djinn_cache_cleanup_total
djinn_cache_cleanup_candidates_total
djinn_cache_cleanup_reclaimed_bytes_total
'

# The six cross-path observability matrix component identifiers, in rollout
# order. zot_retention is the externally executed registry action; the next four
# are coordinator-owned; warm_fingerprint is gated and last.
CHECKLIST_COMPONENTS='zot_retention sccache cargo_target_runs_debris warm_idle warm_pressure warm_fingerprint'

# Helm value paths that the runbook's Stage 0 example must reference.
ZOT_HELM_VALUES='
imagePipeline.zot.retention.enabled
imagePipeline.zot.retention.dryRun
imagePipeline.zot.retention.newestTags
imagePipeline.zot.retention.deleteUntagged
'

PASS=0
FAIL=0

pass() {
    PASS=$((PASS + 1))
    printf '  ok   %s\n' "$1"
}

fail() {
    FAIL=$((FAIL + 1))
    printf '  FAIL %s\n' "$1" >&2
    if [ -n "${2:-}" ]; then
        printf '       %s\n' "$2" >&2
    fi
}

require_file() {
    path=$1
    if [ ! -f "$path" ]; then
        printf 'FATAL: required file not found: %s\n' "$path" >&2
        exit 2
    fi
}

# Assert that a literal string appears in a file.
assert_contains() {
    label=$1
    file=$2
    needle=$3
    if grep -qF -- "$needle" "$file"; then
        pass "$label"
    else
        fail "$label" "expected '$needle' in $file"
    fi
}

# Assert that a literal string does NOT appear in a file.
assert_lacks() {
    label=$1
    file=$2
    needle=$3
    if grep -qF -- "$needle" "$file"; then
        fail "$label" "did not expect '$needle' in $file"
    else
        pass "$label"
    fi
}

# Assert a basic regex appears in a file.
assert_contains_regex() {
    label=$1
    file=$2
    pattern=$3
    if grep -Eq -- "$pattern" "$file"; then
        pass "$label"
    else
        fail "$label" "expected regex /$pattern/ in $file"
    fi
}

# Assert a regex matches across the whole file, tolerating line breaks and
# markdown blockquote markers between tokens. Used for prose that wraps in the
# source so a reflow cannot silently drop an invariant phrase.
assert_contains_regex_multiline() {
    label=$1
    file=$2
    pattern=$3
    # tr converts newlines and blockquote '>' markers to spaces, then -s squeezes
    # repeated spaces so a single-line regex can span wrapped prose inside a
    # markdown blockquote. The pattern uses literal single spaces between tokens.
    if tr '\n>' '  ' < "$file" | tr -s ' ' | grep -Eq -- "$pattern"; then
        pass "$label"
    else
        fail "$label" "expected regex /$pattern/ (spanning line breaks) in $file"
    fi
}

# Extract the set of anchor IDs that Markdown links can target in a file.
# Includes heading-derived GitHub-style slugs and explicit <a name="..."> / {#...}
# anchors. Prints one anchor per line.
extract_anchors() {
    file=$1
    awk '
    /^#{1,6} / {
        h = $0
        sub(/^#{1,6} +/, "", h)
        gsub(/`/, "", h)
        h = tolower(h)
        gsub(/[^a-z0-9 _\-]/, "", h)
        gsub(/ /, "-", h)
        gsub(/^-+|-+$/, "", h)
        if (h != "") print h
    }
    /<a[ \t]+[^>]*name="/ {
        line = $0
        while (match(line, /name="[^"]+"/)) {
            a = substr(line, RSTART + 6, RLENGTH - 7)
            if (a != "") print a
            line = substr(line, RSTART + RLENGTH)
        }
    }
    /\{#\^?[^}]+\}/ {
        line = $0
        while (match(line, /\{#\^?[^}]+\}/)) {
            a = substr(line, RSTART + 2, RLENGTH - 3)
            gsub(/\^/, "", a)
            if (a != "") print a
            line = substr(line, RSTART + RLENGTH)
        }
    }
    ' "$file"
}

# Check every Markdown link in $src. For links with a URL fragment, resolve the
# target file and assert the fragment exists as an anchor there. Links without
# a fragment are ignored here (file existence is asserted elsewhere). Emits pass
# or fail messages under the given label prefix.
check_file_links() {
    label_prefix=$1
    src=$2
    src_dir=$(CDPATH= cd -- "$(dirname -- "$src")" && pwd)

    _links_ok=1
    _link_tmp=$(mktemp)
    awk '
    {
        line = $0
        while (match(line, /\[[^]]+\]\([^)]+\)/)) {
            link = substr(line, RSTART, RLENGTH)
            p1 = index(link, "(")
            p2 = index(link, ")")
            url = substr(link, p1 + 1, p2 - p1 - 1)
            # Strip optional title in double quotes; CommonMark also allows
            # single-quoted titles, but the project docs do not use them and
            # trying to match both inside a POSIX shell single-quoted awk script
            # breaks quote escaping, so we only handle double-quoted titles here.
            if (match(url, /[ \t]+".*"$/)) url = substr(url, 1, RSTART - 1)
            if (url != "") print url
            line = substr(line, RSTART + RLENGTH)
        }
    }
    ' "$src" > "$_link_tmp"

    while IFS= read -r url; do
        case "$url" in
            *#*)
                fragment="${url##*#}"
                path_part="${url%#*}"
                ;;
            *)
                continue
                ;;
        esac
        [ -z "$fragment" ] && continue

        if [ -z "$path_part" ]; then
            target="$src"
        else
            case "$path_part" in
                /*)
                    target="$path_part"
                    ;;
                *)
                    target="$src_dir/$path_part"
                    ;;
            esac
        fi

        if [ ! -f "$target" ]; then
            fail "$label_prefix: link target not found: $url" \
                "resolved to $target"
            _links_ok=0
            continue
        fi

        if ! extract_anchors "$target" | grep -qxF -- "$fragment"; then
            fail "$label_prefix: link fragment not found: $url" \
                "fragment '$fragment' not present in $target"
            _links_ok=0
        fi
    done < "$_link_tmp"
    rm -f -- "$_link_tmp"

    if [ "$_links_ok" -eq 1 ]; then
        pass "$label_prefix: all markdown link fragments resolve"
    fi
}

printf '== shared-cache rollout artifact validation ==\n'
printf 'repository: %s\n' "$REPO_ROOT"

# ── Required files exist ──────────────────────────────────────────────
require_file "$RUNBOOK"
require_file "$CHECKLIST"
require_file "$RUN_DIR_GUIDE"
require_file "$ZOT_OBSERVATION"
pass "all required artifact files exist"

# ── Runbook required headings ─────────────────────────────────────────
# Each non-empty line in RUNBOOK_REQUIRED_HEADINGS must appear verbatim.
printf '\n-- runbook required headings --\n'
_prev_ifs="$IFS"
IFS='
'
for heading in $RUNBOOK_REQUIRED_HEADINGS; do
    [ -z "$heading" ] && continue
    if grep -qF -- "$heading" "$RUNBOOK"; then
        pass "runbook heading present: $heading"
    else
        fail "runbook heading present: $heading" "missing in $RUNBOOK"
    fi
done
IFS="$_prev_ifs"

# ── Cross-link anchor resolution ──────────────────────────────────────
# The docs link to one another with Markdown URLs that may include fragments.
# Broad substring checks are not enough: a link can point to a heading that has
# been renamed or removed. Verify every fragment link resolves to a real anchor
# in the target file.
printf '\n-- cross-link anchor resolution --\n'
check_file_links "runbook" "$RUNBOOK"
check_file_links "checklist" "$CHECKLIST"

# Keep a separate guard that the runbook still contains the literal stage
# headings the checklist expects to link to, so a heading rename cannot be
# hidden by simply removing the link.
printf '\n-- checklist-linked runbook headings --\n'
for stage_heading in \
    '## Required rollout order' \
    '## Stage 0 — Zot dry-run and selected-image preflight' \
    '## Stage 1 — prove build pods do not rely on sccache' \
    '## Stage 2 — operator-owned one-time `/cache/sccache` deletion' \
    '## Stage 3 — recurring sccache guard and run-root debris cleanup' \
    '## Stage 4 — warm-base idle eviction, then pressure eviction' \
    '## Fingerprint-last hold'
do
    assert_contains "checklist-linked runbook heading exists: $stage_heading" \
        "$RUNBOOK" "$stage_heading"
done

# ── Checklist component rows in rollout order ────────────────────────
printf '\n-- checklist component rows --\n'
# The checklist must contain each stable matrix component as a row heading,
# in rollout order, with warm_fingerprint last and gated.
_expected_order='zot_retention sccache cargo_target_runs_debris warm_idle warm_pressure warm_fingerprint'
# Verify each component appears as a ### heading.
for comp in $CHECKLIST_COMPONENTS; do
    assert_contains_regex "checklist row for $comp" \
        "$CHECKLIST" "^### [0-9]+\\. \`?${comp}\`?"
done

# Verify rollout order: the line number of each component heading must be
# strictly increasing in the expected order.
_order_ok=1
_prev_line=0
for comp in $_expected_order; do
    _line=$(grep -nE "^### [0-9]+\\. \`?${comp}\`?" "$CHECKLIST" | head -1 | cut -d: -f1)
    if [ -z "$_line" ]; then
        fail "checklist order: $comp heading not found"
        _order_ok=0
        break
    fi
    if [ "$_line" -le "$_prev_line" ]; then
        fail "checklist order: $comp (line $_line) is not after previous (line $_prev_line)"
        _order_ok=0
        break
    fi
    _prev_line=$_line
done
if [ "$_order_ok" -eq 1 ]; then
    pass "checklist component rows are in rollout order"
fi

# warm_fingerprint must be the LAST component heading in the checklist.
_last_comp_heading_line=$(grep -nE '^### [0-9]+\. ' "$CHECKLIST" | tail -1 | cut -d: -f1)
_last_fp_line=$(grep -nE '^### [0-9]+\. `?warm_fingerprint`?' "$CHECKLIST" | head -1 | cut -d: -f1)
if [ -n "$_last_comp_heading_line" ] && [ -n "$_last_fp_line" ] \
    && [ "$_last_fp_line" -eq "$_last_comp_heading_line" ]; then
    pass "warm_fingerprint is the last checklist component row"
else
    fail "warm_fingerprint is the last checklist component row" \
        "fp heading line=$_last_fp_line, last component heading line=$_last_comp_heading_line"
fi

# ── Fingerprint fail-safe: cannot imply enablement before w06b ───────
printf '\n-- fingerprint fail-safe --\n'
# The fingerprint row must reference the w06b evidence gate.
assert_contains "fingerprint row references w06b gate" \
    "$CHECKLIST" "w06b"
# The runbook's fingerprint hold must also reference w06b.
assert_contains "runbook fingerprint hold references w06b" \
    "$RUNBOOK" "w06b"
# The fingerprint row must explicitly state it is gated/last and not enablement.
assert_contains "fingerprint row is gated and last" \
    "$CHECKLIST" "Fail-safe and last"
# The fail-safe prose wraps across lines in the source; match it as one space-
# joined span so a reflow cannot silently drop the invariant.
assert_contains_regex_multiline \
    "fingerprint row is not enablement" \
    "$CHECKLIST" \
    "cannot be read as proof that destructive fingerprint cleanup already exists or is enabled"
# No fingerprint deletion command/knob may be documented in the runbook.
assert_lacks "runbook has no fingerprint delete command" \
    "$RUNBOOK" "rm -rf -- /cache/cargo-target"
# The fingerprint enable decision must be None / disabled.
assert_contains "fingerprint enable decision is None" \
    "$CHECKLIST" "**None.**"

# ── Telemetry stable component/mode/outcome names ────────────────────
printf '\n-- telemetry stable names --\n'
# The runbook must enumerate the three coordinator component labels.
for comp in $TELEMETRY_COMPONENTS; do
    assert_contains "runbook names telemetry component: $comp" \
        "$RUNBOOK" "$comp"
done
# The runbook must enumerate the two modes.
for mode in $TELEMETRY_MODES; do
    assert_contains "runbook names telemetry mode: $mode" \
        "$RUNBOOK" "$mode"
done
# The runbook must enumerate the bounded outcomes.
for outcome in $TELEMETRY_OUTCOMES; do
    assert_contains "runbook names telemetry outcome: $outcome" \
        "$RUNBOOK" "$outcome"
done
# The three Prometheus metric names must appear in the runbook's PromQL section.
for metric in $TELEMETRY_METRICS; do
    assert_contains "runbook references Prometheus metric: $metric" \
        "$RUNBOOK" "$metric"
done

# ── Coordinator env-var names ────────────────────────────────────────
printf '\n-- coordinator env-var names --\n'
for var in $COORDINATOR_ENV_VARS; do
    assert_contains "runbook references env var: $var" \
        "$RUNBOOK" "$var"
done

# ── Zot preflight mode/outcome names ─────────────────────────────────
printf '\n-- zot preflight stable names --\n'
for mode in $ZOT_PREFLIGHT_MODES; do
    assert_contains "zot observation doc names preflight mode: $mode" \
        "$ZOT_OBSERVATION" "$mode"
done
for outcome in $ZOT_PREFLIGHT_OUTCOMES; do
    assert_contains "zot observation doc names preflight outcome: $outcome" \
        "$ZOT_OBSERVATION" "$outcome"
done

# ── Embedded Helm config example (Stage 0) ───────────────────────────
printf '\n-- helm config example --\n'
for val in $ZOT_HELM_VALUES; do
    assert_contains "runbook helm example references: $val" \
        "$RUNBOOK" "$val"
done
# The runbook must show a helm template invocation.
assert_contains "runbook has helm template command" \
    "$RUNBOOK" "helm template"
# The runbook must reference the gcDelay/gcInterval Zot GC settings.
assert_contains "runbook references gcDelay" "$RUNBOOK" "gcDelay"
assert_contains "runbook references gcInterval" "$RUNBOOK" "gcInterval"

# ── Embedded kubectl config example ──────────────────────────────────
printf '\n-- kubectl config example --\n'
# The runbook's kubectl baseline uses "$NS"; grep for the literal command shape
# without invoking the shell's parameter expansion. Single-quote the needle so
# $NS is passed verbatim to grep -F.
assert_contains "runbook has kubectl set env baseline" \
    "$RUNBOOK" 'kubectl -n "$NS" set env'
assert_contains "runbook has rollout status" \
    "$RUNBOOK" "rollout status"

# ── One-time /cache/sccache deletion command shape ───────────────────
printf '\n-- one-time sccache deletion command --\n'
assert_contains "runbook records du -sh /cache/sccache" \
    "$RUNBOOK" "du -sh /cache/sccache"
assert_contains "runbook records rm -rf -- /cache/sccache" \
    "$RUNBOOK" "rm -rf -- /cache/sccache"
assert_contains "runbook records find inventory" \
    "$RUNBOOK" "find /cache/sccache"
# The checklist must have the explicit operator record fields for the deletion.
assert_contains "checklist has pre-delete operator field" \
    "$CHECKLIST" "Pre-delete confirmation — operator name"
assert_contains "checklist has pre-delete approval field" \
    "$CHECKLIST" "Pre-delete confirmation — approval reference"
assert_contains "checklist has maintenance window field" \
    "$CHECKLIST" "approved maintenance window"
assert_contains "checklist has completion field" \
    "$CHECKLIST" "post-delete rebuild observation"
# The checklist must separate operator record from repository-automated proof.
assert_contains "checklist separates operator record from automated proof" \
    "$CHECKLIST" "deliberately separated"
assert_contains "checklist names the validation guard" \
    "$CHECKLIST" "scripts/check-shared-cache-rollout.sh"

# ── Warm-base completion fields ──────────────────────────────────────
printf '\n-- warm-base completion fields --\n'
for field in \
    'projected_bytes' \
    'reclaimed_bytes' \
    'reached_high_watermark' \
    'remeasurement_failed' \
    'retained_outcomes'
do
    assert_contains "runbook names warm-base field: $field" \
        "$RUNBOOK" "$field"
done
# Idle dry-run/delete log lines.
assert_contains "runbook names idle would-delete log" \
    "$RUNBOOK" "warm-base idle GC would delete idle base"
assert_contains "runbook names idle deleted log" \
    "$RUNBOOK" "warm-base idle GC deleted idle base"
assert_contains "runbook names pressure completed log" \
    "$RUNBOOK" "warm-base pressure GC completed"

# ── Dry-run-first invariant ──────────────────────────────────────────
printf '\n-- dry-run-first invariant --\n'
assert_contains "runbook is dry-run-first" \
    "$RUNBOOK" "dry-run-first"
assert_contains "runbook documents universal stop condition" \
    "$RUNBOOK" "Universal stop condition"

# ── Summary ──────────────────────────────────────────────────────────
printf -- '------------------------------------------\n'
printf 'passed: %d   failed: %d\n' "$PASS" "$FAIL"

if [ "$FAIL" -ne 0 ]; then
    exit 1
fi
exit 0
