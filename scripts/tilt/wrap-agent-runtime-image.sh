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
BASE_BUILD_SCRIPT="${BASE_BUILD_SCRIPT:-$REPO_ROOT/scripts/tilt/build-agent-runtime-base.sh}"
ARTIFACTS_DIR="${ARTIFACTS_DIR:-$REPO_ROOT/.tilt/artifacts}"
BINARY="$ARTIFACTS_DIR/djinn-agent-worker"
LAUNCHER_BINARY="$ARTIFACTS_DIR/djinn-cgroup-launcher"
DOCKERFILE="$REPO_ROOT/server/docker/djinn-agent-runtime.Dockerfile"

if [[ ! -x "$BINARY" ]]; then
    echo "error: $BINARY not found or not executable — run build-binaries.sh first" >&2
    exit 1
fi
if [[ ! -x "$LAUNCHER_BINARY" ]]; then
    echo "error: $LAUNCHER_BINARY not found or not executable — run build-binaries.sh first" >&2
    exit 1
fi

if ! docker image inspect "$BASE_IMAGE" >/dev/null 2>&1; then
    # Tilt tracks the base-image local resource by its Dockerfile inputs, not
    # by whether Docker still retains the resulting tag. Docker Desktop can
    # evict an unused local image while Tilt still reports that resource as
    # Ready, which otherwise leaves every later worker rebuild permanently
    # broken until someone manually retriggers the heavy base resource.
    echo "==> base image $BASE_IMAGE is missing; rebuilding it"
    BASE_TAG="$BASE_IMAGE" "$BASE_BUILD_SCRIPT"
fi

BUILD_CTX="$(mktemp -d)"
trap 'rm -rf "$BUILD_CTX"' EXIT

cp "$BINARY" "$BUILD_CTX/djinn-agent-worker"
cp "$LAUNCHER_BINARY" "$BUILD_CTX/djinn-cgroup-launcher"
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
