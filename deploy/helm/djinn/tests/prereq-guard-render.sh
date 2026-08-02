#!/usr/bin/env bash
# Acceptance contract for templates/prereq-guard.yaml — the install-time refusal
# that stops a fully-armed stock release from landing on a cluster that cannot
# run it.
#
# # Why this test is shaped like this
#
# The guard's only cluster contact is one `lookup`, and `lookup` returns nothing
# under `helm template`. A test that only ran `helm template` would therefore
# exercise the guard's INERT branch and nothing else — it would stay green if
# every `fail` underneath were deleted. That is the exact class of vacuous test
# that let `cgroupLauncher.mode: required` + `cgroupWritable.taskRuns.enabled:
# false` ship as the default pairing and take production down on 2026-07-29.
#
# So this suite copies the chart and replaces the single probe line — the one
# marked `djinn-prereq-probe` — with literal cluster facts, then drives every
# branch of the decision that hangs off it. The marker's presence is asserted
# first: an unfindable probe is treated as a broken contract, never as a
# satisfied one, so deleting or renaming it fails here rather than silently
# turning every case below into a no-op.
#
# Usage: bash deploy/helm/djinn/tests/prereq-guard-render.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHART_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
GUARD="$CHART_DIR/templates/prereq-guard.yaml"

require_tool() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "FAIL: required test tool '$1' is not installed" >&2
        exit 1
    }
}

require_tool helm
require_tool python3
require_tool grep

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

[ -f "$GUARD" ] || {
    echo "FAIL: the prerequisite guard is missing: $GUARD" >&2
    exit 1
}

# ---------------------------------------------------------------------------
# 0. The substitution anchor must exist, exactly once.
# ---------------------------------------------------------------------------
echo "=== the guard exposes exactly one cluster probe ==="
probe_lines=$(grep -c 'djinn-prereq-probe' "$GUARD" || true)
# Two hits: the prose paragraph that explains the marker, and the marker itself.
# What must be unique is the LINE THAT CALLS lookup.
probe_calls=$(grep -c 'lookup "v1" "Node"' "$GUARD" || true)
if [ "$probe_calls" -ne 1 ]; then
    echo "FAIL: expected exactly one Node lookup in $GUARD, found $probe_calls." >&2
    echo "      Every fixture below substitutes that one line; more than one (or none)" >&2
    echo "      means this suite is no longer driving the guard's real decision." >&2
    exit 1
fi
if [ "$probe_lines" -lt 1 ]; then
    echo "FAIL: the djinn-prereq-probe marker is gone from $GUARD" >&2
    exit 1
fi

# Builds a chart copy whose probe line reports the given literal facts.
#   $1  destination directory name under $WORK
#   $2  Go-template list expression for the observed Nodes
#   $3  `true` / `false` — whether kueue.x-k8s.io/v1beta1 is served
fixture_chart() {
    local name=$1 nodes=$2 kueue_api=$3
    local dest="$WORK/$name"
    cp -R "$CHART_DIR" "$dest"
    python3 - "$dest/templates/prereq-guard.yaml" "$nodes" "$kueue_api" <<'PY'
import sys

path, nodes, kueue_api = sys.argv[1:]
lines = open(path, encoding="utf-8").read().splitlines(keepends=True)
targets = [i for i, line in enumerate(lines) if 'lookup "v1" "Node"' in line]
assert len(targets) == 1, f"expected one probe line, found {len(targets)}"
lines[targets[0]] = (
    '{{- $facts := dict "nodes" (%s) "kueueApi" %s -}}\n' % (nodes, kueue_api)
)
open(path, "w", encoding="utf-8").write("".join(lines))
PY
    printf '%s' "$dest"
}

NO_NODES='list'
UNLABELLED_NODE='list (dict "metadata" (dict "name" "node-a" "labels" (dict "kubernetes.io/os" "linux")))'
LABELLED_NODE='list (dict "metadata" (dict "name" "node-a" "labels" (dict "djinn.io/cgroup-writable" "true")))'
# A node whose label is present but NOT "true" must not count as prepared.
FALSE_LABELLED_NODE='list (dict "metadata" (dict "name" "node-a" "labels" (dict "djinn.io/cgroup-writable" "false")))'
# metadata.labels absent entirely — a real shape, and the one most likely to
# blow up a naive label read.
NO_LABELS_NODE='list (dict "metadata" (dict "name" "node-a"))'

expect_render() {
    local label=$1 chart=$2
    shift 2
    if ! helm template prereq-guard-test "$chart" --is-upgrade "$@" >"$WORK/$label.out" 2>&1; then
        echo "FAIL [$label]: the guard refused a cluster that satisfies the prerequisites:" >&2
        cat "$WORK/$label.out" >&2
        exit 1
    fi
    # Non-vacuity: a render that produced nothing would satisfy any "it rendered"
    # assertion trivially.
    grep -q '^kind: Deployment$' "$WORK/$label.out" || {
        echo "FAIL [$label]: the render produced no Deployment, so it proved nothing" >&2
        exit 1
    }
    echo "ok [$label]"
}

expect_refusal() {
    local label=$1 chart=$2
    shift 2
    local -a needles=()
    while [ "$#" -gt 0 ] && [ "$1" != "--" ]; do
        needles+=("$1")
        shift
    done
    [ "${1:-}" = "--" ] && shift
    if helm template prereq-guard-test "$chart" --is-upgrade "$@" >"$WORK/$label.out" 2>&1; then
        echo "FAIL [$label]: the guard accepted a cluster missing its prerequisites." >&2
        echo "      Nothing would have stopped this install from reporting success and" >&2
        echo "      then dispatching zero Jobs." >&2
        exit 1
    fi
    local needle
    for needle in "${needles[@]}"; do
        grep -qF -- "$needle" "$WORK/$label.out" || {
            echo "FAIL [$label]: the refusal never mentions '$needle', so it does not tell" >&2
            echo "      an operator what is missing or how to install it:" >&2
            cat "$WORK/$label.out" >&2
            exit 1
        }
    done
    echo "ok [$label]"
}

# ---------------------------------------------------------------------------
# 1. No visible cluster: the guard stands down.
#
# This is `helm template`, `--dry-run`, and an operator whose credentials cannot
# list Nodes. Failing OPEN there is deliberate — a false refusal on an invisible
# cluster would break Tilt and every other chart contract script in this
# directory.
# ---------------------------------------------------------------------------
echo "=== an invisible cluster leaves the guard inert ==="
expect_render invisible-cluster "$(fixture_chart invisible "$NO_NODES" false)"
# The real, unsubstituted chart takes the same path under `helm template`.
expect_render unsubstituted-chart "$CHART_DIR"

# ---------------------------------------------------------------------------
# 2. Nodes visible, none prepared for the writable cgroup rollout.
# ---------------------------------------------------------------------------
echo "=== unprepared nodes are refused, by name ==="
expect_refusal unlabelled-nodes "$(fixture_chart unlabelled "$UNLABELLED_NODE" true)" \
    'djinn.io/cgroup-writable=true' \
    'deploy/node/k3s/djinn-cgroup-writable-conformance.sh' \
    'RuntimeClass/djinn-cgroup-writable'

echo "=== a node labelled with something other than \"true\" does not count ==="
expect_refusal false-labelled-nodes "$(fixture_chart falselabel "$FALSE_LABELLED_NODE" true)" \
    'djinn.io/cgroup-writable=true'

echo "=== a node with no labels at all is handled, not crashed on ==="
expect_refusal unlabelled-metadata "$(fixture_chart nolabels "$NO_LABELS_NODE" true)" \
    'djinn.io/cgroup-writable=true'

# ---------------------------------------------------------------------------
# 3. Nodes prepared, Kueue absent.
# ---------------------------------------------------------------------------
echo "=== a cluster without the Kueue API is refused, naming the prerequisite ==="
expect_refusal kueue-absent "$(fixture_chart kueueabsent "$LABELLED_NODE" false)" \
    'kueue.x-k8s.io/v1beta1' \
    'deploy/helm/djinn-prereqs' \
    '--wait'

# ---------------------------------------------------------------------------
# 4. Both missing: the operator learns BOTH in one run.
#
# A guard that reported only the first defect would cost a second failed install
# to discover the second.
# ---------------------------------------------------------------------------
echo "=== both prerequisites missing are reported together ==="
expect_refusal both-missing "$(fixture_chart bothmissing "$UNLABELLED_NODE" false)" \
    'djinn.io/cgroup-writable=true' \
    'deploy/helm/djinn-prereqs'

# ---------------------------------------------------------------------------
# 5. A prepared cluster installs. This is the non-vacuity floor for every
#    refusal above: without it they could all be passing because the chart does
#    not render at all under a substituted probe.
# ---------------------------------------------------------------------------
echo "=== a prepared cluster renders ==="
expect_render prepared-cluster "$(fixture_chart prepared "$LABELLED_NODE" true)"

# ---------------------------------------------------------------------------
# 6. The documented escape hatch really escapes.
#
# The refusal message tells an operator to disable the armed profile. If those
# flags did not actually silence the guard, the message would be a lie and an
# unprepared cluster would have no supported install at all.
# ---------------------------------------------------------------------------
echo "=== the opt-out named in the refusal actually works ==="
expect_render opted-out "$(fixture_chart optedout "$UNLABELLED_NODE" false)" \
    --set cgroupLauncher.mode=disabled \
    --set cgroupWritable.runtimeClass.enabled=false \
    --set cgroupWritable.taskRuns.enabled=false \
    --set imagePipeline.controller.launcherAuthorityProtocol=leaf-v1 \
    --set kueue.enabled=false \
    --set kueue.armed=false

# The bootstrap path the refusal message prescribes MUST work, or a fresh
# cluster has no way in at all: node conformance needs
# RuntimeClass/djinn-cgroup-writable-probe, and this chart is the only thing
# that renders it. The guard therefore keys on `taskRuns.enabled` (the
# assignment) and NOT on `runtimeClass.enabled` (the class itself) — this case
# is what pins that distinction.
echo "=== the preparation profile installs on an unconformed cluster, and still renders the probe class ==="
PREPARATION_CHART="$(fixture_chart preparation "$UNLABELLED_NODE" true)"
expect_render preparation "$PREPARATION_CHART" \
    --set cgroupLauncher.mode=disabled \
    --set cgroupWritable.taskRuns.enabled=false \
    --set imagePipeline.controller.launcherAuthorityProtocol=leaf-v1
grep -q 'name: djinn-cgroup-writable-probe' "$WORK/preparation.out" || {
    echo "FAIL: the preparation profile did not render the probe RuntimeClass, so node" >&2
    echo "      conformance could never run and the bootstrap the refusal message" >&2
    echo "      prescribes would be a dead end." >&2
    exit 1
}
echo "ok [preparation renders the probe class]"

echo "=== the local-dev values profile installs on an unprepared cluster ==="
expect_render local-profile "$(fixture_chart localprofile "$UNLABELLED_NODE" false)" \
    --values "$CHART_DIR/values.local.yaml" \
    --set-string migration.designatedOperatorSecret=prereq-guard-test

# ---------------------------------------------------------------------------
# 7. Each half is independently gated: disabling one stack must not silence the
#    other. A single collapsed condition would pass every case above.
# ---------------------------------------------------------------------------
echo "=== disabling Kueue alone does not excuse unprepared nodes ==="
expect_refusal kueue-off-nodes-bad "$(fixture_chart kueueoff "$UNLABELLED_NODE" false)" \
    'djinn.io/cgroup-writable=true' -- \
    --set kueue.enabled=false --set kueue.armed=false

echo "=== disabling the cgroup stack alone does not excuse a missing Kueue ==="
expect_refusal cgroup-off-kueue-bad "$(fixture_chart cgroupoff "$UNLABELLED_NODE" false)" \
    'deploy/helm/djinn-prereqs' -- \
    --set cgroupLauncher.mode=disabled \
    --set cgroupWritable.runtimeClass.enabled=false \
    --set cgroupWritable.taskRuns.enabled=false \
    --set imagePipeline.controller.launcherAuthorityProtocol=leaf-v1

echo "=== All prerequisite-guard render tests passed ==="
