#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="${REPO_ROOT:-$(cd "$SCRIPT_DIR/../.." && pwd)}"
REPO_ROOT="$(cd "$REPO_ROOT" && pwd)"
UI_DIR="$REPO_ROOT/ui"
ARTIFACTS_DIR="${ARTIFACTS_DIR:-$REPO_ROOT/.tilt/artifacts}"
BUILD_FINGERPRINT="${UI_BUILD_FINGERPRINT:-$ARTIFACTS_DIR/.ui-inputs.fingerprint}"
OUTPUT_FINGERPRINT="${UI_OUTPUT_FINGERPRINT:-$ARTIFACTS_DIR/.ui-output.fingerprint}"
PNPM_BIN="${PNPM_BIN:-pnpm}"
BUILD_LOCK_DIR="${TILT_BUILD_LOCK_DIR:-$REPO_ROOT/.tilt/build-transaction.lock}"

# shellcheck source=scripts/tilt/input-fingerprint.sh
source "$SCRIPT_DIR/input-fingerprint.sh"

mkdir -p "$ARTIFACTS_DIR"
tilt_acquire_lock "$BUILD_LOCK_DIR"
trap 'tilt_release_lock' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

BUILD_INPUTS=(
    "$UI_DIR/src"
    "$UI_DIR/public"
    "$UI_DIR/index.html"
    "$UI_DIR/package.json"
    "$UI_DIR/pnpm-lock.yaml"
    "$UI_DIR/pnpm-workspace.yaml"
    "$UI_DIR/.npmrc"
    "$UI_DIR/tsconfig.app.json"
    "$UI_DIR/tsconfig.json"
    "$UI_DIR/tsconfig.node.json"
    "$UI_DIR/vite.config.ts"
    "$SCRIPT_DIR/build-ui.sh"
    "$SCRIPT_DIR/input-fingerprint.sh"
)
CURRENT_FINGERPRINT="$(tilt_input_fingerprint "$REPO_ROOT" "${BUILD_INPUTS[@]}")"

case "${1:-}" in
    "") ;;
    --print-input-fingerprint)
        printf '%s\n' "$CURRENT_FINGERPRINT"
        exit 0
        ;;
    *)
        echo "usage: $0 [--print-input-fingerprint]" >&2
        exit 2
        ;;
esac

if [[ -f "$UI_DIR/dist/index.html" ]] \
    && tilt_fingerprint_matches "$BUILD_FINGERPRINT" "$CURRENT_FINGERPRINT"; then
    CURRENT_OUTPUT_FINGERPRINT="$(
        tilt_input_fingerprint "$UI_DIR/dist" "$UI_DIR/dist"
    )"
    if tilt_fingerprint_matches "$OUTPUT_FINGERPRINT" "$CURRENT_OUTPUT_FINGERPRINT"; then
        echo "==> UI inputs and outputs unchanged; reusing ui/dist"
        exit 0
    fi
fi

cd "$UI_DIR"
"$PNPM_BIN" install --frozen-lockfile
"$PNPM_BIN" build
if [[ ! -f "$UI_DIR/dist/index.html" ]]; then
    echo "error: UI build succeeded without producing $UI_DIR/dist/index.html" >&2
    exit 1
fi
CURRENT_OUTPUT_FINGERPRINT="$(tilt_input_fingerprint "$UI_DIR/dist" "$UI_DIR/dist")"
tilt_store_fingerprint "$OUTPUT_FINGERPRINT" "$CURRENT_OUTPUT_FINGERPRINT"
tilt_store_fingerprint "$BUILD_FINGERPRINT" "$CURRENT_FINGERPRINT"
echo "==> done: $UI_DIR/dist"
