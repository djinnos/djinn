#!/usr/bin/env bash
# Contract for the two facts a rendering-only proof kept missing (task srdw).
#
#   1. Kueue's DRA integration is disabled, so djinn-prereqs installs below
#      Kubernetes 1.34. Left at the upstream default it CrashLoopBackOffs with
#      "could not setup ResourceSlice indexer".
#   2. mpod/vpod, mdeployment/vdeployment and mstatefulset/vstatefulset ARE
#      registered, with failurePolicy: Ignore. values.yaml used to claim the
#      framework list deleted them outright; it does not.
#
# Every negative case is a REAL helm render of this same pinned subchart with
# the property deliberately broken — never a hand-written fixture. A fixture
# can drift into agreeing with a checker that no longer means anything; a
# broken render cannot.
#
# What this canNOT prove, and why the cluster evidence lives in the task rather
# than here: a render cannot tell you the manager starts. `helm template` was
# green for the entire life of the 1.34-floor defect. The gate assertion below
# is a REGRESSION fence on a fact established on real clusters (fails at 1.29,
# installs and reaches Ready at 1.30.13 and 1.31.0), not a substitute for it.
#
# Needs `helm` and python3 with PyYAML. A missing tool FAILS here, never skips.
#
# Usage: bash deploy/helm/djinn-prereqs/tests/feature-gates.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHART="$(cd "$SCRIPT_DIR/.." && pwd)"
CHECKER="$SCRIPT_DIR/check-dra-gates.py"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    exit 1
}

require_tool() {
    command -v "$1" >/dev/null 2>&1 || fail "required tool '$1' is not installed"
}

require_tool helm
require_tool python3
python3 -c 'import yaml' 2>/dev/null || fail "python3 PyYAML is required by this suite"
[ -f "$CHECKER" ] || fail "checker is missing: $CHECKER"

render() {
    # $1 = output file, rest = extra helm args
    local out="$1"
    shift
    helm template djinn-prereqs "$CHART" "$@" >"$out" 2>"$out.err" \
        || fail "helm template failed ($*): $(cat "$out.err")"
}

echo "=== 1/5 default render satisfies the contract ==="
render "$WORK/default.yaml"
python3 "$CHECKER" "$WORK/default.yaml" || fail "the shipped values.yaml does not satisfy its own contract"

echo "=== 2/5 render carries the exact three-gate flag ==="
# Asserted on the flag text as well as through the checker: the parser could in
# principle accept a shape the manager rejects, and this defect was originally
# missed by trusting a render.
EXPECTED_FLAG='--feature-gates=KueueDRAIntegration=false,KueueDRAIntegrationExtendedResource=false,KueueDRAIntegrationPartitionableDevices=false'
grep -Fq -- "$EXPECTED_FLAG" "$WORK/default.yaml" \
    || fail "rendered manager args do not contain the expected flag: $EXPECTED_FLAG"

echo "=== 3/5 NEGATIVE: dropping the gates is caught ==="
# This is the upstream default. It renders perfectly and cannot start below
# Kubernetes 1.34.
render "$WORK/no-gates.yaml" --set-json 'kueue.controllerManager.featureGates=[]'
if grep -Fq -- '--feature-gates' "$WORK/no-gates.yaml"; then
    fail "the negative render still emits --feature-gates; the negative case proves nothing"
fi
if python3 "$CHECKER" "$WORK/no-gates.yaml" >/dev/null 2>&1; then
    fail "checker PASSED a render with no feature gates — it would not catch an upstream default"
fi

echo "=== 4/5 NEGATIVE: flipping KueueDRAIntegration back on is caught ==="
# --set-json with the whole list, not --set on an index: helm's indexed --set
# REPLACES the list element rather than merging into it, which silently renders
# `--feature-gates=%!s(<nil>)=true` and would make this negative case vacuous.
render "$WORK/dra-on.yaml" --set-json 'kueue.controllerManager.featureGates=[{"name":"KueueDRAIntegration","enabled":true},{"name":"KueueDRAIntegrationExtendedResource","enabled":false},{"name":"KueueDRAIntegrationPartitionableDevices","enabled":false}]'
grep -Fq -- '--feature-gates=KueueDRAIntegration=true' "$WORK/dra-on.yaml" \
    || fail "the negative render did not actually enable KueueDRAIntegration"
if python3 "$CHECKER" "$WORK/dra-on.yaml" >/dev/null 2>&1; then
    fail "checker PASSED a render with KueueDRAIntegration enabled"
fi

echo "=== 5/5 NEGATIVE: re-adding 'pod' flips mpod/vpod to Fail and is caught ==="
# A real render with the scoping broken: "pod" put back into
# integrations.frameworks. The webhooks do not appear or disappear — their
# failurePolicy flips from Ignore to Fail, which is exactly the behaviour the
# old values.yaml prose denied.
python3 - "$CHART/values.yaml" "$WORK/pod-readded.values.yaml" <<'PY'
import sys

src, dst = sys.argv[1], sys.argv[2]
lines = open(src, encoding="utf-8").read().splitlines(keepends=True)
out, injected = [], False
for line in lines:
    out.append(line)
    if not injected and line.strip() == '- "batch/job"':
        out.append(line.replace('"batch/job"', '"pod"'))
        injected = True
if not injected:
    raise SystemExit('FAIL: could not find the "batch/job" framework entry to inject after')
open(dst, "w", encoding="utf-8").writelines(out)
PY
render "$WORK/pod-readded.yaml" --values "$WORK/pod-readded.values.yaml"
if python3 "$CHECKER" "$WORK/pod-readded.yaml" >/dev/null 2>&1; then
    fail "checker PASSED a render with 'pod' re-added to integrations.frameworks"
fi
# Prove the negative render broke what we think it broke: mpod is still
# REGISTERED (not deleted) and is now fail-closed.
python3 - "$WORK/pod-readded.yaml" <<'PY'
import sys

import yaml

docs = [d for d in yaml.safe_load_all(open(sys.argv[1], encoding="utf-8")) if isinstance(d, dict)]
hooks = {
    h["name"].split(".")[0]: h.get("failurePolicy")
    for d in docs
    if d.get("kind") == "MutatingWebhookConfiguration"
    for h in d.get("webhooks") or []
}
if "mpod" not in hooks:
    raise SystemExit("FAIL: mpod vanished from the render; the negative case is not what it claims")
if hooks["mpod"] != "Fail":
    raise SystemExit(f"FAIL: expected mpod failurePolicy Fail with 'pod' enabled, got {hooks['mpod']!r}")
print("OK: re-adding 'pod' keeps mpod registered and flips it Ignore -> Fail")
PY

echo
echo "PASS: djinn-prereqs feature-gate and webhook-registration contract"
