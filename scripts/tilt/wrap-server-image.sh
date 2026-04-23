#!/usr/bin/env bash
# Wrap the host-built `djinn-server` binary (staged by build-binaries.sh)
# into a thin debian-slim runtime image and push to the kind-local registry.
#
# Kept separate from build-binaries.sh so Tilt can split the
# resource_deps: cargo build once, then wrap into the two different images
# in parallel.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
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

echo "==> building $IMAGE_TAG"
docker build -t "$IMAGE_TAG" "$BUILD_CTX"
docker push "$IMAGE_TAG"
echo "==> done: $IMAGE_TAG"
