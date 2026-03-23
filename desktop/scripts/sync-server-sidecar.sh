#!/usr/bin/env bash
set -euo pipefail

# Resolve paths relative to the desktop/ directory (where this script is run from).
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DESKTOP_DIR="$(dirname "${SCRIPT_DIR}")"
REPO_ROOT="$(dirname "${DESKTOP_DIR}")"
SERVER_DIR="${REPO_ROOT}/server"

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

if [[ ! -f "${SERVER_DIR}/Cargo.toml" ]]; then
  echo "Server source not found at ${SERVER_DIR}" >&2
  exit 1
fi

cargo build \
  --manifest-path "${SERVER_DIR}/Cargo.toml" \
  --release \
  --target "${TARGET_TRIPLE}" \
  --bin "${BIN_NAME}"

mkdir -p src-tauri/binaries
cp "${SERVER_DIR}/target/${TARGET_TRIPLE}/release/${BIN_NAME}${EXT}" \
  "src-tauri/binaries/${BIN_NAME}-${TARGET_TRIPLE}${EXT}"

echo "Synced sidecar: src-tauri/binaries/${BIN_NAME}-${TARGET_TRIPLE}${EXT}"
