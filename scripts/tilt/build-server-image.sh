#!/usr/bin/env bash
# Build djinn-server inside a bookworm-compatible container and wrap the
# binary in a thin runtime image at localhost:5001/djinn-server:dev.
#
# Why not docker_build() in the Tiltfile: BuildKit's
# --mount=type=cache,target=/app/server/target inside
# server/docker/djinn-server.Dockerfile was wedging such that source
# edits to server/** reused a stale compiled djinn-server from the
# target cache — Tilt "builds" completed in 1s, no new image layers.
#
# This script uses a named docker volume for the cargo target dir (so
# incremental compilation survives across invocations) without going
# through BuildKit's cache-mount mechanism.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
IMAGE_TAG="${IMAGE_TAG:-localhost:5001/djinn-server:dev}"
BUILDER_IMAGE="${BUILDER_IMAGE:-rust:1-slim-bookworm}"
CARGO_REGISTRY_VOLUME="${CARGO_REGISTRY_VOLUME:-djinn-server-cargo-registry}"
TARGET_VOLUME="${TARGET_VOLUME:-djinn-server-target-cache}"

cd "$REPO_ROOT"

docker image inspect "$BUILDER_IMAGE" >/dev/null 2>&1 || docker pull "$BUILDER_IMAGE"

echo "==> cargo build --release inside $BUILDER_IMAGE"
docker run --rm \
    -v "$REPO_ROOT:/app" \
    -v "${CARGO_REGISTRY_VOLUME}:/usr/local/cargo/registry" \
    -v "${TARGET_VOLUME}:/app/server/target" \
    -w /app/server \
    -e SQLX_OFFLINE=true \
    -e RUSTC_WRAPPER= \
    -e CARGO_BUILD_RUSTC_WRAPPER= \
    "$BUILDER_IMAGE" \
    sh -c 'command -v pkg-config >/dev/null 2>&1 || (apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev ca-certificates git build-essential cmake clang libclang-dev protobuf-compiler mold && rm -rf /var/lib/apt/lists/*); cargo build --release --locked --features qdrant -p djinn-server'

BUILD_CTX="$(mktemp -d)"
trap 'rm -rf "$BUILD_CTX"' EXIT
docker run --rm \
    -v "${TARGET_VOLUME}:/target" \
    -v "$BUILD_CTX:/out" \
    "$BUILDER_IMAGE" \
    cp /target/release/djinn-server /out/djinn-server

echo "==> assembling $IMAGE_TAG"
cat > "$BUILD_CTX/Dockerfile" <<'DOCKERFILE'
FROM debian:bookworm-slim
ENV DEBIAN_FRONTEND=noninteractive RUST_LOG=info
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates git libssl3 openssl tini \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 djinn \
    && useradd --system --uid 10001 --gid 10001 --home /home/djinn --create-home --shell /usr/sbin/nologin djinn \
    && mkdir -p /var/lib/djinn/mirrors /var/lib/djinn/cache /var/lib/djinn/projects \
    && chown -R djinn:djinn /var/lib/djinn /home/djinn
COPY djinn-server /usr/local/bin/djinn-server
RUN chmod +x /usr/local/bin/djinn-server
EXPOSE 3000 8443
USER djinn
WORKDIR /home/djinn
ENTRYPOINT ["/usr/bin/tini","--","/usr/local/bin/djinn-server"]
DOCKERFILE

docker build -t "$IMAGE_TAG" "$BUILD_CTX"
docker push "$IMAGE_TAG"
echo "==> done: $IMAGE_TAG"
