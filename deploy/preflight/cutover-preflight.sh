#!/usr/bin/env bash
# Deploy-time preflight for proposal 3i92's CPU-quota cutover.
#
# WHY THIS EXISTS
# ---------------
# Flipping a deployment from `leaf-v1` to `resize-v2` moves ownership of every
# task-run's CPU quota from the launcher's per-invocation cgroup leaf to
# Kubernetes in-place Pod resize. Six things must be true at the moment of the
# flip, and every one of them fails SILENTLY if it is not:
#
#   1. `pods/resize` is granted to the controller Role as the exact triple
#      (apiGroups [""], resources ["pods/resize"], verbs ["patch"]). Absent, every
#      lift is a 403 and every brokered build runs at the unleased floor.
#   2. The launcher native sidecar carries a `resize-v2` CPU ceiling equal to its
#      own lease — and, under `leaf-v1`, carries NONE, because a limit there is
#      an ancestor clamp over every invocation leaf (task 7deu measured a leaf
#      set to 4 cores burning 0.25).
#   3. Birth downsize is confirmable from
#      `status.initContainerStatuses[name=cgroup-launcher]` and nowhere else.
#   4. Every dispatch-eligible catalog image agrees with the authority mode.
#   5. The mode-flip drain fence is empty AND readable.
#   6. Task-run Pods hold no apiserver credential.
#
# Like `render-gate.sh`, this script is deliberately thin. It renders the chart
# the way a deploy would, extracts the `DJINN_K8S_*` environment out of the
# RENDERED djinn-server container, and runs the crate's OWN validator
# (`djinn_k8s::cutover_preflight::run`) over it. There is no rule here.
#
# WHAT IT IS NOT
# --------------
# It is not a cluster check for the Kubernetes version, node handlers or cgroup
# delegation, and it is not a substitute for `render-gate.sh` — that gate asks
# whether the render dispatches AT ALL, which is a precondition for this one.
#
# USAGE
#   deploy/preflight/cutover-preflight.sh <chart-dir> [helm template args...]
#
#   # stock chart defaults, asking about the leaf-v1 status quo
#   deploy/preflight/cutover-preflight.sh deploy/helm/djinn
#
#   # the real question: is this deployment ready to become resize-v2?
#   DJINN_CUTOVER_AUTHORITY_MODE=resize-v2 DJINN_DATABASE_URL=postgres://... \
#     deploy/preflight/cutover-preflight.sh deploy/helm/djinn --values prod-values.yaml
#
# Exit status: 0 the cutover may proceed; 1 at least one defect (each printed
# with its class); 2 the preflight could not be evaluated at all.
#
# Environment:
#   DJINN_CUTOVER_AUTHORITY_MODE   leaf-v1 | resize-v2 (default: leaf-v1)
#   DJINN_DATABASE_URL             read the drain fence for real; without it the
#                                  fence is UNOBSERVABLE, which is a defect
#   DJINN_CUTOVER_OBSERVATIONS     JSON bundle of catalog images / live births
#   HELM                           helm executable (default: helm)
#   CUTOVER_PREFLIGHT_BIN          path to the binary (default: build it)
#   CUTOVER_PREFLIGHT_RENDER       use THIS rendered manifest instead of running
#                                  helm (the contract suite's mutated fixtures)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
HELM="${HELM:-helm}"
RELEASE_NAME="djinn-cutover-preflight"

die() {
  printf 'cutover-preflight: %s\n' "$1" >&2
  exit 2
}

usage() {
  cat <<'EOF'
usage: cutover-preflight.sh <chart-dir> [helm template args...]

Renders the chart, extracts the DJINN_K8S_* environment of the rendered
djinn-server container, and runs djinn_k8s::cutover_preflight::run over the
render plus the Rust-rendered task-run Job.

  cutover-preflight.sh deploy/helm/djinn
  DJINN_CUTOVER_AUTHORITY_MODE=resize-v2 cutover-preflight.sh deploy/helm/djinn

Exit 0 the cutover may proceed, 1 blocked, 2 unevaluable.
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

BIN="${CUTOVER_PREFLIGHT_BIN:-}"
if [ -z "$BIN" ]; then
  BIN="${CARGO_TARGET_DIR:-$REPO_DIR/server/target}/debug/cutover-preflight"
  if [ ! -x "$BIN" ]; then
    command -v cargo >/dev/null 2>&1 ||
      die "cutover-preflight binary missing ($BIN) and cargo is unavailable to build it"
    # Built from `server/`, not via `--manifest-path` from wherever the caller
    # happens to stand: CI sets a RELATIVE `CARGO_BUILD_BUILD_DIR=target`, and
    # cargo resolves that against the PROCESS CWD — so building from the repo
    # root sends every intermediate to `<repo>/target` and rebuilds the whole
    # djinn-k8s chain off an otherwise-warm `server/target` cache.
    (cd "$REPO_DIR/server" && cargo build -p djinn-k8s --bin cutover-preflight) >&2
  fi
fi
[ -x "$BIN" ] || die "cutover-preflight binary is not executable: $BIN"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# The render is the subject under test. `--is-upgrade` matches render-gate.sh:
# `.Release.IsInstall` is the chart's only install-vs-upgrade branch and gates a
# bootstrap secret irrelevant to the cutover.
#
# CUTOVER_PREFLIGHT_RENDER lets the contract suite supply a render it mutated by
# exactly one field. It is never a checked-in known-good file: the suite derives
# every fixture from a live render at test time.
if [ -n "${CUTOVER_PREFLIGHT_RENDER:-}" ]; then
  [ -f "$CUTOVER_PREFLIGHT_RENDER" ] ||
    die "CUTOVER_PREFLIGHT_RENDER does not exist: $CUTOVER_PREFLIGHT_RENDER"
  cp "$CUTOVER_PREFLIGHT_RENDER" "$WORK/rendered.yaml"
else
  command -v "$HELM" >/dev/null 2>&1 || die "required tool '$HELM' is not installed"
  "$HELM" template "$RELEASE_NAME" "$CHART_DIR" --is-upgrade "$@" >"$WORK/rendered.yaml" ||
    die "helm template failed for chart $CHART_DIR"
fi

# Two products from one render, so the manifest the validator judges and the
# environment it is judged under can never come from different bytes:
#
#   * rendered.json — every document, for the RBAC / ServiceAccount / RoleBinding
#     surface. JSON because the validator crate carries serde_yaml as a
#     dev-dependency only.
#   * env.nul       — the DJINN_K8S_* the kubelet would hand djinn-server, with
#     `envFrom` ConfigMap keys first and explicit `env` last, which is the
#     kubelet's own precedence.
python3 - "$WORK/rendered.yaml" "$WORK/rendered.json" >"$WORK/env.nul" <<'PY'
import json
import sys
from pathlib import Path

import yaml

PREFIX = 'DJINN_K8S_'
REQUIRED = ('DJINN_K8S_CGROUP_LAUNCHER_MODE', 'DJINN_K8S_TASK_RUN_CGROUP_WRITABLE_ENABLED')

rendered = Path(sys.argv[1]).read_text(encoding='utf-8')
documents = [doc for doc in yaml.safe_load_all(rendered) if isinstance(doc, dict)]


def fail(message):
    sys.stderr.write(f'cutover-preflight: {message}\n')
    raise SystemExit(2)


if not documents:
    fail('the render contains no documents; an empty render passes every '
         'render-derived check vacuously')

Path(sys.argv[2]).write_text(json.dumps(documents), encoding='utf-8')

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
        continue
    for key, value in (config_maps.get(reference.get('name')) or {}).items():
        if key.startswith(PREFIX):
            environment[key] = str(value)

for entry in container.get('env') or []:
    name = entry.get('name', '')
    if not name.startswith(PREFIX):
        continue
    if 'value' not in entry:
        fail(f'{name} is rendered via valueFrom; a render gate cannot evaluate it')
    environment[name] = str(entry['value'])

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
# DJINN_K8S_* variables exported in the operator's shell. The names below are
# forwarded explicitly because they describe the validation target, not the
# render — which authority mode is being checked, where the durable fence lives,
# and where the cluster/registry observations are.
forward=()
for name in DJINN_CUTOVER_AUTHORITY_MODE DJINN_CUTOVER_OBSERVATIONS DJINN_DATABASE_URL \
  DJINN_LEGACY_LAUNCHER_DIGEST_INVENTORY DJINN_LEGACY_LAUNCHER_DIGEST_INVENTORY_PUBLIC_KEY \
  DJINN_LEGACY_LAUNCHER_DIGEST_INVENTORY_SIGNATURE; do
  if [ -n "${!name:-}" ]; then
    forward+=("$name=${!name}")
  fi
done

env -i "${pairs[@]}" "${forward[@]}" "$BIN" "$WORK/rendered.json"
