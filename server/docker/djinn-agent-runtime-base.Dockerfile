# syntax=docker/dockerfile:1.7
# djinn-agent-runtime-base — the heavy, slow-churning layer that the
# per-task-run sandbox image is built on top of.
#
# Split out of `djinn-agent-runtime.Dockerfile` so that edits to
# `djinn-agent-worker` (the worker binary produced out-of-band by the Tilt
# build pipeline) don't invalidate the LSP fetches / rustup install / apt
# layers underneath. Rebuild this image only when:
#   - Node / rust-analyzer / pyright versions bump
#   - sccache / mold / clang toolchain changes
#   - Debian base apt deps change
#
# Intentionally NOT copying anything from the djinn cargo workspace: the
# worker binary lands in the top image via `COPY` from the host-side build.
#
# Toolchain summary baked in:
#   - rustup + stable Rust (needed by `cargo`-driven tasks)
#   - sccache wired as RUSTC_WRAPPER with SCCACHE_DIR=/cache/sccache so the
#     shared cache PVC (mounted at /cache) persists compilation units
#     across task runs — same pattern as /cache/cargo for the registry
#   - mold + clang linker, exposed via CARGO_BUILD_RUSTFLAGS so any `cargo
#     build`/`cargo check`/`cargo clippy` a task invokes gets fast linking
#   - Node 20 + typescript-language-server + pyright + rust-analyzer

###############################################################################
# Stage 1: fetch language-server tarballs into a throwaway layer.
###############################################################################
FROM debian:bookworm-slim AS lsp

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        xz-utils \
        python3 \
        python3-pip \
        python3-venv \
        unzip \
    && rm -rf /var/lib/apt/lists/*

ARG NODE_VERSION=20.17.0
RUN set -eux; \
    arch="$(dpkg --print-architecture)"; \
    case "$arch" in \
        amd64) node_arch=x64 ;; \
        arm64) node_arch=arm64 ;; \
        *) echo "unsupported arch: $arch" >&2; exit 1 ;; \
    esac; \
    curl -fsSL "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-${node_arch}.tar.xz" \
        -o /tmp/node.tar.xz; \
    mkdir -p /opt/node; \
    tar -xJf /tmp/node.tar.xz -C /opt/node --strip-components=1; \
    rm /tmp/node.tar.xz

ENV PATH=/opt/node/bin:$PATH

ARG TYPESCRIPT_LANGUAGE_SERVER_VERSION=4.3.3
ARG PYRIGHT_VERSION=1.1.389
RUN npm install -g \
        typescript@5.6.3 \
        typescript-language-server@${TYPESCRIPT_LANGUAGE_SERVER_VERSION} \
        pyright@${PYRIGHT_VERSION}

ARG RUST_ANALYZER_VERSION=2024-09-30
RUN set -eux; \
    arch="$(dpkg --print-architecture)"; \
    case "$arch" in \
        amd64) ra_arch=x86_64-unknown-linux-gnu ;; \
        arm64) ra_arch=aarch64-unknown-linux-gnu ;; \
        *) echo "unsupported arch: $arch" >&2; exit 1 ;; \
    esac; \
    curl -fsSL "https://github.com/rust-lang/rust-analyzer/releases/download/${RUST_ANALYZER_VERSION}/rust-analyzer-${ra_arch}.gz" \
        -o /tmp/rust-analyzer.gz; \
    gunzip /tmp/rust-analyzer.gz; \
    install -m 0755 /tmp/rust-analyzer /usr/local/bin/rust-analyzer; \
    rm -f /tmp/rust-analyzer

###############################################################################
# Stage 2: the base runtime image. Adds rustup, sccache, mold, clang on top
# of debian:bookworm-slim + tini and carries the LSPs forward.
###############################################################################
FROM debian:bookworm-slim AS base

ENV DEBIAN_FRONTEND=noninteractive

# Runtime + build toolchain deps. Kept in one RUN so apt layer size stays
# bounded, and so the lists/ cache is dropped before the layer seals.
#   - git, ca-certificates, openssl/libssl3: network fetches + TLS
#   - pkg-config, build-essential: common native-dep build tooling
#   - python3 + venv + pip: for pyright + python language workflows
#   - tini: PID-1 signal handler
#   - curl, xz-utils: bootstrapping rustup + extracting tarballs
#   - clang, mold: fast linker path used by CARGO_BUILD_RUSTFLAGS below
#   - sccache: compilation-result cache, keyed by (rustc version + source
#     hash + flags). Shared across task runs via /cache/sccache, so
#     identical rustc invocations in different tasks hit the cache.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        git \
        openssl \
        libssl3 \
        pkg-config \
        build-essential \
        python3 \
        python3-pip \
        python3-venv \
        tini \
        curl \
        xz-utils \
        clang \
        mold \
        sccache \
    && rm -rf /var/lib/apt/lists/*

# Rustup → /usr/local/{cargo,rustup}. World-readable so the non-root user
# below can invoke cargo/rustc without a chown.
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:/opt/node/bin:$PATH

RUN set -eux; \
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --default-toolchain stable --no-modify-path; \
    chmod -R a+rX /usr/local/rustup /usr/local/cargo; \
    rustc --version; \
    cargo --version

# LSPs + Node runtime from stage 1.
COPY --from=lsp /opt/node /opt/node
COPY --from=lsp /usr/local/bin/rust-analyzer /usr/local/bin/rust-analyzer

# Non-root user. Uid/gid 10001 matches the `djinn` user baked into the
# server image so shared PVCs (mirrors, cache) mount cleanly on both sides.
RUN groupadd --system --gid 10001 djinn \
    && useradd --system --uid 10001 --gid 10001 --home /home/djinn --create-home --shell /usr/sbin/nologin djinn \
    && mkdir -p /workspace /mirror /cache/cargo /cache/pnpm /cache/pip /cache/sccache /var/run/djinn \
    && chown -R djinn:djinn /workspace /cache /var/run/djinn /home/djinn

# Per-run env defaults. Overridable by the Job spec (K8sRuntime) or the
# docker-run env (LocalDockerRuntime) if a task needs per-task-specific
# overrides (notably DJINN_IPC_SOCKET).
#
# Notes:
#   - CARGO_HOME points at /cache/cargo (registry + git fetch cache survive
#     across tasks). Rustup itself lives under /usr/local/cargo, and
#     `cargo`'s search path still finds it via the rustup symlink layout
#     — rustup-installed toolchains read RUSTUP_HOME=/usr/local/rustup.
#   - CARGO_TARGET_DIR is per-task (under the ephemeral /workspace), so
#     sccache is the mechanism that makes compilation reusable across
#     tasks. Every rustc invocation goes through `sccache` (RUSTC_WRAPPER)
#     and hits /cache/sccache on second run of the same compilation unit.
#   - CARGO_BUILD_RUSTFLAGS pulls in mold unconditionally. Any workspace
#     .cargo/config.toml a task checks out can still override this — env
#     vars lose to config.toml — but by default we get fast linking.
ENV CARGO_HOME=/cache/cargo \
    CARGO_TARGET_DIR=/workspace/target \
    PNPM_STORE_DIR=/cache/pnpm \
    PIP_CACHE_DIR=/cache/pip \
    RUSTUP_HOME=/usr/local/rustup \
    RUSTC_WRAPPER=sccache \
    SCCACHE_DIR=/cache/sccache \
    SCCACHE_CACHE_SIZE=10G \
    CARGO_BUILD_RUSTFLAGS=-Clink-arg=-fuse-ld=mold \
    PATH=/usr/local/cargo/bin:/opt/node/bin:/usr/local/bin:/usr/bin:/bin
