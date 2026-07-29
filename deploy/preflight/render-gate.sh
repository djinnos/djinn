#!/usr/bin/env bash
# Deploy-time preflight: would this chart render, with THESE values, dispatch?
#
# WHY THIS EXISTS
# ---------------
# v0.7.25 shipped `djinn_k8s::launcher::validate_enforcement_render`, a
# fail-closed check that runs at dispatch before a Job is submitted. Its
# rejection condition — `cgroupLauncher.mode: required` paired with
# `cgroupWritable.taskRuns.enabled: false` — was *the chart's own default
# pairing*. Every dispatch in production died at `runtime.prepare` with
# `MissingDelegatedRuntimeClass`, a Job was never created, and a fresh
# `helm install` with stock values could not have run a single task on any
# node. Nothing caught it because no test ever ran the real validator against a
# real render: the chart tests asserted the rendered *values* were the expected
# ones, which they were.
#
# This gate closes exactly that hole and nothing more. It renders the chart the
# way a deploy would, pulls the `DJINN_K8S_*` environment out of the RENDERED
# djinn-server container, and runs the crate's OWN validator over it. It is a
# render-shaped question answered by the dispatch-time code, so it cannot drift
# from what dispatch will decide.
#
# WHAT IT IS NOT
# --------------
# It is not a cluster check. It answers "would the server refuse this render at
# dispatch", not "do the nodes satisfy the runtime prerequisites" — node
# labels, the containerd handler, and cgroup delegation are the subject of
# `deploy/node/k3s/djinn-cgroup-writable-conformance.sh`.
#
# USAGE
#   deploy/preflight/render-gate.sh <chart-dir> [helm template args...]
#
#   # stock chart defaults
#   deploy/preflight/render-gate.sh deploy/helm/djinn
#
#   # the values a real deployment would apply (production values live in the
#   # djinn-vps-deploy repo, so an operator points at them from there)
#   deploy/preflight/render-gate.sh deploy/helm/djinn --values prod-values.yaml
#
# Exit status: 0 the render dispatches; non-zero it does not, with the
# rejecting `RenderValidationError` variant named on stderr.
#
# Environment overrides (used by deploy/preflight/tests/render-gate.sh):
#   HELM            helm executable (default: helm)
#   RENDER_GATE_BIN path to the `render-gate` binary (default: build it)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
HELM="${HELM:-helm}"
RELEASE_NAME="djinn-render-gate"

die() {
  printf 'render-gate: %s\n' "$1" >&2
  exit 1
}

usage() {
  cat <<'EOF'
usage: render-gate.sh <chart-dir> [helm template args...]

Renders the chart, extracts the DJINN_K8S_* environment of the rendered
djinn-server container, and runs djinn_k8s::launcher::validate_enforcement_render
over it. Exit 0 when the render would dispatch, non-zero when it would not.

  render-gate.sh deploy/helm/djinn
  render-gate.sh deploy/helm/djinn --values prod-values.yaml
  render-gate.sh deploy/helm/djinn --set cgroupLauncher.mode=disabled
EOF
}

case "${1:-}" in
  -h | --help)
    usage
    exit 0
    ;;
  '')
    usage >&2
    die "a chart directory is required"
    ;;
esac

CHART_DIR="$1"
shift
[ -d "$CHART_DIR" ] || die "chart directory does not exist: $CHART_DIR"
CHART_DIR="$(cd "$CHART_DIR" && pwd)"

command -v python3 >/dev/null 2>&1 || die "required tool 'python3' is not installed"
command -v "$HELM" >/dev/null 2>&1 || die "required tool '$HELM' is not installed"

# Locate the validator binary. It is a thin shell over the real
# `validate_enforcement_render`; building it is the only way to run the actual
# rule rather than a transcription of it.
BIN="${RENDER_GATE_BIN:-}"
if [ -z "$BIN" ]; then
  BIN="${CARGO_TARGET_DIR:-$REPO_DIR/server/target}/debug/render-gate"
  if [ ! -x "$BIN" ]; then
    command -v cargo >/dev/null 2>&1 ||
      die "render-gate binary missing ($BIN) and cargo is unavailable to build it"
    cargo build --manifest-path "$REPO_DIR/server/Cargo.toml" \
      -p djinn-k8s --bin render-gate >&2
  fi
fi
[ -x "$BIN" ] || die "render-gate binary is not executable: $BIN"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# The render is the subject under test. Everything below reads THIS file; the
# `--set`/`--values` arguments are never consulted again, which is what makes
# the gate honest about templates that ignore, override, or drop a value.
#
# `--is-upgrade` is supplied unconditionally, and it is the only argument this
# gate adds. `.Release.IsInstall` is the chart's SOLE branch on install-vs-
# upgrade (deployment-server.yaml requires `migration.designatedOperatorSecret`
# on a fresh install), and that bootstrap secret has nothing to do with whether
# a render dispatches. Without the flag the stock-defaults question — the one
# that went unanswered through the 2026-07-29 outage — could not be asked at
# all. Repeating the flag is harmless if a caller passes it too.
"$HELM" template "$RELEASE_NAME" "$CHART_DIR" --is-upgrade "$@" >"$WORK/rendered.yaml" ||
  die "helm template failed for chart $CHART_DIR"

# Reduce the render to the environment the kubelet would hand the djinn-server
# container: `envFrom` ConfigMap keys first, then explicit `env` entries, with
# later entries winning — the kubelet's own precedence, so a chart that emits a
# name twice is judged the way the cluster would resolve it rather than the way
# a reader might hope.
python3 - "$WORK/rendered.yaml" >"$WORK/env.nul" <<'PY'
import sys
from pathlib import Path

import yaml

PREFIX = 'DJINN_K8S_'
# Without these two the pairing that caused the outage is unobservable, so an
# absent name is a gate failure and not a silent fallback to a code default.
REQUIRED = ('DJINN_K8S_CGROUP_LAUNCHER_MODE', 'DJINN_K8S_TASK_RUN_CGROUP_WRITABLE_ENABLED')

rendered = Path(sys.argv[1]).read_text(encoding='utf-8')
documents = [doc for doc in yaml.safe_load_all(rendered) if isinstance(doc, dict)]


def fail(message):
    sys.stderr.write(f'render-gate: {message}\n')
    raise SystemExit(1)


def warn(message):
    sys.stderr.write(f'render-gate: warning: {message}\n')


config_maps = {
    doc.get('metadata', {}).get('name'): doc.get('data') or {}
    for doc in documents
    if doc.get('kind') == 'ConfigMap'
}

servers = [
    container
    for doc in documents
    if doc.get('kind') == 'Deployment'
    for container in (doc.get('spec', {}).get('template', {}).get('spec', {}).get('containers')
                      or [])
    if container.get('name') == 'djinn-server'
]
if len(servers) != 1:
    fail(f'expected exactly one rendered djinn-server container, found {len(servers)}')
container = servers[0]

environment = {}

for source in container.get('envFrom') or []:
    reference = source.get('configMapRef')
    if not reference:
        # Secret-sourced env cannot be evaluated from a render at all. Only
        # complain if it could plausibly carry the names we depend on.
        warn('ignoring a non-ConfigMap envFrom source; DJINN_K8S_* from it is invisible here')
        continue
    name = reference.get('name')
    if name not in config_maps:
        warn(f'envFrom ConfigMap {name!r} is not part of this render; its keys are invisible here')
        continue
    for key, value in config_maps[name].items():
        if key.startswith(PREFIX):
            environment[key] = str(value)

for entry in container.get('env') or []:
    name = entry.get('name', '')
    if not name.startswith(PREFIX):
        continue
    if 'value' not in entry:
        fail(f'{name} is rendered via valueFrom; a render gate cannot evaluate it')
    value = str(entry['value'])
    if name in environment and environment[name] != value:
        # The kubelet takes the last one. Say so loudly: a duplicate that
        # disagrees is how an `extraEnv` override silently re-arms a knob.
        warn(f'{name} rendered more than once with differing values '
             f'({environment[name]!r} then {value!r}); the kubelet applies the last, and so does '
             f'this gate')
    environment[name] = value

missing = [name for name in REQUIRED if name not in environment]
if missing:
    fail('the rendered djinn-server container does not set ' + ', '.join(missing))

out = sys.stdout
for name, value in sorted(environment.items()):
    out.write(f'{name}={value}\0')
PY

pairs=()
while IFS= read -r -d '' pair; do
  pairs+=("$pair")
done <"$WORK/env.nul"
[ "${#pairs[@]}" -gt 0 ] || die "no DJINN_K8S_* environment was extracted from the render"

# `env -i` is the point: the verdict must depend on the RENDER, never on the
# DJINN_K8S_* variables that happen to be exported in the operator's shell.
env -i "${pairs[@]}" "$BIN"
