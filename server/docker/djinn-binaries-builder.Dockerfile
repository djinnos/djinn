# djinn-binaries-builder — cached toolchain image for the Tilt binary build.
#
# Keep package installation out of every `djinn-binaries` invocation. The
# build script rebuilds this image through Docker's layer cache, so an unchanged
# Dockerfile is a cheap cache hit while dependency edits invalidate correctly.

FROM rust:1-slim-bookworm

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        clang \
        cmake \
        git \
        libclang-dev \
        libssl-dev \
        mold \
        pkg-config \
        protobuf-compiler \
        sccache \
    && rm -rf /var/lib/apt/lists/*
