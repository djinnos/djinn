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
# Stage 1: compile the release binary from the Cargo workspace.
###############################################################################
FROM rust:1.82-slim-bookworm AS build

ENV DEBIAN_FRONTEND=noninteractive \
    CARGO_TERM_COLOR=always \
    SQLX_OFFLINE=true \
    RUSTC_WRAPPER= \
    CARGO_BUILD_RUSTC_WRAPPER=

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

WORKDIR /app
COPY server /app/server

WORKDIR /app/server

# Override the host workspace .cargo/config.toml (which expects sccache + a
# specific host linker setup) with a container-local config that just works.
RUN mkdir -p .cargo && printf '%s\n' \
    '[build]' \
    '' \
    '[target.x86_64-unknown-linux-gnu]' \
    'linker = "clang"' \
    'rustflags = ["-C", "link-arg=-fuse-ld=mold"]' \
    '' \
    '[net]' \
    'git-fetch-with-cli = true' \
    > .cargo/config.toml

# Cache mounts speed up iterative rebuilds. Contents vanish after the RUN
# step, so we copy the binary out to a regular image layer (/out).
#
# Cargo writes to `/app/server/target` (workspace-relative — Cargo.toml
# lives at /app/server). The cache mount target MUST match that path or
# every build is a cold compile. Also cache `cargo/git` for git deps.
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

COPY --from=build /out/djinn-server /usr/local/bin/djinn-server

# :3000 — HTTP API + UI (matches service.apiPort in values.yaml).
# :8443 — worker reverse-RPC TCP listener (matches service.rpcPort).
EXPOSE 3000 8443

USER djinn
WORKDIR /home/djinn

# tini handles PID 1 so `kubectl delete pod` → SIGTERM propagates to
# djinn-server's graceful-shutdown path (HTTP drain + RPC listener cancel).
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/djinn-server"]
CMD []
