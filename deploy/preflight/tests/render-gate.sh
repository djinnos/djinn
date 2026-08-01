#!/usr/bin/env bash
# Acceptance contract for deploy/preflight/render-gate.sh.
#
# The gate exists because v0.7.25's dispatch validator rejected the chart's own
# default values, so this suite is organised around two questions the 2026-07-29
# outage proved nobody was asking:
#
#   1. Does the gate reject the pairing that actually caused the outage, accept
#      the pairings that work, and accept the STOCK chart defaults?
#   2. Does it read the RENDER, or is it just paraphrasing the `--set` flags it
#      was handed? A gate that reads flags would have passed the outage render,
#      because the fatal values were the chart's own defaults, not flags.
#
# Question 2 is settled by driving the gate through a stubbed `helm` that emits
# a fixture manifest regardless of its arguments: known-bad flags with a
# known-good render must PASS, and known-good flags with a render whose ONLY
# difference is one env value must FAIL.
#
# Usage: bash deploy/preflight/tests/render-gate.sh
#
# Overrides (for deliberately mutating the tool to check this suite is not
# vacuous — see the PR description for the recorded runs):
#   RENDER_GATE      path to the gate script under test
#   RENDER_GATE_BIN  path to the render-gate binary the gate should use
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/../../.." && pwd)"
CHART_DIR="$REPO_DIR/deploy/helm/djinn"
GATE="${RENDER_GATE:-$REPO_DIR/deploy/preflight/render-gate.sh}"
CHART_CONTRACT="$REPO_DIR/deploy/helm/djinn/tests/cgroup-writable-render.sh"

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

# Build once here so an individual case never pays for it and a compile error
# is reported as a compile error rather than as a gate verdict.
if [ -z "${RENDER_GATE_BIN:-}" ]; then
  cargo build --manifest-path "$REPO_DIR/server/Cargo.toml" -p djinn-k8s --bin render-gate
  RENDER_GATE_BIN="${CARGO_TARGET_DIR:-$REPO_DIR/server/target}/debug/render-gate"
fi
export RENDER_GATE_BIN
[ -x "$RENDER_GATE_BIN" ] || {
  printf 'FAIL: render-gate binary is not executable: %s\n' "$RENDER_GATE_BIN" >&2
  exit 1
}

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
failures=0
case_index=0

# Runs the gate and asserts exit status plus, optionally, a required substring
# of its combined output. Both halves matter: an exit code alone cannot tell a
# `MissingDelegatedRuntimeClass` rejection apart from a helm crash.
expect_gate() {
  local label=$1 want=$2 needle=$3
  shift 3
  case_index=$((case_index + 1))
  local out status
  set +e
  out=$("$GATE" "$@" 2>&1)
  status=$?
  set -e
  if [ "$want" = pass ] && [ "$status" -ne 0 ]; then
    printf 'FAIL [%s]: expected exit 0, got %s\n%s\n' "$label" "$status" "$out" >&2
    failures=$((failures + 1))
    return
  fi
  if [ "$want" = fail ] && [ "$status" -eq 0 ]; then
    printf 'FAIL [%s]: expected a non-zero exit, got 0\n%s\n' "$label" "$out" >&2
    failures=$((failures + 1))
    return
  fi
  if [ -n "$needle" ] && ! printf '%s' "$out" | grep -qF -- "$needle"; then
    printf 'FAIL [%s]: output does not mention %s\n%s\n' "$label" "$needle" "$out" >&2
    failures=$((failures + 1))
    return
  fi
  printf 'ok [%s]\n' "$label"
}

# ---------------------------------------------------------------------------
# 1. The launcher pairing, through the real chart.
# ---------------------------------------------------------------------------
expect_gate 'armed launcher without the task-run RuntimeClass is refused' \
  fail 'cgroupLauncher.mode=required requires cgroupWritable.taskRuns.enabled=true' \
  "$CHART_DIR" --set cgroupLauncher.mode=required --set cgroupWritable.taskRuns.enabled=false

expect_gate 'armed launcher with the task-run RuntimeClass dispatches' \
  pass 'DISPATCHABLE' \
  "$CHART_DIR" --set cgroupLauncher.mode=required \
  --set cgroupWritable.taskRuns.enabled=true --set cgroupWritable.runtimeClass.enabled=true

expect_gate 'disarmed launcher dispatches without the RuntimeClass' \
  pass 'DISPATCHABLE' \
  "$CHART_DIR" --set cgroupLauncher.mode=disabled --set cgroupWritable.taskRuns.enabled=false \
  --set imagePipeline.controller.launcherAuthorityProtocol=leaf-v1

# The regression that took production down: a stock `helm install` of the chart
# has to be able to dispatch, with no operator overrides at all.
expect_gate 'stock chart defaults dispatch' pass 'DISPATCHABLE' "$CHART_DIR"

# ---------------------------------------------------------------------------
# 2. The explicit local/development profile may disable the complete stack.
# ---------------------------------------------------------------------------
ARMED_CHART="$WORK/chart-armed-default"
cp -R "$CHART_DIR" "$ARMED_CHART"
python3 - "$ARMED_CHART/values.yaml" <<'PY'
import sys
from pathlib import Path

path = Path(sys.argv[1])
lines = path.read_text(encoding='utf-8').splitlines(keepends=True)
patched = 0
for index, line in enumerate(lines):
    if line.rstrip('\n') == 'cgroupLauncher:':
        for offset in range(index + 1, len(lines)):
            stripped = lines[offset].strip()
            if stripped.startswith('mode:'):
                assert stripped == 'mode: required', f'unexpected default launcher mode: {stripped}'
                lines[offset] = lines[offset].replace('mode: required', 'mode: disabled')
                patched += 1
                break
        break
for index, line in enumerate(lines):
    if line.strip() == 'launcherAuthorityProtocol: "resize-v2"':
        lines[index] = line.replace('"resize-v2"', '"leaf-v1"')
        patched += 1
        break
assert patched == 2, 'coherent launcher mode/protocol defaults were not found in values.yaml'
path.write_text(''.join(lines), encoding='utf-8')
PY

expect_gate 'values.yaml explicitly disabling the launcher remains renderable' \
  pass 'DISPATCHABLE' "$ARMED_CHART"

# The chart contract must pass against the unmodified armed defaults.
case_index=$((case_index + 1))
if DJINN_CHART_DIR="$CHART_DIR" bash "$CHART_CONTRACT" >"$WORK/contract-clean.log" 2>&1; then
  printf 'ok [chart contract passes against the shipped chart]\n'
else
  printf 'FAIL [chart contract passes against the shipped chart]\n%s\n' \
    "$(cat "$WORK/contract-clean.log")" >&2
  failures=$((failures + 1))
fi

# ---------------------------------------------------------------------------
# 3. The gate reads the RENDER, not the flags.
# ---------------------------------------------------------------------------
# One real, dispatchable render is the base for every fixture below, so each
# one differs from a genuinely working deployment by exactly the mutation named.
helm template render-gate-fixture "$CHART_DIR" --is-upgrade \
  --set cgroupLauncher.mode=required \
  --set cgroupWritable.runtimeClass.enabled=true \
  --set cgroupWritable.taskRuns.enabled=true >"$WORK/good.yaml"

# Line-scoped edits of one `- name:`/`value:` env pair. Deliberately not a YAML
# round-trip: a re-serialised manifest differs from the real render in a hundred
# incidental ways, and the claim being tested is that ONE value flips the
# verdict.
mutate() {
  python3 - "$1" "$2" "$3" "$4" "${5:-}" <<'PY'
import sys
from pathlib import Path

source, destination, operation, name, value = sys.argv[1:6]
lines = Path(source).read_text(encoding='utf-8').splitlines(keepends=True)
hits = [i for i, line in enumerate(lines) if line.strip() == f'- name: {name}']
assert len(hits) == 1, f'expected exactly one {name} env entry, found {len(hits)}'
index = hits[0]
assert lines[index + 1].strip().startswith('value:'), f'{name} is not a literal-value env entry'
indent = lines[index].split('-', 1)[0]

if operation == 'set-value':
    lines[index + 1] = f'{indent}  value: "{value}"\n'
elif operation == 'duplicate':
    lines[index + 2:index + 2] = [f'{indent}- name: {name}\n', f'{indent}  value: "{value}"\n']
elif operation == 'delete':
    del lines[index:index + 2]
else:
    raise SystemExit(f'unknown operation {operation}')

Path(destination).write_text(''.join(lines), encoding='utf-8')
PY
}

# A `helm` that ignores every argument it is given and prints a fixed manifest.
# Whatever verdict the gate reaches under this stub came from the render.
stub_helm() {
  local fixture=$1 path="$WORK/helm-stub-$2"
  cat >"$path" <<EOF
#!/usr/bin/env bash
cat '$fixture'
EOF
  chmod +x "$path"
  printf '%s' "$path"
}

ACTIVATION_ENV='DJINN_K8S_TASK_RUN_CGROUP_WRITABLE_ENABLED'
mutate "$WORK/good.yaml" "$WORK/bad.yaml" set-value "$ACTIVATION_ENV" false
python3 - "$WORK/good.yaml" "$WORK/bad.yaml" <<'PY'
import sys
from pathlib import Path

good, bad = (Path(path).read_text(encoding='utf-8').splitlines() for path in sys.argv[1:3])
differences = [(a, b) for a, b in zip(good, bad) if a != b]
assert len(good) == len(bad), 'the fixture changed more than one line'
assert len(differences) == 1, f'expected exactly one differing line, got {differences}'
assert 'value:' in differences[0][0], f'the differing line is not an env value: {differences[0]}'
PY

HELM="$(stub_helm "$WORK/good.yaml" good)" \
  expect_gate 'a dispatchable render passes despite undispatchable --set flags' \
  pass 'DISPATCHABLE' \
  "$CHART_DIR" --set cgroupLauncher.mode=required --set cgroupWritable.taskRuns.enabled=false

HELM="$(stub_helm "$WORK/bad.yaml" bad)" \
  expect_gate 'one flipped env value in the render fails despite dispatchable --set flags' \
  fail 'MissingDelegatedRuntimeClass' \
  "$CHART_DIR" --set cgroupLauncher.mode=required \
  --set cgroupWritable.taskRuns.enabled=true --set cgroupWritable.runtimeClass.enabled=true

# ---------------------------------------------------------------------------
# 4. Explicit env arrays are strategic-merge keys. Duplicate names must be
#    rejected before extraction; a missing required name remains an error.
# ---------------------------------------------------------------------------
mutate "$WORK/good.yaml" "$WORK/dup-last-false.yaml" duplicate "$ACTIVATION_ENV" false
mutate "$WORK/bad.yaml" "$WORK/dup-last-true.yaml" duplicate "$ACTIVATION_ENV" true
mutate "$WORK/good.yaml" "$WORK/no-mode.yaml" delete DJINN_K8S_CGROUP_LAUNCHER_MODE

HELM="$(stub_helm "$WORK/dup-last-false.yaml" duplast-false)" \
  expect_gate 'the real djinn-server collision is rejected before deploy' \
  fail 'Deployment/render-gate-fixture-djinn-server containers/djinn-server' "$CHART_DIR"

HELM="$(stub_helm "$WORK/dup-last-true.yaml" duplast-true)" \
  expect_gate 'duplicate diagnostics name every repeated variable' \
  fail "$ACTIVATION_ENV" "$CHART_DIR"

HELM="$(stub_helm "$WORK/no-mode.yaml" no-mode)" \
  expect_gate 'a render that omits the launcher mode is an error, not a default' \
  fail 'does not set DJINN_K8S_CGROUP_LAUNCHER_MODE' "$CHART_DIR"

python3 - "$WORK/good.yaml" "$WORK/breadth-base.yaml" <<'PY'
import sys
from pathlib import Path

source, destination = map(Path, sys.argv[1:3])
extra = '''
---
apiVersion: v1
kind: Pod
metadata: {name: direct-pod}
spec:
  containers:
    - name: worker
      image: example.invalid/test
      env: [{name: SHARED_OK, value: a}]
  initContainers:
    - name: setup
      image: example.invalid/test
      env: [{name: INIT_OK, value: a}]
---
apiVersion: batch/v1
kind: CronJob
metadata: {name: scheduled}
spec:
  schedule: '* * * * *'
  jobTemplate:
    spec:
      template:
        spec:
          restartPolicy: Never
          containers:
            - name: cron-worker
              image: example.invalid/test
              env: [{name: CRON_OK, value: a}]
---
apiVersion: apps/v1
kind: DaemonSet
metadata: {name: agents}
spec:
  selector: {matchLabels: {app: agent}}
  template:
    metadata: {labels: {app: agent}}
    spec:
      containers:
        - name: agent
          image: example.invalid/test
          env: [{name: CROSS_CONTAINER, value: a}]
        - name: helper
          image: example.invalid/test
          env: [{name: CROSS_CONTAINER, value: b}]
'''
destination.write_text(source.read_text(encoding='utf-8') + extra, encoding='utf-8')
PY

HELM="$(stub_helm "$WORK/breadth-base.yaml" breadth-base)" \
  expect_gate 'same name in different containers remains valid' pass 'DISPATCHABLE' "$CHART_DIR"

python3 - "$WORK/breadth-base.yaml" "$WORK/dup-pod.yaml" "$WORK/dup-init.yaml" "$WORK/dup-cron.yaml" "$WORK/dup-controller.yaml" <<'PY'
import sys
from pathlib import Path

base = Path(sys.argv[1]).read_text(encoding='utf-8')
mutations = [
    ('SHARED_OK, value: a}', 'SHARED_OK, value: a}, {name: SHARED_OK, value: b}'),
    ('INIT_OK, value: a}', 'INIT_OK, value: a}, {name: INIT_OK, value: b}'),
    ('CRON_OK, value: a}', 'CRON_OK, value: a}, {name: CRON_OK, value: b}'),
    ('CROSS_CONTAINER, value: a}',
     'CROSS_CONTAINER, value: a}, {name: CROSS_CONTAINER, value: duplicate}'),
]
for destination, (before, after) in zip(sys.argv[2:], mutations):
    assert base.count(before) == 1
    Path(destination).write_text(base.replace(before, after), encoding='utf-8')
PY

HELM="$(stub_helm "$WORK/dup-pod.yaml" dup-pod)" \
  expect_gate 'direct Pod regular-container duplicates are rejected' \
  fail 'Pod/direct-pod containers/worker: SHARED_OK' "$CHART_DIR"
HELM="$(stub_helm "$WORK/dup-init.yaml" dup-init)" \
  expect_gate 'direct Pod init-container duplicates are rejected' \
  fail 'Pod/direct-pod initContainers/setup: INIT_OK' "$CHART_DIR"
HELM="$(stub_helm "$WORK/dup-cron.yaml" dup-cron)" \
  expect_gate 'CronJob nested Pod-template duplicates are rejected' \
  fail 'CronJob/scheduled containers/cron-worker: CRON_OK' "$CHART_DIR"
HELM="$(stub_helm "$WORK/dup-controller.yaml" dup-controller)" \
  expect_gate 'controller Pod-template duplicates are rejected' \
  fail 'DaemonSet/agents containers/agent: CROSS_CONTAINER' "$CHART_DIR"

# ---------------------------------------------------------------------------
if [ "$failures" -ne 0 ]; then
  printf 'FAIL: %s of %s render-gate cases failed\n' "$failures" "$case_index" >&2
  exit 1
fi
printf 'PASS: render preflight contract (%s cases)\n' "$case_index"
