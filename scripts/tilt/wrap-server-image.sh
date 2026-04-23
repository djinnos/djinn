#!/usr/bin/env bash
# Wrap the host-built `djinn-server` binary (staged by build-binaries.sh)
# into the runtime image defined by server/docker/djinn-server.Dockerfile
# and push to the kind-local registry.
#
# Kept separate from build-binaries.sh so Tilt can split the
# resource_deps: cargo build once, then wrap into the two different images
# in parallel.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
DOCKERFILE="$REPO_ROOT/server/docker/djinn-server.Dockerfile"
# Tilt's `custom_build` invokes this with $EXPECTED_REF set to a per-build
# content-hashed tag (e.g. localhost:5001/djinn-server:tilt-build-…); that ref
# ends up rewritten into the Deployment's PodSpec so K8s actually rolls the
# pod. Standalone callers can override via $IMAGE_TAG or fall back to :dev.
IMAGE_TAG="${EXPECTED_REF:-${IMAGE_TAG:-localhost:5001/djinn-server:dev}}"
ARTIFACTS_DIR="${ARTIFACTS_DIR:-$REPO_ROOT/.tilt/artifacts}"
BINARY="$ARTIFACTS_DIR/djinn-server"

if [[ ! -x "$BINARY" ]]; then
    echo "error: $BINARY not found or not executable — run build-binaries.sh first" >&2
    exit 1
fi

BUILD_CTX="$(mktemp -d)"
trap 'rm -rf "$BUILD_CTX"' EXIT

cp "$BINARY" "$BUILD_CTX/djinn-server"

echo "==> building $IMAGE_TAG"
docker build -f "$DOCKERFILE" -t "$IMAGE_TAG" "$BUILD_CTX"
docker push "$IMAGE_TAG"
echo "==> done: $IMAGE_TAG"
