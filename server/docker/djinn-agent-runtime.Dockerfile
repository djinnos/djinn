# syntax=docker/dockerfile:1.7
# djinn-agent-runtime — the per-task-run sandbox image used by
# `LocalDockerRuntime` (see server/crates/djinn-runtime/).
#
# The server (running outside Docker, or in its own container) performs a
# host-side mirror clone per task run, then `docker run`s this image with:
#
#   /workspace  -> freshly-cloned task-branch worktree (bind, rw)
#   /mirror     -> shared mirror root                  (bind, ro)
#   /cache/*    -> cargo / pnpm / pip caches           (bind, rw)
#   /var/run/djinn -> IPC socket directory             (bind, rw)
#
# The entrypoint is `djinn-agent-worker`, which reads the `TaskRunSpec` from
# stdin (length-prefixed bincode frame) and dials `$DJINN_IPC_SOCKET` for the
# reverse-RPC channel back to the host-side `SupervisorServices` impl.
#
# Workspace layout this Dockerfile builds from is the full `server/` Cargo
# workspace. The referenced `djinn-agent-worker` crate is one of its members
# (see server/Cargo.toml). Stage 1 compiles only that binary; the rest of the
# workspace is pulled in as source only for resolution.
#
# The image is NOT a compose service — `docker-compose.yml` only lays out the
# host volume paths. `LocalDockerRuntime` pulls/builds this image separately
# and runs one container per task-run.

###############################################################################
# Stage 1: build the worker binary from the Cargo workspace.
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

WORKDIR /src
COPY server /src/server

WORKDIR /src/server

# Override the workspace .cargo/config.toml (which expects sccache + a specific
# linker setup on the dev host) with a container-local config that keeps
# things building reliably.
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

# Cache mounts make the build fast on rebuild but their contents vanish after
# the RUN step, so we copy the binary out to a regular image layer (/out).
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/server/target \
    set -eux; \
    cargo build --release --locked -p djinn-agent-worker; \
    mkdir -p /out; \
    cp /src/server/target/release/djinn-agent-worker /out/djinn-agent-worker; \
    strip /out/djinn-agent-worker

###############################################################################
# Stage 2: fetch language servers into a throwaway layer.
#
# Kept separate from the runtime stage so we can bust the LSP cache without
# rebuilding debian apt layers, and so failed LSP fetches don't leave
# half-populated paths in the final image.
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

# Node (for typescript-language-server + pyright). Pinned LTS tarball; we
# unpack into /opt/node so it relocates cleanly into the runtime stage.
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

# typescript-language-server + pyright. `npm install -g` into /opt/node keeps
# the binaries colocated with the node runtime; both are thin JS wrappers.
ARG TYPESCRIPT_LANGUAGE_SERVER_VERSION=4.3.3
ARG PYRIGHT_VERSION=1.1.389
RUN npm install -g \
        typescript@5.6.3 \
        typescript-language-server@${TYPESCRIPT_LANGUAGE_SERVER_VERSION} \
        pyright@${PYRIGHT_VERSION}

# rust-analyzer — grab the official release tarball rather than `cargo install`
# (saves ~15 minutes and a cargo registry cache in the build).
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
# Stage 3: the runtime image.
###############################################################################
FROM debian:bookworm-slim AS runtime

ENV DEBIAN_FRONTEND=noninteractive

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
    && rm -rf /var/lib/apt/lists/*

# Rust toolchain via rustup. Installed to /usr/local/{cargo,rustup} so it's
# shared across users (the worker runs as a non-root user below).
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:/opt/node/bin:$PATH

RUN set -eux; \
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --default-toolchain stable --no-modify-path; \
    chmod -R a+rX /usr/local/rustup /usr/local/cargo; \
    rustc --version; \
    cargo --version

# Copy the LSPs + Node runtime out of the LSP stage.
COPY --from=lsp /opt/node /opt/node
COPY --from=lsp /usr/local/bin/rust-analyzer /usr/local/bin/rust-analyzer

# Copy the worker binary from the build stage.
COPY --from=build /out/djinn-agent-worker /usr/local/bin/djinn-agent-worker

# Non-root user. Matches the uid documented in the plan (10001) so bind-mount
# ownership on the host (/var/lib/djinn/...) can be pre-configured.
RUN groupadd --system --gid 10001 djinn \
    && useradd --system --uid 10001 --gid 10001 --home /home/djinn --create-home --shell /usr/sbin/nologin djinn \
    && mkdir -p /workspace /mirror /cache/cargo /cache/pnpm /cache/pip /var/run/djinn \
    && chown -R djinn:djinn /workspace /cache /var/run/djinn /home/djinn

# Per-run env defaults. `LocalDockerRuntime` overrides any of these that need
# to be task-specific (notably DJINN_IPC_SOCKET).
ENV CARGO_HOME=/cache/cargo \
    CARGO_TARGET_DIR=/workspace/target \
    PNPM_STORE_DIR=/cache/pnpm \
    PIP_CACHE_DIR=/cache/pip \
    RUSTUP_HOME=/usr/local/rustup \
    PATH=/usr/local/cargo/bin:/opt/node/bin:/usr/local/bin:/usr/bin:/bin

USER djinn
WORKDIR /workspace

# tini for correct PID 1 signal handling so `docker kill` → SIGTERM → the
# worker can flush an in-flight terminal frame before exit.
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/djinn-agent-worker"]
