#!/usr/bin/env bash
# Install the requested LLVM/Clang toolchain via apt. Skeleton — no
# C/C++ consumer today beyond build deps that install-system.sh
# handles.
#
# Inputs (env):
#   CLANG_VERSION       — required. e.g. "18".
#   SCIP_INDEXER        — optional. "scip-clang" → installs the indexer
#                         at `${SCIP_CLANG_VERSION}` (default `latest`)
#                         from the official GitHub release. Linux x86_64
#                         only — upstream does not ship arm64 binaries;
#                         on arm64 we log a warning and skip.
#   SCIP_CLANG_VERSION  — optional. Pin scip-clang to a specific tag,
#                         e.g. `v0.3.4`.
set -euo pipefail

: "${CLANG_VERSION:?CLANG_VERSION is required, e.g. \"18\"}"

arch="$(uname -m)"

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

if [ "${SCIP_INDEXER:-}" = "scip-clang" ]; then
    if [ "${arch}" != "x86_64" ]; then
        echo "[install-clang] scip-clang has no upstream arm64 build — skipping" >&2
    else
        sc_version="${SCIP_CLANG_VERSION:-latest}"
        if [ "${sc_version}" = "latest" ]; then
            sc_version="$(curl -fsSL https://api.github.com/repos/sourcegraph/scip-clang/releases/latest \
                | grep -oE '"tag_name"[[:space:]]*:[[:space:]]*"v[0-9.]+"' | head -n1 \
                | sed -E 's/.*"(v[0-9.]+)"/\1/')"
            [ -n "${sc_version}" ] || { echo "[install-clang] could not resolve scip-clang latest" >&2; exit 1; }
        fi
        sc_url="https://github.com/sourcegraph/scip-clang/releases/download/${sc_version}/scip-clang-x86_64-linux"
        curl --proto '=https' --tlsv1.2 -fsSL "${sc_url}" -o /usr/local/bin/scip-clang
        chmod +x /usr/local/bin/scip-clang
    fi
fi
