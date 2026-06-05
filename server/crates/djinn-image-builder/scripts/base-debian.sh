#!/usr/bin/env bash
# Base layer for Debian-derived images. Installs the essentials every
# downstream script assumes are available: bash, curl, git, tini,
# ca-certificates, gnupg (for third-party repo keys). It also bakes in the
# common native build deps (libpq-dev, cmake, pkg-config) so project
# build/test commands can link the ubiquitous `-sys` crates by default —
# see the second install pass below. apt-cache is left dirty on purpose —
# install-system.sh cleans up after its own pass.
set -euo pipefail

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends \
    bash \
    ca-certificates \
    curl \
    git \
    gnupg \
    libatomic1 \
    tini \
    unzip \
    xz-utils
# libatomic1: the prebuilt Node binaries (v26+) dynamically link libatomic.so.1,
# which trixie-slim does not ship — without it `node` dies at startup with
# "error while loading shared libraries: libatomic.so.1" (exit 127) and the
# Node image build fails. Tiny runtime lib, harmless for the other languages.
# Native build deps baked in by default so a project's test/build can link
# common `-sys` crates without each project having to list them in
# `system_packages`:
#   - libpq-dev   : link Postgres-client test binaries (sqlx/diesel `-lpq`).
#                   `postgresql-client` only ships the runtime libpq5, not
#                   the `.so` symlink + `libpq-fe.h` the linker needs.
#   - cmake       : build script for crates like aws-lc-sys (rustls backend),
#                   which fails its cc fallback without it.
#   - pkg-config  : how openssl-sys / libpq-sys / many `-sys` crates locate
#                   their system libs at build time.
apt-get install -y --no-install-recommends \
    cmake \
    libpq-dev \
    pkg-config
apt-get clean
rm -rf /var/lib/apt/lists/*

mkdir -p /opt/djinn/bin /etc/profile.d /etc/djinn

# Runtime user. The task-run/warm Pods run as runAsUser=10001, but this image
# (FROM debian:trixie-slim) otherwise has no matching passwd entry or home
# directory, so $HOME resolves to "/" and every $HOME-relative path the agent
# uses — its Landlock scratch dir (~/.cache/djinn), the LSP auto-install dir
# (~/.local/share/djinn/bin, used for gopls/rust-analyzer/npm servers), and
# Go's default GOCACHE (~/.cache/go-build) — lands in root-owned / and fails
# with EACCES. Create the user + a writable home so HOME=/home/djinn (set as
# an ENV in the generated Dockerfile) is actually usable. The `|| true`s keep
# the build idempotent if the uid/gid ever pre-exist on a newer base.
groupadd --system --gid 10001 djinn 2>/dev/null || true
useradd --system --uid 10001 --gid 10001 --home-dir /home/djinn --shell /usr/sbin/nologin djinn 2>/dev/null || true
mkdir -p /home/djinn
chown 10001:10001 /home/djinn
chmod 0775 /home/djinn
