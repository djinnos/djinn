#!/usr/bin/env bash
# Base layer for Alpine-derived images. Installs the busybox-augmenting
# tools every downstream script assumes are available.
set -euo pipefail

apk add --no-cache \
    bash \
    ca-certificates \
    curl \
    git \
    gnupg \
    tini \
    xz

mkdir -p /opt/djinn/bin /etc/profile.d /etc/djinn
