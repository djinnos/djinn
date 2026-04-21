# syntax=docker/dockerfile:1.7
# djinn-server — the long-lived controller Pod that runs in the Djinn Helm
# release. Listens for HTTP traffic (UI + REST) on `:3000` and for worker
# reverse-RPC traffic (bincode-over-TCP) on `:8443`. It dispatches one Job
# per task run using the `djinn-agent-runtime` image.
#
# Build context expectations (mirrors `djinn-agent-runtime.Dockerfile`): the
# root repo is the context (`server/` + `.sqlx` are visible). Invoke from the
# repo root, e.g.:
#
#   docker build -f server/docker/djinn-server.Dockerfile -t djinn-server:dev .
#
# Produces a `debian:bookworm-slim` runtime image containing only the
# `djinn-server` binary, `git`, `ca-certificates`, `libssl3`, and `tini` for
# PID-1 signal handling. The non-root uid matches the one baked into
# `djinn-agent-runtime.Dockerfile` (10001) so shared PVCs (mirrors, cache) can
# be mounted by both images without chown dances.

###############################################################################
# Build pipeline (cargo-chef): split dep-compile from source-compile so source
# changes don't bust the (slow) dep layer.
#
# Layer reuse expected:
#   - source-only edit  → planner re-runs (cheap), `chef cook` layer is cached,
#                         only `cargo build` runs (incremental, only touched
#                         crates recompile).
#   - Cargo.toml/lock   → planner emits a new recipe.json, `chef cook` layer
#                         busts and rebuilds all deps from scratch.
#
# Env notes:
#   The host workspace's .cargo/config.toml sets `rustc-wrapper = "sccache"`,
#   which would crash any cargo invocation in this image (no sccache binary).
#   `RUSTC_WRAPPER=` + `CARGO_BUILD_RUSTC_WRAPPER=` override that. They live
#   on the chef base so planner inherits them too — without that, even
#   `cargo chef prepare` (just `cargo metadata`) crashes with "container
#   process is already dead" when cargo tries to spawn the missing wrapper.
###############################################################################

FROM rust:1.82-slim-bookworm AS chef

ENV DEBIAN_FRONTEND=noninteractive \
    CARGO_TERM_COLOR=always \
    SQLX_OFFLINE=true \
    RUSTC_WRAPPER= \
    CARGO_BUILD_RUSTC_WRAPPER=

# System deps live on the chef base so both planner and builder inherit.
# Planner needs `git` (cargo's git-fetch-with-cli) and a working CC; builder
# additionally uses clang+mold for linking, libssl-dev/protobuf for crate
# build scripts.
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config \
        libssl-dev \
        ca-certificates \
        git \
        build-essential \
        cmake \
        clang \
        libclang-dev \
        protobuf-compiler \
        mold \
    && rm -rf /var/lib/apt/lists/*

RUN cargo install cargo-chef --locked --version 0.1.68
WORKDIR /app

###############################################################################
# Planner stage: extract dep info into recipe.json. Source is copied so chef
# can read every Cargo.toml in the workspace, but no compilation happens here.
###############################################################################
FROM chef AS planner
COPY server /app/server
WORKDIR /app/server
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo chef prepare --recipe-path recipe.json

###############################################################################
# Builder stage: cook deps from recipe.json, then build the binary from source.
# Inherits all system + cargo + env config from chef.
###############################################################################
FROM chef AS builder

WORKDIR /app/server

# Cook deps. recipe.json is tiny + content-addressed by Cargo.toml/lock, so
# this layer is cached unless deps actually change. Cooked output lands in
# the target cache mount, surviving across builds.
COPY --from=planner /app/server/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/server/target \
    cargo chef cook --release --recipe-path recipe.json -p djinn-server

# Bring in the actual source and build only the binary. Deps are already
# compiled and sitting in the target cache mount, so this is pure
# incremental compilation of djinn-server's own crates.
COPY server /app/server
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/server/target \
    set -eux; \
    cargo build --release --locked -p djinn-server; \
    mkdir -p /out; \
    cp /app/server/target/release/djinn-server /out/djinn-server; \
    strip /out/djinn-server

###############################################################################
# Stage 2: the runtime image.
###############################################################################
FROM debian:bookworm-slim AS runtime

ENV DEBIAN_FRONTEND=noninteractive \
    RUST_LOG=info

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        git \
        libssl3 \
        openssl \
        tini \
    && rm -rf /var/lib/apt/lists/*

# Non-root user. Uid/gid match `djinn-agent-runtime.Dockerfile` so shared
# PVCs (`/var/lib/djinn/mirrors`, `/var/lib/djinn/cache/*`) are readable by
# both the server and worker Pods without chown gymnastics.
RUN groupadd --system --gid 10001 djinn \
    && useradd --system --uid 10001 --gid 10001 --home /home/djinn --create-home --shell /usr/sbin/nologin djinn \
    && mkdir -p /var/lib/djinn/mirrors /var/lib/djinn/cache/cargo /var/lib/djinn/cache/pnpm /var/lib/djinn/cache/pip \
    && chown -R djinn:djinn /var/lib/djinn /home/djinn

COPY --from=builder /out/djinn-server /usr/local/bin/djinn-server

# :3000 — HTTP API + UI (matches service.apiPort in values.yaml).
# :8443 — worker reverse-RPC TCP listener (matches service.rpcPort).
EXPOSE 3000 8443

USER djinn
WORKDIR /home/djinn

# tini handles PID 1 so `kubectl delete pod` → SIGTERM propagates to
# djinn-server's graceful-shutdown path (HTTP drain + RPC listener cancel).
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/djinn-server"]
CMD []
