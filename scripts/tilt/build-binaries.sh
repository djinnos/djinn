#!/usr/bin/env bash
# Compile djinn-server + djinn-agent-worker in one cargo invocation and
# stage the binaries under `.tilt/artifacts/` for the per-image wrapper
# scripts to pick up.
#
# Why one script for both:
#   The two binaries share six workspace crates (djinn-core, djinn-db,
#   djinn-graph, djinn-runtime, djinn-supervisor, djinn-workspace) plus
#   ~80 external deps unified by workspace-hack. Building them in a single
#   `cargo build -p djinn-server -p djinn-agent-worker` means shared deps
#   compile once per source change instead of twice. Target dir and cargo
#   registry are reused across both.
#
# Why host-side (in a cached purpose-built container) and not BuildKit:
#   BuildKit's --mount=type=cache,target=.../target was wedging such that
#   source edits reused a stale compiled binary — "builds" completed in 1s
#   with no new image layers. Named docker volumes survive across Tilt
#   invocations without that failure mode.
#
# Caching layers (all named docker volumes, survive `tilt down`):
#   djinn-cargo-registry  — downloaded .crate files + git deps
#   djinn-cargo-target    — incremental compilation results
#   djinn-sccache         — compilation-unit cache (wrapped rustc),
#                           keyed by (rustc version + source + flags).
#                           Earns its keep when the target volume is
#                           wiped (docker volume prune) — sccache
#                           rebuilds cheaply from its own cache.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="${REPO_ROOT:-$(cd "$SCRIPT_DIR/../.." && pwd)}"
REPO_ROOT="$(cd "$REPO_ROOT" && pwd)"
DEFAULT_BUILDER_IMAGE="djinn-binaries-builder:dev"
BUILDER_IMAGE="${BUILDER_IMAGE:-$DEFAULT_BUILDER_IMAGE}"
BUILDER_DOCKERFILE="$REPO_ROOT/server/docker/djinn-binaries-builder.Dockerfile"
CARGO_REGISTRY_VOLUME="${CARGO_REGISTRY_VOLUME:-djinn-cargo-registry}"
TARGET_VOLUME="${TARGET_VOLUME:-djinn-cargo-target}"
SCCACHE_VOLUME="${SCCACHE_VOLUME:-djinn-sccache}"
ARTIFACTS_DIR="${ARTIFACTS_DIR:-$REPO_ROOT/.tilt/artifacts}"
BUILD_FINGERPRINT="${BUILD_FINGERPRINT:-$ARTIFACTS_DIR/.binaries-inputs.fingerprint}"
BINARY_OUTPUT_FINGERPRINT="${BINARY_OUTPUT_FINGERPRINT:-$ARTIFACTS_DIR/.binaries-output.fingerprint}"
UI_OUTPUT_FINGERPRINT="${UI_OUTPUT_FINGERPRINT:-$ARTIFACTS_DIR/.ui-output.fingerprint}"
UI_DIST_DIR="${UI_DIST_DIR:-$REPO_ROOT/ui/dist}"
BUILD_LOCK_DIR="${TILT_BUILD_LOCK_DIR:-$REPO_ROOT/.tilt/build-transaction.lock}"
DOCKER_BIN="${DOCKER_BIN:-docker}"

# shellcheck source=scripts/tilt/input-fingerprint.sh
source "$SCRIPT_DIR/input-fingerprint.sh"

cd "$REPO_ROOT"
mkdir -p "$ARTIFACTS_DIR"
BUILDER_IID_FILE=""
cleanup() {
    if [[ -n "$BUILDER_IID_FILE" ]]; then
        rm -f "$BUILDER_IID_FILE"
    fi
    tilt_release_lock
}
tilt_acquire_lock "$BUILD_LOCK_DIR"
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# A fresh Tilt process otherwise runs this manual resource's initial build even
# when the staged release binaries already match every build input. Use a
# canonical content fingerprint instead of mtimes so edits, additions,
# deletions, and clock skew all invalidate correctly. The fingerprint is
# published only after both binaries are atomically staged below.
BUILD_INPUTS=(
    "$REPO_ROOT/server/src"
    "$REPO_ROOT/server/crates"
    "$REPO_ROOT/server/.cargo"
    "$REPO_ROOT/server/.sqlx"
    "$REPO_ROOT/server/Cargo.toml"
    "$REPO_ROOT/server/Cargo.lock"
    "$REPO_ROOT/server/rust-toolchain.toml"
    "$REPO_ROOT/server/build.rs"
    "$REPO_ROOT/server/docker/djinn-binaries-builder.Dockerfile"
    "$UI_DIST_DIR"
    "$SCRIPT_DIR/build-binaries.sh"
    "$SCRIPT_DIR/build-ui.sh"
    "$SCRIPT_DIR/input-fingerprint.sh"
)
INPUT_FINGERPRINT="$(tilt_input_fingerprint "$REPO_ROOT" "${BUILD_INPUTS[@]}")"
CURRENT_FINGERPRINT="$({
    printf 'inputs=%s\n' "$INPUT_FINGERPRINT"
    printf 'builder-image=%s\n' "$BUILDER_IMAGE"
} | git -C "$REPO_ROOT" hash-object --stdin)"

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

validate_volume_name() {
    local variable="$1"
    local value="$2"
    if [[ ! "$value" =~ ^[A-Za-z0-9][A-Za-z0-9_.-]+$ ]]; then
        echo "error: $variable is not a valid Docker volume name: $value" >&2
        exit 2
    fi
}

validate_volume_name CARGO_REGISTRY_VOLUME "$CARGO_REGISTRY_VOLUME"
validate_volume_name TARGET_VOLUME "$TARGET_VOLUME"
validate_volume_name SCCACHE_VOLUME "$SCCACHE_VOLUME"

if [[ -f "$UI_OUTPUT_FINGERPRINT" ]]; then
    if [[ ! -f "$UI_DIST_DIR/index.html" ]]; then
        echo "error: staged UI output is missing; rebuild djinn-ui-dist first" >&2
        exit 1
    fi
    CURRENT_UI_OUTPUT_FINGERPRINT="$(
        tilt_input_fingerprint "$UI_DIST_DIR" "$UI_DIST_DIR"
    )"
    if ! tilt_fingerprint_matches "$UI_OUTPUT_FINGERPRINT" "$CURRENT_UI_OUTPUT_FINGERPRINT"; then
        echo "error: staged UI output changed after its build; rebuild djinn-ui-dist first" >&2
        exit 1
    fi
fi

if [[ -x "$ARTIFACTS_DIR/djinn-server" \
    && -x "$ARTIFACTS_DIR/djinn-agent-worker" \
    && -x "$ARTIFACTS_DIR/djinn-cgroup-launcher" ]] \
    && tilt_fingerprint_matches "$BUILD_FINGERPRINT" "$CURRENT_FINGERPRINT"; then
    CURRENT_BINARY_OUTPUT_FINGERPRINT="$(
        tilt_input_fingerprint "$ARTIFACTS_DIR" \
            "$ARTIFACTS_DIR/djinn-server" \
            "$ARTIFACTS_DIR/djinn-agent-worker" \
            "$ARTIFACTS_DIR/djinn-cgroup-launcher"
    )"
    if tilt_fingerprint_matches \
        "$BINARY_OUTPUT_FINGERPRINT" "$CURRENT_BINARY_OUTPUT_FINGERPRINT"; then
        echo "==> build inputs and outputs unchanged; reusing staged djinn binaries"
        exit 0
    fi
fi

BUILDER_IMAGE_ID=""
if [[ "$BUILDER_IMAGE" == "$DEFAULT_BUILDER_IMAGE" ]]; then
    echo "==> ensuring cached binary-builder image $BUILDER_IMAGE"
    BUILDER_IID_FILE="$(mktemp "${TMPDIR:-/tmp}/djinn-builder-iid.XXXXXX")"
    "$DOCKER_BIN" build \
        -f "$BUILDER_DOCKERFILE" \
        -t "$BUILDER_IMAGE" \
        --iidfile "$BUILDER_IID_FILE" \
        "$(dirname "$BUILDER_DOCKERFILE")"
    BUILDER_IMAGE_ID="$(tr -d '\r\n' < "$BUILDER_IID_FILE")"
    rm -f "$BUILDER_IID_FILE"
    BUILDER_IID_FILE=""
else
    # Caller-supplied images remain supported. They own the toolchain contract
    # (sccache, mold, clang, pkg-config, OpenSSL, CMake, and protoc).
    "$DOCKER_BIN" image inspect "$BUILDER_IMAGE" >/dev/null 2>&1 \
        || "$DOCKER_BIN" pull "$BUILDER_IMAGE"
    BUILDER_IMAGE_ID="$("$DOCKER_BIN" image inspect --format '{{.Id}}' "$BUILDER_IMAGE")"
fi
if [[ -z "$BUILDER_IMAGE_ID" ]]; then
    echo "error: Docker did not resolve builder image $BUILDER_IMAGE" >&2
    exit 1
fi

echo "==> cargo build (djinn-server + djinn-agent-worker) in $BUILDER_IMAGE_ID"
"$DOCKER_BIN" run --rm \
    -v "$REPO_ROOT:/app" \
    -v "${CARGO_REGISTRY_VOLUME}:/usr/local/cargo/registry" \
    -v "${TARGET_VOLUME}:/app/server/target" \
    -v "${SCCACHE_VOLUME}:/root/.cache/sccache" \
    -w /app/server \
    -e SQLX_OFFLINE=true \
    -e RUSTC_WRAPPER=sccache \
    -e SCCACHE_DIR=/root/.cache/sccache \
    -e SCCACHE_CACHE_SIZE=10G \
    -e CARGO_BUILD_RUSTFLAGS=-Clink-arg=-fuse-ld=mold \
    "$BUILDER_IMAGE_ID" \
    sh -c '
        set -eux
        command -v sccache
        command -v mold
        command -v clang
        command -v protoc
        sccache --start-server || true
        cargo build --release --locked \
            --features qdrant \
            -p djinn-server \
            -p djinn-agent-worker \
            -p djinn-cgroup-launcher
        sccache --show-stats || true
    '

echo "==> extracting binaries into $ARTIFACTS_DIR"
# A second short `docker run` lets us read the release binaries out of the
# target volume into a regular host dir — named volumes aren't directly
# addressable from `docker cp` without a container, and copying through
# the build container would force it to outlive the cargo invocation.
"$DOCKER_BIN" run --rm \
    -v "${TARGET_VOLUME}:/target" \
    -v "${ARTIFACTS_DIR}:/out" \
    "$BUILDER_IMAGE_ID" \
    sh -c '
        set -eux
        server_tmp="/out/.djinn-server.$$"
        worker_tmp="/out/.djinn-agent-worker.$$"
        launcher_tmp="/out/.djinn-cgroup-launcher.$$"
        trap '\''rm -f "$server_tmp" "$worker_tmp" "$launcher_tmp"'\'' EXIT
        cp /target/release/djinn-server "$server_tmp"
        cp /target/release/djinn-agent-worker "$worker_tmp"
        cp /target/release/djinn-cgroup-launcher "$launcher_tmp"
        # Strip all binaries to trim image size. `strip` is in binutils
        # which ships in the rust:*-slim base.
        strip "$server_tmp"
        strip "$worker_tmp"
        strip "$launcher_tmp"
        chmod +x "$server_tmp" "$worker_tmp" "$launcher_tmp"
        # Publish each artifact with one rename. Direct copy + strip generated
        # several file-watch events, reparsed the Tiltfile repeatedly, and
        # could briefly hash an unstripped/partial worker binary.
        mv -f "$server_tmp" /out/djinn-server
        mv -f "$worker_tmp" /out/djinn-agent-worker
        mv -f "$launcher_tmp" /out/djinn-cgroup-launcher
        trap - EXIT
    '

if [[ ! -x "$ARTIFACTS_DIR/djinn-server" \
    || ! -x "$ARTIFACTS_DIR/djinn-agent-worker" \
    || ! -x "$ARTIFACTS_DIR/djinn-cgroup-launcher" ]]; then
    echo "error: binary build completed without all executable artifacts" >&2
    exit 1
fi
CURRENT_BINARY_OUTPUT_FINGERPRINT="$(
    tilt_input_fingerprint "$ARTIFACTS_DIR" \
        "$ARTIFACTS_DIR/djinn-server" \
        "$ARTIFACTS_DIR/djinn-agent-worker" \
        "$ARTIFACTS_DIR/djinn-cgroup-launcher"
)"
tilt_store_fingerprint "$BINARY_OUTPUT_FINGERPRINT" "$CURRENT_BINARY_OUTPUT_FINGERPRINT"
tilt_store_fingerprint "$BUILD_FINGERPRINT" "$CURRENT_FINGERPRINT"

echo "==> done: $ARTIFACTS_DIR/{djinn-server,djinn-agent-worker,djinn-cgroup-launcher}"
