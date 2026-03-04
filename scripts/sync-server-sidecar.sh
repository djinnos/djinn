#!/usr/bin/env bash
set -euo pipefail

REMOTE="git@github.com:djinnos/server.git"
BRANCH="main"
CACHE_DIR=".cache/server-src"
BIN_NAME="djinn-server"
TARGET_TRIPLE="${TAURI_ENV_TARGET_TRIPLE:-$(rustc -vV | sed -n 's/^host: //p')}"

if [[ -z "${TARGET_TRIPLE}" ]]; then
  echo "Could not determine target triple" >&2
  exit 1
fi

EXT=""
if [[ "${TARGET_TRIPLE}" == *windows* ]]; then
  EXT=".exe"
fi

if [[ -L "${CACHE_DIR}" ]]; then
  # Symlink to a local server repo — skip clone/fetch, use as-is.
  echo "Using symlinked server source: $(readlink -f "${CACHE_DIR}")"
elif [[ ! -d "${CACHE_DIR}/.git" ]]; then
  mkdir -p .cache
  git clone --depth 1 --branch "${BRANCH}" "${REMOTE}" "${CACHE_DIR}"
else
  git -C "${CACHE_DIR}" remote set-url origin "${REMOTE}"
  git -C "${CACHE_DIR}" fetch --depth 1 origin "${BRANCH}"
  git -C "${CACHE_DIR}" checkout -B "${BRANCH}" "origin/${BRANCH}"
  git -C "${CACHE_DIR}" clean -fdx
fi

cargo build \
  --manifest-path "${CACHE_DIR}/Cargo.toml" \
  --release \
  --target "${TARGET_TRIPLE}" \
  --bin "${BIN_NAME}"

mkdir -p src-tauri/binaries
cp "${CACHE_DIR}/target/${TARGET_TRIPLE}/release/${BIN_NAME}${EXT}" \
  "src-tauri/binaries/${BIN_NAME}-${TARGET_TRIPLE}${EXT}"

echo "Synced sidecar: src-tauri/binaries/${BIN_NAME}-${TARGET_TRIPLE}${EXT}"
