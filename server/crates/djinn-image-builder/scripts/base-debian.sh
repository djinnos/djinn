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
    xz-utils
apt-get clean
rm -rf /var/lib/apt/lists/*

mkdir -p /opt/djinn/bin /etc/profile.d /etc/djinn
