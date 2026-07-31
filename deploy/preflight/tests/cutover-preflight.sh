#!/usr/bin/env bash
# Acceptance contract for deploy/preflight/cutover-preflight.sh.
#
# The question this suite exists to answer is not "does the script run" but
# "would it have caught the defect". So it is organised the same way
# tests/render-gate.sh is:
#
#   1. Does the gate reject each of the six defect classes, and accept the
#      STOCK chart under the mode it currently runs?
#   2. Does it read the RENDER, or is it paraphrasing its arguments? Settled by
#      handing it a render mutated by exactly ONE field while passing the
#      arguments of a healthy deployment.
#
# EVERY FIXTURE IS DERIVED FROM A LIVE RENDER
# -------------------------------------------
# The base is `helm template` of the shipped chart, produced here, now. Each
# negative fixture is that file with one line changed, and `assert_one_line`
# proves it — so a refusal can never be attributed to fixture drift. No
# checked-in YAML is the input to anything.
#
# THE DRAIN FENCE
# ---------------
# Without DJINN_DATABASE_URL the durable fence is UNOBSERVABLE, which is a
# defect by design: "the database was unreachable" and "nothing is in flight"
# must never resolve the same way. The suite therefore asserts the exact defect
# CLASS SET rather than only the exit status, so a run with no database still
# proves every render-derived class came back clean. Given a database (the
# `cutover-preflight` quality-gate lane provides one) the clean case is a
# genuine exit 0.
#
# Usage: bash deploy/preflight/tests/cutover-preflight.sh
#
# Overrides (used when deliberately mutating the tool to check this suite is
# not vacuous — see the PR description for the recorded runs):
#   CUTOVER_PREFLIGHT      path to the gate script under test
#   CUTOVER_PREFLIGHT_BIN  path to the binary the gate should use
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/../../.." && pwd)"
CHART_DIR="$REPO_DIR/deploy/helm/djinn"
GATE="${CUTOVER_PREFLIGHT:-$REPO_DIR/deploy/preflight/cutover-preflight.sh}"

require_tool() {
  command -v "$1" >/dev/null 2>&1 || {
    printf 'FAIL: required test tool %s is not installed\n' "$1" >&2
    exit 1
  }
}
require_tool helm
require_tool python3
require_tool bash
require_tool cargo

[ -x "$GATE" ] || [ -f "$GATE" ] || {
  printf 'FAIL: gate script not found: %s\n' "$GATE" >&2
  exit 1
}

if [ -z "${CUTOVER_PREFLIGHT_BIN:-}" ]; then
  # From `server/` — see the note in deploy/preflight/cutover-preflight.sh: a
  # relative CARGO_BUILD_BUILD_DIR resolves against the process CWD, and this
  # suite runs from the repository root.
  (cd "$REPO_DIR/server" && cargo build -p djinn-k8s --bin cutover-preflight)
  CUTOVER_PREFLIGHT_BIN="${CARGO_TARGET_DIR:-$REPO_DIR/server/target}/debug/cutover-preflight"
fi
export CUTOVER_PREFLIGHT_BIN
[ -x "$CUTOVER_PREFLIGHT_BIN" ] || {
  printf 'FAIL: cutover-preflight binary is not executable: %s\n' "$CUTOVER_PREFLIGHT_BIN" >&2
  exit 1
}

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
failures=0
case_index=0

# The classes the gate is EXPECTED to report, as an exact set. Asserting the set
# — rather than "output mentions X" — is what makes a no-database run still
# prove that every render-derived class came back clean.
expect_classes() {
  local label=$1 want_status=$2 want_classes=$3
  shift 3
  case_index=$((case_index + 1))
  local out status classes
  set +e
  out=$("$GATE" "$@" 2>&1)
  status=$?
  set -e
  classes=$(printf '%s\n' "$out" | sed -n 's/^cutover-preflight: DEFECT \([a-z-]*\) .*/\1/p' |
    sort -u | paste -sd, -)
  if [ "$status" -ne "$want_status" ]; then
    printf 'FAIL [%s]: expected exit %s, got %s\n%s\n' "$label" "$want_status" "$status" "$out" >&2
    failures=$((failures + 1))
    return
  fi
  if [ "$classes" != "$want_classes" ]; then
    printf 'FAIL [%s]: expected defect classes {%s}, got {%s}\n%s\n' \
      "$label" "$want_classes" "$classes" "$out" >&2
    failures=$((failures + 1))
    return
  fi
  printf 'ok [%s] classes={%s}\n' "$label" "$classes"
}

# The drain fence is a durable fact, so its verdict depends on whether this lane
# has a database. Everything else in this suite is render-derived and does not.
if [ -n "${DJINN_DATABASE_URL:-}" ]; then
  BASELINE_CLASSES=""
  BASELINE_STATUS=0
  printf 'note: DJINN_DATABASE_URL is set; the drain fence is read for real\n'
else
  BASELINE_CLASSES="drain-fence"
  BASELINE_STATUS=1
  printf 'note: DJINN_DATABASE_URL is unset; the drain fence is UNOBSERVABLE and therefore a\n'
  printf 'note: defect. Every case below asserts the exact class set, so the render-derived\n'
  printf 'note: classes are still proven clean.\n'
fi

# Add `drain-fence` to a class list, keeping it sorted and deduplicated the way
# the gate prints it.
with_baseline() {
  if [ -z "$BASELINE_CLASSES" ]; then
    printf '%s' "$1"
  elif [ -z "$1" ]; then
    printf '%s' "$BASELINE_CLASSES"
  else
    printf '%s\n%s\n' "$1" "$BASELINE_CLASSES" | tr ',' '\n' | sort -u | paste -sd, -
  fi
}

# ---------------------------------------------------------------------------
# 1. The stock chart, and an armed chart, under the mode each actually runs.
# ---------------------------------------------------------------------------
# The chart ships `cgroupLauncher.mode: disabled`, which is the leaf-v1 status
# quo: no sidecar, no broker, nothing to bound.
expect_classes 'stock chart defaults are a clean leaf-v1 deployment' \
  "$BASELINE_STATUS" "$(with_baseline '')" "$CHART_DIR"

ARMED_ARGS=(--set cgroupLauncher.mode=required --set cgroupWritable.taskRuns.enabled=true
  --set cgroupWritable.runtimeClass.enabled=true)

expect_classes 'an armed chart is a clean leaf-v1 deployment' \
  "$BASELINE_STATUS" "$(with_baseline '')" "$CHART_DIR" "${ARMED_ARGS[@]}"

DJINN_CUTOVER_AUTHORITY_MODE=resize-v2 \
  expect_classes 'an armed chart is ready for the resize-v2 cutover' \
  "$BASELINE_STATUS" "$(with_baseline '')" "$CHART_DIR" "${ARMED_ARGS[@]}"

# The defect an operator would actually hit first: flipping the authority mode
# while the launcher is still disabled. There is no sidecar for pod resize to
# move, so every brokered build would be bounded only by the node.
DJINN_CUTOVER_AUTHORITY_MODE=resize-v2 \
  expect_classes 'resize-v2 with the launcher disabled is refused' \
  1 "$(with_baseline 'launcher-cpu-ceiling')" "$CHART_DIR"

# ---------------------------------------------------------------------------
# 2. The gate reads the RENDER, not the flags.
# ---------------------------------------------------------------------------
helm template djinn-cutover-preflight "$CHART_DIR" --is-upgrade "${ARMED_ARGS[@]}" \
  >"$WORK/live.yaml"

# The `pods/resize` rule renders as three consecutive lines:
#
#     - apiGroups: [""]
#       resources: ["pods/resize"]
#       verbs: ["patch"]
#
# Every mutation below is anchored on the `resources: ["pods/resize"]` line and
# addressed by OFFSET from it, so `apiGroups: [""]` — which the chart renders a
# dozen times — is unambiguous. Deliberately line-scoped rather than a YAML
# round-trip: a re-serialised manifest differs from the real render in a hundred
# incidental ways, and the claim under test is that ONE line flips the verdict.
mutate() {
  python3 - "$WORK/live.yaml" "$1" "$2" "$3" <<'PY'
import sys
from pathlib import Path

source, destination, offset, replacement = sys.argv[1:5]
offset = int(offset)
lines = Path(source).read_text(encoding='utf-8').splitlines(keepends=True)
anchors = [i for i, line in enumerate(lines) if line.strip() == 'resources: ["pods/resize"]']
assert len(anchors) == 1, f'expected exactly one pods/resize rule, found {len(anchors)}'
index = anchors[0] + offset

if replacement == '<delete-rule>':
    # The whole three-line rule: `- apiGroups`, `resources`, `verbs`.
    assert lines[anchors[0] - 1].strip() == '- apiGroups: [""]'
    assert lines[anchors[0] + 1].strip() == 'verbs: ["patch"]'
    del lines[anchors[0] - 1:anchors[0] + 2]
else:
    indent = lines[index][:len(lines[index]) - len(lines[index].lstrip())]
    lines[index] = f'{indent}{replacement}\n'

Path(destination).write_text(''.join(lines), encoding='utf-8')
PY
}

# Prove each fixture differs from the live render by exactly one line.
assert_one_line() {
  local label=$1 fixture=$2 delta=$3
  python3 - "$WORK/live.yaml" "$fixture" "$delta" "$label" <<'PY'
import sys
from pathlib import Path

live, fixture, delta, label = sys.argv[1:5]
a = Path(live).read_text(encoding='utf-8').splitlines()
b = Path(fixture).read_text(encoding='utf-8').splitlines()
if delta == 'replaced':
    assert len(a) == len(b), f'{label}: the fixture changed the line count'
    differences = [(x, y) for x, y in zip(a, b) if x != y]
    assert len(differences) == 1, f'{label}: expected one differing line, got {differences}'
elif delta == 'rule-deleted':
    # The rule is three lines. Removing fewer, or touching anything outside
    # them, is fixture drift and must fail here rather than in a verdict.
    assert len(a) == len(b) + 3, f'{label}: expected exactly the three rule lines removed'
    from difflib import SequenceMatcher
    blocks = [op for op in SequenceMatcher(None, a, b).get_opcodes() if op[0] != 'equal']
    assert len(blocks) == 1, f'{label}: expected one contiguous edit, got {blocks}'
    tag, i1, i2, j1, j2 = blocks[0]
    assert tag == 'delete' and i2 - i1 == 3, f'{label}: unexpected edit {blocks[0]}'
    assert [line.strip() for line in a[i1:i2]] == [
        '- apiGroups: [""]', 'resources: ["pods/resize"]', 'verbs: ["patch"]'
    ], f'{label}: the wrong three lines were removed: {a[i1:i2]}'
elif delta == 'appended':
    assert b[:len(a)] == a, f'{label}: the fixture altered the live render instead of appending'
    assert len(b) > len(a), f'{label}: nothing was appended'
else:
    raise SystemExit(f'unknown delta {delta}')
PY
}

# --- AC3: the pods/resize triple, four independent mutations. Each element of
# the triple is mutated on its own, because each one alone makes the grant
# useless while leaving a rule a lenient reader would accept: `get` on the
# subresource authorises nothing (it takes PATCH only), `apps` names a resource
# that does not exist, and `pods` is not `pods/resize`.
mutate "$WORK/rbac-verb.yaml" 1 'verbs: ["get"]'
assert_one_line 'verb patch -> get' "$WORK/rbac-verb.yaml" replaced
mutate "$WORK/rbac-group.yaml" -1 '- apiGroups: ["apps"]'
assert_one_line 'apiGroup "" -> apps' "$WORK/rbac-group.yaml" replaced
mutate "$WORK/rbac-resource.yaml" 0 'resources: ["pods"]'
assert_one_line 'resource pods/resize -> pods' "$WORK/rbac-resource.yaml" replaced
mutate "$WORK/rbac-deleted.yaml" 0 '<delete-rule>'
assert_one_line 'pods/resize rule deleted' "$WORK/rbac-deleted.yaml" rule-deleted

for fixture in rbac-verb rbac-group rbac-resource rbac-deleted; do
  CUTOVER_PREFLIGHT_RENDER="$WORK/$fixture.yaml" \
    expect_classes "render mutation $fixture is refused" \
    1 "$(with_baseline 'pods-resize-rbac')" "$CHART_DIR" "${ARMED_ARGS[@]}"
done

# --- AC9: a RoleBinding naming the task-run ServiceAccount.
TASKRUN_SA=$(python3 - "$WORK/live.yaml" <<'PY'
import sys
from pathlib import Path

import yaml

for doc in yaml.safe_load_all(Path(sys.argv[1]).read_text(encoding='utf-8')):
    if not isinstance(doc, dict) or doc.get('kind') != 'ServiceAccount':
        continue
    labels = (doc.get('metadata') or {}).get('labels') or {}
    if labels.get('app.kubernetes.io/component') == 'taskrun':
        print(doc['metadata']['name'])
        break
else:
    raise SystemExit('the live render carries no taskrun ServiceAccount')
PY
)
cp "$WORK/live.yaml" "$WORK/taskrun-bound.yaml"
cat >>"$WORK/taskrun-bound.yaml" <<EOF
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: djinn-cutover-preflight-taskrun-escalation
  namespace: djinn
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: Role
  name: djinn-cutover-preflight-controller
subjects:
  - kind: ServiceAccount
    name: $TASKRUN_SA
    namespace: djinn
EOF
assert_one_line 'taskrun RoleBinding added' "$WORK/taskrun-bound.yaml" appended

CUTOVER_PREFLIGHT_RENDER="$WORK/taskrun-bound.yaml" \
  expect_classes 'a RoleBinding naming the task-run ServiceAccount is refused' \
  1 "$(with_baseline 'credential-boundary')" "$CHART_DIR" "${ARMED_ARGS[@]}"

# The gate reads the render, not the flags: the SAME healthy arguments that
# passed in section 1 now fail, and only because the render changed.
printf 'ok [the verdicts above came from the render; the arguments never changed]\n'
case_index=$((case_index + 1))

# --- AC7: a catalog observation the render cannot contain.
cat >"$WORK/observations.json" <<'EOF'
{
  "catalog": [
    {
      "pull_ref": "registry.example/legacy@sha256:3333333333333333333333333333333333333333333333333333333333333333",
      "digest": "sha256:3333333333333333333333333333333333333333333333333333333333333333"
    }
  ]
}
EOF
DJINN_CUTOVER_AUTHORITY_MODE=resize-v2 DJINN_CUTOVER_OBSERVATIONS="$WORK/observations.json" \
  expect_classes 'a pre-handshake catalog image blocks a resize-v2 cutover' \
  1 "$(with_baseline 'catalog-protocol')" "$CHART_DIR" "${ARMED_ARGS[@]}"

# --- AC6: a birth observation confirmed from the WRONG status array.
cat >"$WORK/misleading-birth.json" <<'EOF'
{
  "births": [
    {
      "target_cpu": "4000m",
      "pod": {
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": { "name": "djinn-task-run-misleading", "namespace": "djinn" },
        "status": {
          "initContainerStatuses": [],
          "containerStatuses": [
            {
              "name": "cgroup-launcher",
              "ready": true,
              "restartCount": 0,
              "image": "registry.example/cutover:preflight",
              "imageID": "",
              "resources": { "limits": { "cpu": "4000m" } }
            }
          ]
        }
      }
    }
  ]
}
EOF
DJINN_CUTOVER_AUTHORITY_MODE=resize-v2 DJINN_CUTOVER_OBSERVATIONS="$WORK/misleading-birth.json" \
  expect_classes 'a matching status.containerStatuses entry never confirms a birth' \
  1 "$(with_baseline 'birth-confirmation')" "$CHART_DIR" "${ARMED_ARGS[@]}"

# --- AC5: the same birth, confirmed from the RIGHT array, in canonical form.
# `4` is what the apiserver stores a `4000m` PATCH back as. A string comparison
# would never confirm it.
cat >"$WORK/canonical-birth.json" <<'EOF'
{
  "births": [
    {
      "target_cpu": "4000m",
      "pod": {
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": { "name": "djinn-task-run-canonical", "namespace": "djinn" },
        "status": {
          "initContainerStatuses": [
            {
              "name": "cgroup-launcher",
              "ready": true,
              "restartCount": 0,
              "image": "registry.example/cutover:preflight",
              "imageID": "",
              "resources": { "limits": { "cpu": "4" } }
            }
          ]
        }
      }
    }
  ]
}
EOF
DJINN_CUTOVER_AUTHORITY_MODE=resize-v2 DJINN_CUTOVER_OBSERVATIONS="$WORK/canonical-birth.json" \
  expect_classes 'a ceiling of 4 confirms a target of 4000m' \
  "$BASELINE_STATUS" "$(with_baseline '')" "$CHART_DIR" "${ARMED_ARGS[@]}"

# ---------------------------------------------------------------------------
# 3. A malformed cutover question is unevaluable (exit 2), never a verdict.
# ---------------------------------------------------------------------------
case_index=$((case_index + 1))
set +e
out=$(DJINN_CUTOVER_AUTHORITY_MODE=leaf-v3 "$GATE" "$CHART_DIR" 2>&1)
status=$?
set -e
if [ "$status" -ne 2 ]; then
  printf 'FAIL [a malformed authority mode is unevaluable]: expected exit 2, got %s\n%s\n' \
    "$status" "$out" >&2
  failures=$((failures + 1))
elif ! printf '%s' "$out" | grep -qF 'UNEVALUABLE'; then
  printf 'FAIL [a malformed authority mode is unevaluable]: wrong reason\n%s\n' "$out" >&2
  failures=$((failures + 1))
else
  printf 'ok [a malformed authority mode is unevaluable, not defaulted to leaf-v1]\n'
fi

# ---------------------------------------------------------------------------
if [ "$failures" -ne 0 ]; then
  printf 'FAIL: %s of %s cutover-preflight cases failed\n' "$failures" "$case_index" >&2
  exit 1
fi
printf 'PASS: cutover preflight contract (%s cases)\n' "$case_index"
