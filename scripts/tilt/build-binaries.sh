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
# Why host-side (in a rust:1-slim-bookworm container) and not BuildKit:
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

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BUILDER_IMAGE="${BUILDER_IMAGE:-rust:1-slim-bookworm}"
CARGO_REGISTRY_VOLUME="${CARGO_REGISTRY_VOLUME:-djinn-cargo-registry}"
TARGET_VOLUME="${TARGET_VOLUME:-djinn-cargo-target}"
SCCACHE_VOLUME="${SCCACHE_VOLUME:-djinn-sccache}"
ARTIFACTS_DIR="${ARTIFACTS_DIR:-$REPO_ROOT/.tilt/artifacts}"

cd "$REPO_ROOT"
mkdir -p "$ARTIFACTS_DIR"

docker image inspect "$BUILDER_IMAGE" >/dev/null 2>&1 || docker pull "$BUILDER_IMAGE"

echo "==> cargo build (djinn-server + djinn-agent-worker) in $BUILDER_IMAGE"
docker run --rm \
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
    "$BUILDER_IMAGE" \
    sh -c '
        set -eux
        # Install the toolchain pieces not baked into rust:1-slim-bookworm.
        # sccache is installed via apt so the binary lands on $PATH without
        # a cargo-install detour; mold + clang provide the link-time win
        # declared via CARGO_BUILD_RUSTFLAGS above. pkg-config + libssl-dev
        # + protobuf-compiler are transitive build-deps of git2 / tonic.
        if ! command -v sccache >/dev/null 2>&1; then
            apt-get update
            apt-get install -y --no-install-recommends \
                pkg-config libssl-dev ca-certificates git \
                build-essential cmake clang libclang-dev \
                protobuf-compiler mold sccache
            rm -rf /var/lib/apt/lists/*
        fi
        sccache --start-server || true
        cargo build --release --locked \
            --features qdrant \
            -p djinn-server \
            -p djinn-agent-worker
        sccache --show-stats || true
    '

echo "==> extracting binaries into $ARTIFACTS_DIR"
# A second short `docker run` lets us read the release binaries out of the
# target volume into a regular host dir — named volumes aren't directly
# addressable from `docker cp` without a container, and copying through
# the build container would force it to outlive the cargo invocation.
docker run --rm \
    -v "${TARGET_VOLUME}:/target" \
    -v "${ARTIFACTS_DIR}:/out" \
    "$BUILDER_IMAGE" \
    sh -c '
        set -eux
        cp /target/release/djinn-server      /out/djinn-server
        cp /target/release/djinn-agent-worker /out/djinn-agent-worker
        # Strip both binaries to trim image size. `strip` is in binutils
        # which ships in the rust:*-slim base.
        strip /out/djinn-server
        strip /out/djinn-agent-worker
        chmod +x /out/djinn-server /out/djinn-agent-worker
    '

echo "==> done: $ARTIFACTS_DIR/{djinn-server,djinn-agent-worker}"
