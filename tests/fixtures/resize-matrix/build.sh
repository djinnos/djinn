#!/usr/bin/env bash
# Build the three REAL image classes of the mixed-version matrix (omp4).
#
#   build.sh [kind-cluster-name]
#
# Produces three independently built images and writes their immutable content
# addresses to server/target/resize-matrix/images.json, which
# server/tests/task_run_resize_mixed_version.rs reads. Nothing here is a config
# variant of anything else:
#
#   djinn-resize-matrix-legacy    pre-protocol launcher, NO declaration
#   djinn-resize-matrix-leaf-v1   current launcher, baked `leaf-v1`
#   djinn-resize-matrix-resize-v2 current launcher, baked `resize-v2`
#
# WHY THE LEGACY LAUNCHER IS COMPILED AND NOT CONFIGURED
#
# The cheap way to produce a "legacy" image is to take the modern image and
# leave DJINN_LAUNCHER_AUTHORITY_PROTOCOL unset. That is a modern launcher that
# could handshake and chose not to, and it would pass every behavioural cell in
# the matrix while proving nothing about compatibility with launchers that
# genuinely predate the protocol. So the legacy launcher is compiled from
# DJINN_RESIZE_MATRIX_PREPROTOCOL_COMMIT in a detached git worktree, and the
# matrix scans the emitted binary for both wire strings.
#
# `kind load docker-image` rather than a registry push, for the reason the
# fbiy-C1 probe image gives: a pull failure surfaces as "the Pod never ran",
# which is indistinguishable from the launcher-readiness failure under test.
#
# Exit codes:
#   2  usage / not the djinn workspace
#   3  a pin is malformed (a tag where a digest belongs)
#   1  anything else
set -euo pipefail

CLUSTER=${1:-}

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# resize-matrix -> fixtures -> tests -> repo
REPO_ROOT="$(cd "$HERE/../../.." && pwd)"
SERVER_DIR="$REPO_ROOT/server"
OUT_DIR="$SERVER_DIR/target/resize-matrix"

fail() {
    local code=$1
    shift
    printf 'FAIL: %s\n' "$*" >&2
    exit "$code"
}

[ -d "$SERVER_DIR/crates/djinn-cgroup-launcher" ] \
    || fail 2 "$SERVER_DIR is not the djinn server workspace"

# --- pins -------------------------------------------------------------------
# shellcheck disable=SC1091
. "$HERE/pins.env"

: "${DJINN_RESIZE_MATRIX_PREPROTOCOL_COMMIT:?pins.env must set the pre-protocol commit}"
: "${DJINN_RESIZE_MATRIX_BASE_DIGEST:?pins.env must set the base image digest}"
: "${DJINN_RESIZE_MATRIX_BASE_REPOSITORY:?pins.env must set the base image repository}"

# Guard: a pin that is a tag is not a pin. Enforced here as well as in the Rust
# suite, because this script is also run by hand and by the workflow, and a
# mutable base would produce a "legacy" image nobody inspected.
[[ "$DJINN_RESIZE_MATRIX_BASE_DIGEST" =~ ^sha256:[0-9a-f]{64}$ ]] \
    || fail 3 "base pin '$DJINN_RESIZE_MATRIX_BASE_DIGEST' is not an immutable sha256: digest"
[[ "$DJINN_RESIZE_MATRIX_PREPROTOCOL_COMMIT" =~ ^[0-9a-f]{40}$ ]] \
    || fail 3 "pre-protocol pin '$DJINN_RESIZE_MATRIX_PREPROTOCOL_COMMIT' is not a full commit sha"

LEGACY_IMAGE="djinn-resize-matrix-legacy:omp4"
LEAF_IMAGE="djinn-resize-matrix-leaf-v1:omp4"
RESIZE_IMAGE="djinn-resize-matrix-resize-v2:omp4"

# --- the current binaries ---------------------------------------------------
printf '>>> building the current djinn-cgroup-launcher and governor_probe (release)\n'
(
    cd "$SERVER_DIR"
    cargo build --release -p djinn-cgroup-launcher \
        --bin djinn-cgroup-launcher --example governor_probe
)
TARGET="$SERVER_DIR/target/release"

# --- the pre-protocol launcher ----------------------------------------------
LEGACY_CACHE="$OUT_DIR/legacy/$DJINN_RESIZE_MATRIX_PREPROTOCOL_COMMIT"
LEGACY_BIN="$LEGACY_CACHE/djinn-cgroup-launcher"
LEGACY_PROBE="$LEGACY_CACHE/legacy_probe"
if [ ! -x "$LEGACY_BIN" ] || [ ! -x "$LEGACY_PROBE" ]; then
    printf '>>> compiling the pre-protocol launcher and worker probe at %s\n' \
        "$DJINN_RESIZE_MATRIX_PREPROTOCOL_COMMIT"
    WORKTREE="$OUT_DIR/preprotocol-worktree"
    rm -rf "$WORKTREE"
    mkdir -p "$OUT_DIR"
    git -C "$REPO_ROOT" worktree prune
    git -C "$REPO_ROOT" worktree add --detach "$WORKTREE" \
        "$DJINN_RESIZE_MATRIX_PREPROTOCOL_COMMIT" >/dev/null
    # The worker half must ALSO be pre-protocol. The renderer puts the sidecar
    # and the worker on one image tag, so a legacy image is a legacy launcher
    # AND a legacy worker; and the current worker cannot even complete the
    # handshake against a pre-protocol launcher, because the READY payload grew
    # a protocol byte. Copied into the THROWAWAY worktree so it links against
    # the pre-protocol crate; nothing in the repository's own tree is touched.
    mkdir -p "$WORKTREE/server/crates/djinn-cgroup-launcher/examples"
    cp "$HERE/legacy_probe.rs" \
        "$WORKTREE/server/crates/djinn-cgroup-launcher/examples/legacy_probe.rs"
    (
        cd "$WORKTREE/server"
        CARGO_TARGET_DIR="$OUT_DIR/preprotocol-target" \
            cargo build --release -p djinn-cgroup-launcher \
            --bin djinn-cgroup-launcher --example legacy_probe
    )
    mkdir -p "$LEGACY_CACHE"
    cp "$OUT_DIR/preprotocol-target/release/djinn-cgroup-launcher" "$LEGACY_BIN"
    cp "$OUT_DIR/preprotocol-target/release/examples/legacy_probe" "$LEGACY_PROBE"
    git -C "$REPO_ROOT" worktree remove --force "$WORKTREE"
else
    printf '>>> reusing the cached pre-protocol binaries at %s\n' "$LEGACY_CACHE"
fi

# A local sanity check, duplicated in Rust. Cheap, and it fails at build time
# rather than three minutes into a live cell.
if grep -aq 'leaf-v1' "$LEGACY_BIN" || grep -aq 'resize-v2' "$LEGACY_BIN"; then
    fail 3 "the pre-protocol launcher at $LEGACY_BIN carries an authority wire string; \
the pin no longer names a pre-protocol revision"
fi

# --- the three images -------------------------------------------------------
CONTEXT="$(mktemp -d)"
trap 'rm -rf "$CONTEXT"' EXIT

cp "$TARGET/djinn-cgroup-launcher" "$CONTEXT/djinn-cgroup-launcher"
cp "$TARGET/examples/governor_probe" "$CONTEXT/governor_probe"
cp "$LEGACY_BIN" "$CONTEXT/djinn-cgroup-launcher-preprotocol"
cp "$LEGACY_PROBE" "$CONTEXT/legacy_probe"

build_one() {
    local dockerfile=$1 image=$2
    printf '>>> docker build %s (%s)\n' "$image" "$dockerfile"
    cp "$HERE/$dockerfile" "$CONTEXT/$dockerfile"
    docker build --quiet \
        --file "$CONTEXT/$dockerfile" \
        --build-arg "DJINN_RESIZE_MATRIX_BASE_REPOSITORY=$DJINN_RESIZE_MATRIX_BASE_REPOSITORY" \
        --build-arg "DJINN_RESIZE_MATRIX_BASE_DIGEST=$DJINN_RESIZE_MATRIX_BASE_DIGEST" \
        --tag "$image" "$CONTEXT" >/dev/null
}

build_one Dockerfile.legacy "$LEGACY_IMAGE"
build_one Dockerfile.leaf-v1 "$LEAF_IMAGE"
build_one Dockerfile.resize-v2 "$RESIZE_IMAGE"

digest_of() {
    docker image inspect "$1" --format '{{.Id}}'
}

LEGACY_DIGEST="$(digest_of "$LEGACY_IMAGE")"
LEAF_DIGEST="$(digest_of "$LEAF_IMAGE")"
RESIZE_DIGEST="$(digest_of "$RESIZE_IMAGE")"

# Three independently built images must be three distinct artifacts. If two of
# these collide the "not a config variant of one image" requirement has been
# quietly violated by a Dockerfile edit.
if [ "$LEGACY_DIGEST" = "$LEAF_DIGEST" ] \
    || [ "$LEGACY_DIGEST" = "$RESIZE_DIGEST" ] \
    || [ "$LEAF_DIGEST" = "$RESIZE_DIGEST" ]; then
    fail 3 "two image classes resolved to the same digest; they are config variants, not classes"
fi

if [ -n "$CLUSTER" ]; then
    for image in "$LEGACY_IMAGE" "$LEAF_IMAGE" "$RESIZE_IMAGE"; do
        printf '>>> kind load docker-image %s --name %s\n' "$image" "$CLUSTER"
        kind load docker-image "$image" --name "$CLUSTER"
    done
fi

mkdir -p "$OUT_DIR"
MANIFEST="$OUT_DIR/images.json"
cat >"$MANIFEST" <<EOF
{
  "preprotocol_commit": "$DJINN_RESIZE_MATRIX_PREPROTOCOL_COMMIT",
  "base_digest": "$DJINN_RESIZE_MATRIX_BASE_DIGEST",
  "classes": {
    "legacy":    { "tag": "$LEGACY_IMAGE", "digest": "$LEGACY_DIGEST" },
    "leaf-v1":   { "tag": "$LEAF_IMAGE",   "digest": "$LEAF_DIGEST" },
    "resize-v2": { "tag": "$RESIZE_IMAGE", "digest": "$RESIZE_DIGEST" }
  }
}
EOF

printf 'PASS: three image classes built%s\n' "${CLUSTER:+ and loaded onto $CLUSTER}"
printf '  manifest: %s\n' "$MANIFEST"
