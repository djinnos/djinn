#!/usr/bin/env bash
# Build the heavy `djinn-agent-runtime-base` image (LSPs + rustup + sccache
# + mold + clang + apt deps). The per-task-run image FROMs this one.
#
# Local-only tag — not pushed to the kind-local registry. The thin wrap
# step (wrap-agent-runtime-image.sh) resolves the FROM against the local
# docker image store and pushes only the composed final image.
#
# Rebuild triggers (declared in the Tiltfile's deps= for this resource):
#   - server/docker/djinn-agent-runtime-base.Dockerfile
# Typical change frequency: rare (Node/rust-analyzer version bumps, apt dep
# additions, toolchain swaps). Not triggered by worker source edits.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BASE_TAG="${BASE_TAG:-djinn-agent-runtime-base:dev}"
DOCKERFILE="$REPO_ROOT/server/docker/djinn-agent-runtime-base.Dockerfile"

# Empty build context — the base image doesn't COPY anything from the
# repo, just FROMs debian-slim and curls tarballs from the internet.
# Using /tmp keeps the context tiny (a few bytes) so `docker build`
# doesn't waste time tarring the repo root.
BUILD_CTX="$(mktemp -d)"
trap 'rm -rf "$BUILD_CTX"' EXIT
cp "$DOCKERFILE" "$BUILD_CTX/Dockerfile"

echo "==> building $BASE_TAG"
DOCKER_BUILDKIT=1 docker build -t "$BASE_TAG" "$BUILD_CTX"
echo "==> done: $BASE_TAG"
