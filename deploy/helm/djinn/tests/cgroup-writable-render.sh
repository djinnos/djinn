#!/usr/bin/env bash
# Hermetic acceptance contract for the kubelet-delegated writable cgroup rollout.
# Usage: bash deploy/helm/djinn/tests/cgroup-writable-render.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHART_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/../../../.." && pwd)"
FIXTURES="$SCRIPT_DIR/fixtures/cgroup-writable"
CONFORMANCE="$REPO_DIR/deploy/node/k3s/djinn-cgroup-writable-conformance.sh"
TEMPLATE="$REPO_DIR/deploy/node/k3s/containerd/config.toml.tmpl"

require_tool() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "FAIL: required test tool '$1' is not installed" >&2
    exit 1
  }
}
require_tool helm
require_tool python3
require_tool bash

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# RuntimeClass remains an installation gate: defaults omit it and enabling it
# must render precisely the runtime handler and eligibility selector.
helm template cgroup-writable-default "$CHART_DIR" --is-upgrade >"$WORK/default.yaml"
helm template cgroup-writable-enabled "$CHART_DIR" --is-upgrade \
  --set cgroupWritableRuntimeClass.enabled=true >"$WORK/enabled.yaml"
if helm template cgroup-writable-invalid "$CHART_DIR" --set-string cgroupWritableRuntimeClass.enabled=not-a-bool >/dev/null 2>&1; then
  echo 'FAIL: cgroupWritableRuntimeClass.enabled accepted a malformed rollout value' >&2
  exit 1
fi
python3 - "$WORK/default.yaml" "$WORK/enabled.yaml" <<'PY'
import sys


def documents(text):
    return [doc for doc in text.split('\n---\n') if doc.strip()]


def runtime_classes(text):
    return [doc for doc in documents(text) if '\nkind: RuntimeClass\n' in f'\n{doc}']

base, enabled = (open(path, encoding='utf-8').read() for path in sys.argv[1:])
assert not runtime_classes(base), 'disabled defaults rendered RuntimeClass'
classes = runtime_classes(enabled)
assert len(classes) == 1, f'enabled render expected one RuntimeClass, got {len(classes)}'
manifest = classes[0]
for exact in (
    'name: djinn-cgroup-writable',
    'handler: runc-cgroupwritable',
    'djinn.io/cgroup-writable: "true"',
):
    assert exact in manifest, f'missing RuntimeClass contract: {exact}'
assert 'runtimeClassName:' not in enabled, 'foundation must not assign RuntimeClass to task-run PodSpecs'
PY

# The alert is evaluated against compact event fixtures so the matcher is not
# merely present in a rendered string: both required labels are necessary.
python3 - "$CHART_DIR/templates/configmap-monitoring.yaml" "$FIXTURES" <<'PY'
import json
import re
import sys
from pathlib import Path

source, fixture_dir = map(Path, sys.argv[1:])
text = source.read_text(encoding='utf-8')
match = re.search(r'alert: DjinnCgroupWritableSandboxFailure\n\s+expr: (.+)', text)
assert match, 'sandbox alert missing'
expr = match.group(1)
assert 'type="Warning"' in expr
assert 'reason="FailedCreatePodSandBox"' in expr
assert 'message=~".*runc-cgroupwritable.*"' in expr

def matches(event):
    return (event['type'] == 'Warning'
            and event['reason'] == 'FailedCreatePodSandBox'
            and 'runc-cgroupwritable' in event['message'])

expected = {
    'alert-positive.json': True,
    'alert-missing-handler.json': False,
    'alert-missing-reason.json': False,
}
for name, wanted in expected.items():
    got = matches(json.loads((fixture_dir / name).read_text(encoding='utf-8')))
    assert got == wanted, f'{name}: expected {wanted}, got {got}'
PY

# The pinned table is intentionally exact, and task-run sources/renders must
# never gain a cgroup hostPath during this foundation-only rollout.
python3 - "$TEMPLATE" "$CONFORMANCE" "$WORK/enabled.yaml" "$REPO_DIR/deploy/helm/djinn/templates" <<'PY'
import re
import sys
from pathlib import Path

template, conformance, rendered, templates = map(Path, sys.argv[1:])
expected = '''[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.runc-cgroupwritable]
  runtime_type = "io.containerd.runc.v2"
  cgroup_writable = true'''
assert expected in template.read_text(encoding='utf-8'), 'pinned runtime table changed'
conformance_text = conformance.read_text(encoding='utf-8')
assert "RUNTIME_TABLE=" in conformance_text, 'conformance no longer validates live table'
assert "RUNTIME_CLASS='djinn-cgroup-writable'" in conformance_text
assert 'runtimeClassName: $RUNTIME_CLASS' in conformance_text
# This asserts the failure branch of must_deny itself, instead of merely
# checking for filenames. The fakes below model each named successful write.
must_deny = re.search(r'must_deny\(\) \{([^\n]+)\}', conformance_text)
assert must_deny, 'worker mutation denial helper missing'
assert 'if sh -c "$1"; then' in must_deny.group(1) and 'exit 1' in must_deny.group(1), \
    'must_deny no longer rejects a successful worker mutation'
for name, required_probe in {
    'child': 'must_deny "mkdir',
    'cpu-max': 'cpu.max',
    'cgroup-procs': 'cgroup.procs',
    'cgroup-kill': 'cgroup.kill',
    'launcher-leaf': 'must_deny "rmdir',
    'process-move': 'printf \\"\\$\\$\\"',
}.items():
    assert required_probe in conformance_text, f'worker mutation denial missing: {name}'
for source in list(templates.rglob('*')) + [rendered]:
    if source.is_file():
        text = source.read_text(encoding='utf-8')
        assert '/sys/fs/cgroup' not in text, f'cgroup hostPath source appeared in {source}'
PY

# There is one deployment-side owner for label mutation. References in the chart
# are scheduling selectors/comments, never kubectl label operations.
python3 - "$REPO_DIR/deploy" "$CONFORMANCE" <<'PY'
import sys
from pathlib import Path

root, owner = map(Path, sys.argv[1:])
mutators = []
for path in root.rglob('*'):
    if not path.is_file() or 'tests' in path.parts:
        continue
    text = path.read_text(encoding='utf-8', errors='ignore')
    if 'label node' in text and ('cgroup-writable' in text or '$LABEL' in text):
        mutators.append(path.resolve())
assert mutators == [owner.resolve()], f'label mutation owner mismatch: {mutators}'
PY

make_stubs() {
  local case_name=$1 dir=$2 template_copy=$3 live_config=$4 log=$5 manifest=$6
  mkdir -p "$dir"
  cat >"$dir/id" <<'EOF'
#!/usr/bin/env bash
[ "$1" = -u ] && { echo 0; exit 0; }
exec /usr/bin/id "$@"
EOF
  cat >"$dir/systemctl" <<EOF
#!/usr/bin/env bash
set -euo pipefail
printf 'restart %s\n' "\$*" >>'$log'
if [ "${case_name}" = handler-removed ]; then
  printf '[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.runc-cgroupwritable]\n  runtime_type = "io.containerd.runc.v2"\n' >'$live_config'
else
  cp '$template_copy' '$live_config'
fi
EOF
  cat >"$dir/kubectl" <<EOF
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "\$*" >>'$log'
case "\$1" in
  label) exit 0 ;;
  get)
    if [ "\$2" = node ]; then exit 0; fi
    if [ "\$2" = pod ] && [ "\${*: -2}" = '-o json' ]; then
      if [ "${case_name}" = sandbox ]; then printf '%s' 'Warning FailedCreatePodSandBox runc-cgroupwritable'; fi
      exit 0
    fi
    if [ "\$2" = pod ]; then
      if [ "${case_name}" = wrong-node ]; then printf 'fixture-node|other-node'; else printf 'fixture-node|fixture-node'; fi
      exit 0
    fi
    ;;
  apply)
    cat >'$manifest'
    grep -F 'runtimeClassName: djinn-cgroup-writable' '$manifest' >/dev/null
    grep -F 'nodeName: fixture-node' '$manifest' >/dev/null
    exit 0 ;;
  wait)
    if [ "${case_name}" = timeout ] || [ "${case_name}" = sandbox ]; then
      exit 1
    fi
    exit 0 ;;
  exec)
    # kubectl receives the whole launcher/worker program. Inspect it before
    # modeling the specific successful operation which must_deny rejects.
    probe="\$*"
    case "${case_name}" in
      readonly)
        # Inspect the launcher's actual writable-root operations, not merely
        # the cgroup filesystem-type observation that precedes them.
        [[ "\$probe" == *'mkdir "\$child"'* ]] || exit 91
        [[ "\$probe" == *'rmdir "\$child"'* ]] || exit 91
        printf 'fixture worker-probe root=read-only result=unexpected-success\n' >>'$log' ;;
      namespace-escape)
        [[ "\$probe" == *'[ ! -d "\$root/system.slice" ]'* ]] || exit 92
        printf 'fixture worker-probe namespace=host-visible result=unexpected-success\n' >>'$log' ;;
      mutation-child)
        [[ "\$probe" == *'mkdir \\"\$leaf\\"'* ]] || exit 93
        printf 'fixture worker-probe mutation=child-creation result=unexpected-success\n' >>'$log' ;;
      mutation-cpu-max)
        # The redirection distinguishes the denied write from the preceding
        # read used to retain cpu.max's valid current value.
        [[ "\$probe" == *'> \\"\$root/cpu.max\\"'* ]] || exit 94
        printf 'fixture worker-probe mutation=cpu.max result=unexpected-success\n' >>'$log' ;;
      mutation-cgroup-procs)
        [[ "\$probe" == *'printf 1 > \\"\$root/cgroup.procs\\"'* ]] || exit 95
        printf 'fixture worker-probe mutation=cgroup.procs result=unexpected-success\n' >>'$log' ;;
      mutation-cgroup-kill)
        [[ "\$probe" == *'printf 1 > \\"\$root/cgroup.kill\\"'* ]] || exit 96
        printf 'fixture worker-probe mutation=cgroup.kill result=unexpected-success\n' >>'$log' ;;
      mutation-launcher-leaf)
        [[ "\$probe" == *'rmdir \\"\$launcher_leaf\\"'* ]] || exit 97
        printf 'fixture worker-probe mutation=launcher-leaf result=unexpected-success\n' >>'$log' ;;
      mutation-process-move)
        # Match the root process-movement write itself; neither its explanatory
        # comment nor the separate launcher-leaf movement is sufficient.
        [[ "\$probe" == *'printf \\"\\\$\\\$\\" > \\"\$root/cgroup.procs\\"'* ]] || exit 98
        printf 'fixture worker-probe mutation=process-movement result=unexpected-success\n' >>'$log' ;;
      *) exit 0 ;;
    esac
    # The nonzero result models must_deny aborting after that write succeeds.
    # Its source-level semantic assertion above catches a regression that
    # would instead accept a successful sh -c mutation.
    exit 1
    ;;
  delete) exit 0 ;;
esac
exit 0
EOF
  chmod +x "$dir/id" "$dir/systemctl" "$dir/kubectl"
}

run_lifecycle_case() {
  local case_name=$1 expected=$2 case_dir="$WORK/$1" output status
  mkdir -p "$case_dir"
  cp "$TEMPLATE" "$case_dir/template.toml"
  : >"$case_dir/log"
  make_stubs "$case_name" "$case_dir/bin" "$case_dir/template.toml" "$case_dir/live.toml" "$case_dir/log" "$case_dir/manifest.yaml"
  set +e
  PATH="$case_dir/bin:$PATH" \
    DJINN_KUBECTL=kubectl \
    DJINN_K3S_RESTART_CMD='systemctl restart k3s' \
    DJINN_CGROUP_TEMPLATE_SOURCE="$case_dir/template.toml" \
    DJINN_CGROUP_TEMPLATE_PATH="$case_dir/installed.toml" \
    DJINN_CGROUP_LIVE_CONFIG_PATH="$case_dir/live.toml" \
    bash "$CONFORMANCE" --node fixture-node >"$case_dir/stdout" 2>"$case_dir/stderr"
  status=$?
  set -e
  if [ "$expected" = success ]; then
    [ "$status" -eq 0 ] || { cat "$case_dir/stderr" >&2; return 1; }
    [ "$(cat "$case_dir/stdout")" = 'PASS node=fixture-node handler=runc-cgroupwritable cgroup_root=/ writable=true isolated=true worker_denials=true' ]
    grep -Fx 'label node fixture-node djinn.io/cgroup-writable- --overwrite' "$case_dir/log" >/dev/null
    grep -Fx 'label node fixture-node djinn.io/cgroup-writable=true --overwrite' "$case_dir/log" >/dev/null
    unlabel_line=$(grep -nFx 'label node fixture-node djinn.io/cgroup-writable- --overwrite' "$case_dir/log" | head -1 | cut -d: -f1)
    restart_line=$(grep -nFx 'restart restart k3s' "$case_dir/log" | head -1 | cut -d: -f1)
    label_line=$(grep -nFx 'label node fixture-node djinn.io/cgroup-writable=true --overwrite' "$case_dir/log" | head -1 | cut -d: -f1)
    exec_line=$(grep -n '^exec ' "$case_dir/log" | head -1 | cut -d: -f1)
    [ "$unlabel_line" -lt "$restart_line" ]
    [ "$label_line" -gt "$exec_line" ]
  else
    [ "$status" -ne 0 ] || { echo "FAIL: $case_name unexpectedly succeeded" >&2; return 1; }
    [ ! -s "$case_dir/stdout" ] || { echo "FAIL: $case_name printed PASS" >&2; return 1; }
    if [ "$case_name" != handler-removed ]; then
      grep -q '^delete pod ' "$case_dir/log"
    fi
    # Failures after initial unlabel must remove eligibility again in cleanup.
    [ "$(grep -Fc 'label node fixture-node djinn.io/cgroup-writable- --overwrite' "$case_dir/log")" -ge 2 ]
    ! grep -Fq 'djinn.io/cgroup-writable=true' "$case_dir/log"
    case "$case_name" in
      readonly) expected_probe='fixture worker-probe root=read-only result=unexpected-success' ;;
      namespace-escape) expected_probe='fixture worker-probe namespace=host-visible result=unexpected-success' ;;
      mutation-child) expected_probe='fixture worker-probe mutation=child-creation result=unexpected-success' ;;
      mutation-cpu-max) expected_probe='fixture worker-probe mutation=cpu.max result=unexpected-success' ;;
      mutation-cgroup-procs) expected_probe='fixture worker-probe mutation=cgroup.procs result=unexpected-success' ;;
      mutation-cgroup-kill) expected_probe='fixture worker-probe mutation=cgroup.kill result=unexpected-success' ;;
      mutation-launcher-leaf) expected_probe='fixture worker-probe mutation=launcher-leaf result=unexpected-success' ;;
      mutation-process-move) expected_probe='fixture worker-probe mutation=process-movement result=unexpected-success' ;;
      *) expected_probe='' ;;
    esac
    if [ -n "$expected_probe" ]; then
      grep -Fx "$expected_probe" "$case_dir/log" >/dev/null
    fi
  fi
}

# Fake command/live-config seams exercise the real lifecycle, not a cluster.
run_lifecycle_case success success
for case_name in timeout wrong-node sandbox readonly namespace-escape \
  mutation-child mutation-cpu-max mutation-cgroup-procs mutation-cgroup-kill \
  mutation-launcher-leaf mutation-process-move handler-removed; do
  run_lifecycle_case "$case_name" failure
done

echo 'PASS: cgroup writable render and lifecycle contract'
