#!/usr/bin/env bash
# Install the requested LLVM/Clang toolchain via apt. Skeleton — no
# C/C++ consumer today beyond build deps that install-system.sh
# handles.
#
# Inputs (env):
#   CLANG_VERSION — required. e.g. "18".
set -euo pipefail

: "${CLANG_VERSION:?CLANG_VERSION is required, e.g. \"18\"}"

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends \
    "clang-${CLANG_VERSION}" \
    "lld-${CLANG_VERSION}" \
    "libc++-${CLANG_VERSION}-dev" \
    "libc++abi-${CLANG_VERSION}-dev"
apt-get clean
rm -rf /var/lib/apt/lists/*

# Symlink the versioned binaries to unsuffixed names so `clang` /
# `clang++` / `ld.lld` are picked up by default.
for bin in clang clang++ ld.lld; do
    target="/usr/bin/${bin}-${CLANG_VERSION}"
    if [ -x "${target}" ]; then
        ln -sf "${target}" "/usr/local/bin/${bin}"
    fi
done
