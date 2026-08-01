#!/usr/bin/env bash
# Every fail-closed arming switch the server binary reads MUST have a chart
# surface, and the standalone SCIP-index Job's cadence knobs must render as
# values the server can parse.
#
# # Why this test exists
#
# PR #2697 split the semantic (SCIP) index out of the graph-warm Job so a warm
# would stop paying for it. `DJINN_K8S_SCIP_INDEX_ENABLED` was made the per-tick
# arming switch precisely so that arming would be "a config flip, not a
# redeploy" — and then no chart, no values file and no deploy script ever set
# it. The feature shipped, every test stayed green, and it created zero Jobs in
# production while `kueue-topology.yaml` went on rendering a `-scip` LocalQueue
# to admit Jobs nothing could create. Every graph-warm kept paying the full
# inline index: 3644s of a measured 5442s warm.
#
# The required env set is READ OUT OF THE RUST SOURCE
# (`djinn_k8s::config::FAIL_CLOSED_ARMING_SWITCHES`), never enumerated here. A
# new arming switch added to that list without a chart surface fails this test;
# deleting an existing switch's env block from deployment-server.yaml fails it
# too; renaming or deleting the list itself fails it, because a parse that finds
# no switches is treated as a broken contract rather than a satisfied one. A
# test with the names hard-coded here would have gone on passing through exactly
# the defect it exists to catch.
#
# Usage: bash deploy/helm/djinn/tests/scip-index-arming-render.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHART_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_ROOT="$(cd "$CHART_DIR/../../.." && pwd)"
CONFIG_RS="$REPO_ROOT/server/crates/djinn-k8s/src/config.rs"

require_tool() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "FAIL: required test tool '$1' is not installed" >&2
        exit 1
    }
}

require_tool helm
require_tool python3

[ -f "$CONFIG_RS" ] || {
    echo "FAIL: cannot read the arming-switch source of truth: $CONFIG_RS" >&2
    exit 1
}

TMPDIR_RENDER=$(mktemp -d)
trap 'rm -rf "$TMPDIR_RENDER"' EXIT

render() {
    local output=$1
    shift
    helm template scip-index-arming-test "$CHART_DIR" \
        --is-upgrade \
        --show-only templates/deployment-server.yaml "$@" >"$output"
}

# Assert that every switch named in FAIL_CLOSED_ARMING_SWITCHES is rendered onto
# the server Deployment with a non-empty value.
assert_every_arming_switch_is_rendered() {
    local manifest=$1
    python3 - "$CONFIG_RS" "$manifest" <<'PY'
import re
import sys

config_rs, manifest = sys.argv[1:]
source = open(config_rs, encoding="utf-8").read()

block = re.search(
    r"pub const FAIL_CLOSED_ARMING_SWITCHES\s*:\s*&\[ArmingSwitch\]\s*=\s*&\[(.*?)\n\];",
    source,
    re.S,
)
assert block, (
    "FAIL_CLOSED_ARMING_SWITCHES was not found in "
    f"{config_rs}. If it was renamed or moved, point this test at the new "
    "source of truth — an unparsed list must never read as an empty one."
)
switches = re.findall(r'env\s*:\s*"([^"]+)"', block.group(1))
assert switches, (
    "FAIL_CLOSED_ARMING_SWITCHES parsed to zero entries; the contract this "
    "test enforces would be vacuous"
)

rendered = open(manifest, encoding="utf-8").read().splitlines()


def env_value(name):
    for index, line in enumerate(rendered):
        if re.match(rf"^\s*- name: {re.escape(name)}$", line):
            match = re.match(r"^\s*value: (.+)$", rendered[index + 1])
            assert match, f"{name} is rendered with no literal value"
            return match.group(1).strip().strip('"')
    return None


missing = [name for name in switches if env_value(name) is None]
assert not missing, (
    "these fail-closed arming switches have NO surface on the server "
    f"Deployment: {missing}. An unset fail-closed switch is not a disabled "
    "feature, it is an unreachable one: there is no supported way to turn it "
    "on and the deployment looks healthy while the code path never runs."
)
for name in switches:
    value = env_value(name)
    assert value != "", f"{name} renders an empty value, which the server cannot parse"

print(f"checked {len(switches)} fail-closed arming switch(es): {', '.join(switches)}")
PY
}

assert_env_values() {
    local manifest=$1
    shift
    python3 - "$manifest" "$@" <<'PY'
import re
import sys

manifest, *pairs = sys.argv[1:]
lines = open(manifest, encoding="utf-8").read().splitlines()


def env_value(name):
    for index, line in enumerate(lines):
        if re.match(rf"^\s*- name: {re.escape(name)}$", line):
            match = re.match(r"^\s*value: (.+)$", lines[index + 1])
            assert match, f"{name} has no rendered value"
            value = match.group(1)
            assert value.startswith('"') and value.endswith('"'), (
                f"{name} must render as a quoted literal, got {value!r}"
            )
            return value.strip('"')
    raise AssertionError(f"{name} is not rendered onto the server Deployment")


for pair in pairs:
    name, expected = pair.split("=", 1)
    actual = env_value(name)
    assert actual == expected, f"{name}: expected {expected!r}, got {actual!r}"
    if expected in ("true", "false"):
        continue
    # Helm decodes YAML numbers as float64, so a bare `| quote` renders 10800
    # as "10800" but a larger default as scientific notation the server's
    # `parse::<u64>()` rejects — and a rejected value is silently replaced by
    # the built-in default, i.e. a knob that reads as wired but is not.
    assert re.fullmatch(r"[0-9]+", actual), (
        f"{name} must render as a decimal integer, got {actual!r}"
    )
PY
}

expect_rejected() {
    local name=$1
    shift
    echo "=== invalid $name ==="
    if render "$TMPDIR_RENDER/$name.yaml" "$@" 2>&1; then
        echo "FAIL: invalid scipIndex scenario '$name' rendered successfully" >&2
        exit 1
    fi
}

echo "=== every fail-closed arming switch has a chart surface ==="
render "$TMPDIR_RENDER/defaults.yaml"
assert_every_arming_switch_is_rendered "$TMPDIR_RENDER/defaults.yaml"

echo "=== shipped defaults render the SCIP Job disarmed, explicitly ==="
assert_env_values "$TMPDIR_RENDER/defaults.yaml" \
    DJINN_K8S_SCIP_INDEX_ENABLED=false \
    DJINN_K8S_SCIP_INDEX_INTERVAL_SECONDS=10800 \
    DJINN_K8S_SCIP_QUIESCENCE_SECONDS=3523 \
    DJINN_K8S_SCIP_CLAIM_WAIT_SECONDS=900

echo "=== an operator can arm it and retune every gate ==="
render "$TMPDIR_RENDER/armed.yaml" \
    --set scipIndex.enabled=true \
    --set scipIndex.intervalSeconds=3600 \
    --set scipIndex.quiescenceSeconds=0 \
    --set scipIndex.claimWaitSeconds=0
assert_env_values "$TMPDIR_RENDER/armed.yaml" \
    DJINN_K8S_SCIP_INDEX_ENABLED=true \
    DJINN_K8S_SCIP_INDEX_INTERVAL_SECONDS=3600 \
    DJINN_K8S_SCIP_QUIESCENCE_SECONDS=0 \
    DJINN_K8S_SCIP_CLAIM_WAIT_SECONDS=0

expect_rejected negative-interval --set scipIndex.intervalSeconds=-1
expect_rejected negative-quiescence --set scipIndex.quiescenceSeconds=-1
expect_rejected negative-claim-wait --set scipIndex.claimWaitSeconds=-1
expect_rejected noninteger-enabled --set-string scipIndex.enabled=yes
expect_rejected unknown-key --set scipIndex.enable=true

echo "=== All SCIP-index arming Helm render tests passed ==="
