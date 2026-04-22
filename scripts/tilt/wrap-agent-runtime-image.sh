#!/usr/bin/env bash
# Wrap the host-built `djinn-agent-worker` binary (staged by
# build-binaries.sh) on top of the `djinn-agent-runtime-base` image and
# push the result to the kind-local registry.
#
# The base image carries all the heavy, slow-churning toolchain bits (LSPs,
# rustup, sccache, mold). This script produces a thin top layer that only
# copies the worker binary in — so edits to the worker re-tag the image in
# ~seconds rather than re-fetching Node/rust-analyzer/etc.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
IMAGE_TAG="${IMAGE_TAG:-localhost:5001/djinn-agent-runtime:dev}"
BASE_IMAGE="${BASE_IMAGE:-djinn-agent-runtime-base:dev}"
ARTIFACTS_DIR="${ARTIFACTS_DIR:-$REPO_ROOT/.tilt/artifacts}"
BINARY="$ARTIFACTS_DIR/djinn-agent-worker"
DOCKERFILE="$REPO_ROOT/server/docker/djinn-agent-runtime.Dockerfile"

if [[ ! -x "$BINARY" ]]; then
    echo "error: $BINARY not found or not executable — run build-binaries.sh first" >&2
    exit 1
fi

if ! docker image inspect "$BASE_IMAGE" >/dev/null 2>&1; then
    echo "error: base image $BASE_IMAGE not present — run build-agent-runtime-base.sh first" >&2
    exit 1
fi

BUILD_CTX="$(mktemp -d)"
trap 'rm -rf "$BUILD_CTX"' EXIT

cp "$BINARY" "$BUILD_CTX/djinn-agent-worker"
cp "$DOCKERFILE" "$BUILD_CTX/Dockerfile"

echo "==> building $IMAGE_TAG (FROM $BASE_IMAGE)"
docker build \
    --build-arg "BASE_IMAGE=$BASE_IMAGE" \
    -t "$IMAGE_TAG" \
    "$BUILD_CTX"

# SKIP_PUSH=1 for standalone/offline callers (build-runtime-image.sh).
# Tilt always pushes so the kind cluster can pull from localhost:5001.
if [[ "${SKIP_PUSH:-0}" != "1" ]]; then
    docker push "$IMAGE_TAG"
fi
echo "==> done: $IMAGE_TAG"
