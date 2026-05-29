#!/usr/bin/env bash
# Base layer for Debian-derived images. Installs the essentials every
# downstream script assumes are available: bash, curl, git, tini,
# ca-certificates, gnupg (for third-party repo keys). apt-cache is left
# dirty on purpose — install-system.sh cleans up after its own pass.
set -euo pipefail

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends \
    bash \
    ca-certificates \
    curl \
    git \
    gnupg \
    tini \
    unzip \
    xz-utils
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
