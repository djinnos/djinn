# syntax=docker/dockerfile:1.7
# Multistage build for djinn-server.
#
# Stage 1: compile the release binary with SQLX_OFFLINE so no live DB is needed.
# Stage 2: a slim Debian runtime with only the shared libs the binary links to.

########################
# Stage 1: builder
########################
FROM rust:1.82-slim-bookworm AS builder

ENV DEBIAN_FRONTEND=noninteractive \
    SQLX_OFFLINE=true \
    CARGO_TERM_COLOR=always

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
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy the entire workspace. A good .dockerignore is important so we don't
# drag target/ or ~/.djinn into the build context.
COPY . .

# Build from the server/ crate root. .sqlx/ lives there and is consumed by
# sqlx macros at compile time when SQLX_OFFLINE=true.
WORKDIR /build/server

# The host's .cargo/config.toml declares sccache as the rustc wrapper and
# mold+clang as the linker. Neither is in the builder image, so we override
# those with an in-container config and unset the wrapper env var for good
# measure. Install mold for its linker win (optional — plain ld works too).
RUN apt-get update && apt-get install -y --no-install-recommends mold \
    && rm -rf /var/lib/apt/lists/*

ENV RUSTC_WRAPPER= \
    CARGO_BUILD_RUSTC_WRAPPER=

RUN mkdir -p .cargo && printf '%s\n' \
    '[build]' \
    '# rustc-wrapper intentionally unset; host .cargo/config.toml requires sccache.' \
    '' \
    '[target.x86_64-unknown-linux-gnu]' \
    'linker = "clang"' \
    'rustflags = ["-C", "link-arg=-fuse-ld=mold"]' \
    '' \
    '[net]' \
    'git-fetch-with-cli = true' \
    > .cargo/config.toml

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/server/target \
    cargo build --release --locked --bin djinn-server \
    && cp /build/server/target/release/djinn-server /usr/local/bin/djinn-server

########################
# Stage 2: runtime
########################
FROM debian:bookworm-slim AS runtime

ENV DEBIAN_FRONTEND=noninteractive \
    RUST_LOG=info

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        git \
        openssl \
        libssl3 \
        tini \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/bin/djinn-server /usr/local/bin/djinn-server

# Pre-create ~/.djinn so it exists before the bind-mount overlays it
# (compose mounts the host ~/.djinn here anyway, but this keeps the image
# usable without a mount).
RUN mkdir -p /root/.djinn /workspace

EXPOSE 8372

# tini for correct PID 1 signal handling — djinn-server spawns children.
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/djinn-server"]
CMD []
