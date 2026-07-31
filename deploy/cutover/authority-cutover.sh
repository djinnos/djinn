#!/usr/bin/env bash
# THE OPERATOR ENTRY POINT for proposal 3i92's launcher-authority cutover.
#
# WHY THIS EXISTS
# ---------------
# `ResizeRollout` (server/src/task_run_resize_rollout.rs) has encoded the whole
# fenced transition — the ordering, both drain checks, the compare-and-swap, the
# reverse that can refuse itself — since task `eeky`. Until this script and
# `server/src/bin/authority_cutover.rs` landed, `ResizeRollout::production` had
# ZERO callers in any binary: the staged activation was a library with tests
# that nobody could run. This is what an operator types.
#
# WHY IT DELEGATES TO THE PREFLIGHT WRAPPER
# -----------------------------------------
# The flip is gated on `djinn_k8s::cutover_preflight::run`, whose Rust half
# re-renders the task-run Job from `DJINN_K8S_*`. Those variables must come from
# the RENDERED djinn-server container, not from the operator's shell — otherwise
# the credential-boundary and launcher-ceiling classes are judged against
# whatever happens to be exported. `deploy/preflight/cutover-preflight.sh`
# already renders the chart, extracts that environment and re-execs under
# `env -i`. So this script does not repeat any of it: it points
# CUTOVER_PREFLIGHT_BIN at the `authority-cutover` binary and hands over.
#
# The consequence is the point: the gate the deploy lane runs and the gate the
# flip runs are the same gate, over the same render, under the same environment.
#
# USAGE
#   DJINN_CUTOVER_DIRECTION=activate \
#   DJINN_CUTOVER_PLAN=/path/plan.json \
#   DJINN_CUTOVER_AUTHORITY_MODE=resize-v2 \
#   DJINN_DATABASE_URL=postgres://... \
#     deploy/cutover/authority-cutover.sh deploy/helm/djinn --values prod-values.yaml
#
#   # and the reverse
#   DJINN_CUTOVER_DIRECTION=rollback DJINN_CUTOVER_AUTHORITY_MODE=leaf-v1 ...
#
# Exit status: 0 the mode flipped and admission resumed; 1 blocked (the mode did
# NOT move, and admission is left paused whenever the block came at or after the
# pause step — the binary says which); 2 unevaluable, nothing attempted.
#
# Environment:
#   DJINN_CUTOVER_DIRECTION        activate | rollback (required, never defaulted)
#   DJINN_CUTOVER_PLAN             retained set, probe, expected epoch, registry URL
#   DJINN_CUTOVER_AUTHORITY_MODE   must name the mode the direction targets
#   DJINN_DATABASE_URL             required — the fence, catalog and singleton live there
#   DJINN_CUTOVER_OBSERVATIONS     JSON bundle of catalog images / live births
#   DJINN_CUTOVER_PAUSED_BY        recorded on the pause row (default: authority-cutover)
#   AUTHORITY_CUTOVER_BIN          path to the binary (default: build it)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
PREFLIGHT="$REPO_DIR/deploy/preflight/cutover-preflight.sh"

die() {
  printf 'authority-cutover: %s\n' "$1" >&2
  exit 2
}

usage() {
  cat <<'EOF'
usage: authority-cutover.sh <chart-dir> [helm template args...]

Runs proposal 3i92's fenced launcher-authority cutover, or its reverse, through
ResizeRollout::production. The flip is gated on the REAL deploy preflight over
the same render this script hands the binary.

  DJINN_CUTOVER_DIRECTION=activate DJINN_CUTOVER_PLAN=plan.json \
  DJINN_CUTOVER_AUTHORITY_MODE=resize-v2 DJINN_DATABASE_URL=postgres://... \
    deploy/cutover/authority-cutover.sh deploy/helm/djinn

Exit 0 flipped, 1 blocked (mode unchanged), 2 unevaluable.
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

[ -f "$PREFLIGHT" ] || die "preflight wrapper not found at $PREFLIGHT"

# Refused HERE as well as in the binary. The binary's refusal is the enforcement
# — it is what a caller bypassing this script still hits — but an operator who
# forgot the direction should not first watch a chart render.
[ -n "${DJINN_CUTOVER_DIRECTION:-}" ] || die "DJINN_CUTOVER_DIRECTION is not set (activate|rollback)"
[ -n "${DJINN_CUTOVER_PLAN:-}" ] || die "DJINN_CUTOVER_PLAN is not set"
[ -f "${DJINN_CUTOVER_PLAN}" ] || die "DJINN_CUTOVER_PLAN does not exist: $DJINN_CUTOVER_PLAN"
[ -n "${DJINN_DATABASE_URL:-}" ] || die "DJINN_DATABASE_URL is not set"

BIN="${AUTHORITY_CUTOVER_BIN:-}"
if [ -z "$BIN" ]; then
  BIN="${CARGO_TARGET_DIR:-$REPO_DIR/server/target}/debug/authority-cutover"
  if [ ! -x "$BIN" ]; then
    command -v cargo >/dev/null 2>&1 ||
      die "authority-cutover binary missing ($BIN) and cargo is unavailable to build it"
    # Built from `server/` for the reason cutover-preflight.sh documents: CI sets
    # a RELATIVE CARGO_BUILD_BUILD_DIR that cargo resolves against the process
    # CWD, so building from the repo root rebuilds the whole chain off a cold
    # cache.
    (cd "$REPO_DIR/server" && cargo build -p djinn-server --bin authority-cutover) >&2
  fi
fi
[ -x "$BIN" ] || die "authority-cutover binary is not executable: $BIN"

# Hand over. From here the preflight wrapper owns the render, the DJINN_K8S_*
# extraction and the `env -i` re-exec; the binary it runs is ours.
CUTOVER_PREFLIGHT_BIN="$BIN" exec "$PREFLIGHT" "$@"
