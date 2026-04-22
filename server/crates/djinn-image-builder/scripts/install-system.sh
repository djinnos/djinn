#!/usr/bin/env bash
# Install a batch of apt / apk packages.
#
# Inputs (env, space-separated):
#   APT_PACKAGES  — packages to install via apt-get (Debian).
#   APK_PACKAGES  — packages to install via apk (Alpine).
#
# Exactly one of the two is expected to be populated per call; the
# other is ignored. The Dockerfile generator emits two separate RUN
# lines for distros that need both, so we don't need to dual-dispatch.
set -euo pipefail

if [ -n "${APT_PACKAGES:-}" ]; then
    export DEBIAN_FRONTEND=noninteractive
    apt-get update
    # shellcheck disable=SC2086 # intentional word-splitting of $APT_PACKAGES
    apt-get install -y --no-install-recommends $APT_PACKAGES
    apt-get clean
    rm -rf /var/lib/apt/lists/*
fi

if [ -n "${APK_PACKAGES:-}" ]; then
    # shellcheck disable=SC2086
    apk add --no-cache $APK_PACKAGES
fi
