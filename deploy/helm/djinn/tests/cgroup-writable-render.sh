#!/usr/bin/env bash
# Hermetic acceptance contract for the kubelet-delegated writable cgroup rollout.
# Usage: bash deploy/helm/djinn/tests/cgroup-writable-render.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# DJINN_CHART_DIR lets a caller point the SAME contract at a mutated copy of the
# chart. deploy/preflight/tests/render-gate.sh uses it to prove this script
# actually rejects a values.yaml that restores the undispatchable default
# pairing, rather than merely describing it.
CHART_DIR="${DJINN_CHART_DIR:-$(cd "$SCRIPT_DIR/.." && pwd)}"
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

# The preparation release has two independent switches. Defaults must render
# neither the RuntimeClass nor a task-run assignment; enabling only
# runtimeClass installs the existing handler and selector without activating
# task-run assignment.
helm template cgroup-writable-default "$CHART_DIR" --is-upgrade >"$WORK/default.yaml"
helm template cgroup-writable-preparation "$CHART_DIR" --is-upgrade \
  --set cgroupWritable.runtimeClass.enabled=true,cgroupWritable.taskRuns.enabled=false \
  --set cgroupLauncher.mode=disabled \
  --set imagePipeline.controller.launcherAuthorityProtocol=leaf-v1 >"$WORK/preparation.yaml"
helm template cgroup-writable-activation "$CHART_DIR" --is-upgrade \
  --set cgroupWritable.runtimeClass.enabled=true,cgroupWritable.taskRuns.enabled=true >"$WORK/activation.yaml"
# The fully armed production pairing: an armed launcher together with the
# task-run RuntimeClass assignment it cannot dispatch without.
helm template cgroup-writable-armed "$CHART_DIR" --is-upgrade \
  --set cgroupLauncher.mode=required \
  --set cgroupWritable.runtimeClass.enabled=true,cgroupWritable.taskRuns.enabled=true >"$WORK/armed.yaml"
for invalid in \
  'cgroupWritable.runtimeClass.enabled=not-a-bool' \
  'cgroupWritable.taskRuns.enabled=not-a-bool'; do
  if helm template cgroup-writable-invalid "$CHART_DIR" --set-string "$invalid" >/dev/null 2>&1; then
    echo "FAIL: cgroupWritable accepted malformed preparation value: $invalid" >&2
    exit 1
  fi
done
for invalid in \
  'cgroupWritable.unknown.enabled=true' \
  'cgroupWritable.runtimeClass.unknown=true' \
  'cgroupWritable.runtimeClass.enabled=false,cgroupWritable.taskRuns.enabled=true'; do
  if helm template cgroup-writable-invalid "$CHART_DIR" --set "$invalid" >/dev/null 2>&1; then
    echo "FAIL: cgroupWritable accepted invalid preparation value: $invalid" >&2
    exit 1
  fi
done
python3 - "$WORK/default.yaml" "$WORK/preparation.yaml" "$WORK/activation.yaml" "$WORK/armed.yaml" "$FIXTURES" <<'PY'
import json
import sys
from pathlib import Path


def documents(text):
    return [doc for doc in text.split('\n---\n') if doc.strip()]


def runtime_classes(text):
    return [doc for doc in documents(text) if '\nkind: RuntimeClass\n' in f'\n{doc}']


TASK_RUN_CLASS = 'djinn-cgroup-writable'
PROBE_CLASS = 'djinn-cgroup-writable-probe'


def runtime_class_named(text, name):
    """Return the single RuntimeClass document whose metadata.name is `name`.

    The trailing newline matters: 'name: djinn-cgroup-writable' is a prefix of
    'name: djinn-cgroup-writable-probe', so a substring test would happily
    hand back the wrong object and certify the wrong scheduling contract.
    """
    matches = [doc for doc in runtime_classes(text) if f'\n  name: {name}\n' in f'\n{doc}\n']
    assert len(matches) == 1, f'expected exactly one RuntimeClass named {name}, got {len(matches)}'
    return matches[0]


def effective(doc):
    """Drop comment lines so an assertion reads the spec, not the prose.

    The probe class's rationale necessarily mentions the very words its own
    negative assertions forbid ('nodeSelector', the eligibility marker), and a
    comment is not a scheduling constraint.
    """
    return '\n'.join(line for line in doc.splitlines() if not line.lstrip().startswith('#'))


base, preparation, activation, armed = (
    Path(path).read_text(encoding='utf-8') for path in sys.argv[1:5]
)
fixtures = Path(sys.argv[5])
assert len(runtime_classes(base)) == 2, 'armed defaults must render both RuntimeClasses'
assert '''- name: DJINN_IMAGE_LAUNCHER_AUTHORITY_PROTOCOL
              value: "resize-v2"''' in base, 'shipped chart must arm resize-v2 authority explicitly'
runtime_class_named(base, TASK_RUN_CLASS)
runtime_class_named(base, PROBE_CLASS)

# Enabling the single gate must render BOTH classes: the constrained task-run
# class and the unconstrained probe class. They exist for opposite reasons and
# must never be separately switchable — a probe class without its task-run
# class would be an ungated handler, and a task-run class without its probe
# class is the deadlock this pair was split to remove (node conformance cannot
# run on a node that does not yet carry the marker conformance itself earns).
classes = runtime_classes(preparation)
assert len(classes) == 2, f'preparation render expected two RuntimeClasses, got {len(classes)}'

task_run_class = runtime_class_named(preparation, TASK_RUN_CLASS)
for exact in (
    f'name: {TASK_RUN_CLASS}',
    'handler: runc-cgroupwritable',
    'scheduling:',
    'nodeSelector:',
    'djinn.io/cgroup-writable: "true"',
):
    assert exact in effective(task_run_class), f'missing task-run RuntimeClass contract: {exact}'

probe_class = runtime_class_named(preparation, PROBE_CLASS)
probe_spec = effective(probe_class)
for exact in (f'name: {PROBE_CLASS}', 'handler: runc-cgroupwritable'):
    assert exact in probe_spec, f'missing probe RuntimeClass contract: {exact}'
for forbidden in ('scheduling:', 'nodeSelector:', 'djinn.io/cgroup-writable'):
    assert forbidden not in probe_spec, (
        f'probe RuntimeClass carries {forbidden!r}; the RuntimeClass admission controller '
        'would merge it into the probe Pod as a NodeAffinity predicate, which the kubelet '
        'rejects on the very unlabeled node conformance exists to evaluate — spec.nodeName '
        'bypasses the scheduler, not the kubelet admission predicate'
    )

assert 'runtimeClassName:' not in preparation, 'preparation must not assign RuntimeClass to task-run PodSpecs'
assert len(runtime_classes(activation)) == 2, 'activation must retain both RuntimeClasses'
runtime_class_named(activation, TASK_RUN_CLASS)
runtime_class_named(activation, PROBE_CLASS)
assert len(runtime_classes(armed)) == 2, 'armed must retain both RuntimeClasses'

# No chart-rendered PodSpec may ever name the probe class. The probe class is
# reachable only from the node conformance program.
for label, rendered in (
    ('chart defaults', base),
    ('preparation', preparation),
    ('activation', activation),
    ('armed', armed),
):
    for line in rendered.splitlines():
        stripped = line.strip()
        if stripped.startswith('runtimeClassName:'):
            assert stripped == f'runtimeClassName: {TASK_RUN_CLASS}', (
                f'{label}: rendered PodSpec names {stripped!r}; task-run Pods must use the '
                f'node-selected {TASK_RUN_CLASS} class only'
            )


def server_env(rendered, name):
    marker = f'name: {name}\n              value: '
    assert marker in rendered, f'server render omitted {name}'
    return rendered.split(marker, 1)[1].splitlines()[0].strip().strip('"')


def server_activation(rendered):
    return server_env(rendered, 'DJINN_K8S_TASK_RUN_CGROUP_WRITABLE_ENABLED')


assert server_activation(base) == 'true', 'armed defaults omitted task-run activation'
assert server_activation(preparation) == 'false', 'preparation render armed task-run activation'
assert server_activation(activation) == 'true', 'activation render omitted task-run activation'


def assert_dispatchable(rendered, label):
    """Reject a render djinn-server would refuse to dispatch.

    THIS FILE USED TO CERTIFY THE OPPOSITE. It asserted the default render sets
    DJINN_K8S_TASK_RUN_CGROUP_WRITABLE_ENABLED=false and never asked which
    cgroupLauncher.mode was rendered beside it. Stock values shipped `required`
    with `false` — the exact pairing `validate_enforcement_render` refuses — so
    the "correct default render" this test approved was one under which a fresh
    install creates zero Jobs. That is what took production down on 2026-07-29,
    with CI green throughout.

    The authoritative gate is `deploy/preflight/render-gate.sh`, which runs the
    real validator over a real render. This assertion is the hermetic, helm-only
    half: it keeps the chart-contract lane, which needs no Rust toolchain, from
    ever reintroducing the pairing.
    """
    mode = server_env(rendered, 'DJINN_K8S_CGROUP_LAUNCHER_MODE')
    enabled = server_activation(rendered)
    assert not (mode == 'required' and enabled == 'false'), (
        f'{label}: undispatchable render — cgroupLauncher.mode={mode} paired with '
        f'cgroupWritable.taskRuns.enabled={enabled} is refused at dispatch as '
        f'RenderValidationError::MissingDelegatedRuntimeClass, so no Job is ever created'
    )


for label, rendered in (
    ('chart defaults', base),
    ('preparation', preparation),
    ('activation', activation),
    ('armed', armed),
):
    assert_dispatchable(rendered, label)

assert server_env(base, 'DJINN_K8S_CGROUP_LAUNCHER_MODE') == 'required'


def fail(message, contract):
    raise AssertionError(f'{contract["release"]}: {message}')


def launcher(contract):
    matches = [item for item in contract['taskRunPod']['spec']['initContainers'] if item['name'] == 'cgroup-launcher']
    assert len(matches) == 1, f'{contract["release"]}: expected one launcher'
    return matches[0]


def validate_release_contract(contract):
    """Validate a release artifact without contacting a cluster.

    A rollback selects the old preparation artifact for subsequently created
    task-run Pods; it never edits task-run Pods that activation already made.
    """
    server = contract['server']
    pod = contract['taskRunPod']['spec']
    sidecar = launcher(contract)
    enabled = server['environment']['DJINN_K8S_TASK_RUN_CGROUP_WRITABLE_ENABLED'] == 'true'
    generation = server['runtimeImageGeneration']
    security = sidecar['securityContext']
    caps = security['capabilities']
    volume_names = {volume['name'] for volume in pod['volumes']}

    assert security['allowPrivilegeEscalation'] is False
    assert security['readOnlyRootFilesystem'] is True
    assert security['seccompProfile'] == {'type': 'RuntimeDefault'}
    assert caps['drop'] == ['ALL']
    if pod.get('runtimeClassName') == PROBE_CLASS:
        fail('task-run PodSpec assigned the conformance probe RuntimeClass, '
             'which carries no eligibility nodeSelector', contract)
    for volume in pod['volumes']:
        if 'hostPath' in volume and volume['hostPath'].get('path') == '/sys/fs/cgroup':
            fail('enabled task-run PodSpec contains a /sys/fs/cgroup hostPath', contract)

    if generation == 'privileged':
        if enabled or 'runtimeClassName' in pod:
            fail('old privileged launcher must remain unassigned in preparation', contract)
        assert sidecar['imageGeneration'] == 'privileged'
        assert sidecar['env']['DJINN_LAUNCHER_CGROUP_ROOT'] == '/run/djinn-cgroup'
        assert sidecar['volumeMounts'] == [{'name': 'launcher-cgroup', 'mountPath': '/run/djinn-cgroup'}]
        assert volume_names == {'launcher-cgroup'}
        assert caps['add'] == ['CHOWN', 'SETGID', 'SETUID', 'SETPCAP', 'SYS_ADMIN', 'SYS_RESOURCE']
        assert security['appArmorProfile'] == {'type': 'Unconfined'}
        return

    if generation != 'privilege-free':
        fail('unknown runtime image generation', contract)
    if not enabled or pod.get('runtimeClassName') != TASK_RUN_CLASS:
        fail('privilege-free launcher is not atomically paired with task-run RuntimeClass assignment', contract)
    assert sidecar['imageGeneration'] == 'privilege-free'
    assert sidecar['env']['DJINN_LAUNCHER_CGROUP_ROOT'] == '/sys/fs/cgroup'
    assert sidecar['volumeMounts'] == []
    assert volume_names == set()
    assert caps['add'] == ['CHOWN', 'SETGID', 'SETUID', 'SETPCAP']
    assert 'appArmorProfile' not in security


preparation_contract = json.loads((fixtures / 'preparation-task-run.json').read_text(encoding='utf-8'))
activation_contract = json.loads((fixtures / 'activation-task-run.json').read_text(encoding='utf-8'))
assert server_activation(preparation) == preparation_contract['server']['environment']['DJINN_K8S_TASK_RUN_CGROUP_WRITABLE_ENABLED']
assert server_activation(activation) == activation_contract['server']['environment']['DJINN_K8S_TASK_RUN_CGROUP_WRITABLE_ENABLED']
validate_release_contract(preparation_contract)
validate_release_contract(activation_contract)

# Atomic rollback restores exactly the complete preparation render. It does not
# claim a mutation of already-running new-path Pods, which may finish normally.
rollback_contract = preparation_contract
assert rollback_contract == preparation_contract
validate_release_contract(rollback_contract)

for fixture, expected in (
    ('privilege-free-without-runtimeclass.json', 'RuntimeClass assignment'),
    ('enabled-task-run-hostpath.json', '/sys/fs/cgroup hostPath'),
    ('probe-class-task-run.json', 'conformance probe RuntimeClass'),
):
    try:
        validate_release_contract(json.loads((fixtures / fixture).read_text(encoding='utf-8')))
    except AssertionError as error:
        assert expected in str(error), f'{fixture}: unexpected rejection: {error}'
    else:
        raise AssertionError(f'{fixture}: forbidden release artifact was accepted')
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
python3 - "$TEMPLATE" "$CONFORMANCE" "$WORK/preparation.yaml" "$REPO_DIR/deploy/helm/djinn/templates" <<'PY'
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
# The probe must name the unconstrained probe class. Naming the task-run class
# is the deadlock: its merged nodeSelector becomes a NodeAffinity predicate the
# kubelet enforces even for a Pod bound by spec.nodeName, so the probe can only
# run on a node already carrying the marker that only a passing probe earns.
assert "PROBE_RUNTIME_CLASS='djinn-cgroup-writable-probe'" in conformance_text, \
    'conformance no longer pins the probe-only RuntimeClass'
assert 'runtimeClassName: $PROBE_RUNTIME_CLASS' in conformance_text, \
    'conformance probe manifest no longer uses the probe-only RuntimeClass'
assert "RUNTIME_CLASS='djinn-cgroup-writable'" not in conformance_text, \
    'conformance reintroduced the node-selected task-run RuntimeClass'
# spec.nodeName binding is what makes the probe an observation of ONE node.
assert 'nodeName: $NODE_NAME' in conformance_text, 'conformance probe is no longer node-bound'
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
    selector=\${*: -1}
    if [ "\$2" = node ]; then
      case "\$selector" in
        # The node's own InternalIP, which the identity check compares the
        # probe's reported host against. The label read stays empty: this
        # program removed eligibility before anything else ran.
        *status.addresses*) printf '%s\n' '10.10.0.7' ;;
        *) : ;;
      esac
      exit 0
    fi
    if [ "\$2" = pod ] && [ "\$selector" = json ]; then
      if [ "${case_name}" = sandbox ]; then printf '%s' 'Warning FailedCreatePodSandBox runc-cgroupwritable'; fi
      exit 0
    fi
    if [ "\$2" = pod ]; then
      # wrong-node models a probe bound to a different node than the one
      # requested. Every other case reports the requested binding and a host
      # that is the requested node's own InternalIP.
      if [ "${case_name}" = wrong-node ]; then pod_node_name=other-node; else pod_node_name=fixture-node; fi
      rendered=\${selector#jsonpath=}
      rendered=\${rendered//\\{.spec.nodeName\\}/\$pod_node_name}
      rendered=\${rendered//\\{.status.hostIP\\}/10.10.0.7}
      printf '%s' "\$rendered"
      exit 0
    fi
    ;;
  apply)
    cat >'$manifest'
    # Exact-match: 'runtimeClassName: djinn-cgroup-writable' is a PREFIX of the
    # probe class line, so a substring grep cannot tell the two apart.
    grep -Fx '  runtimeClassName: djinn-cgroup-writable-probe' '$manifest' >/dev/null
    if grep -Fx '  runtimeClassName: djinn-cgroup-writable' '$manifest' >/dev/null; then
      printf 'probe used the node-selected task-run RuntimeClass\n' >&2
      exit 1
    fi
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
