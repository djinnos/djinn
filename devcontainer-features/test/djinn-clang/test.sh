#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=/dev/null
source dev-container-features-test-lib

check "clang on PATH" bash -c 'command -v clang'
check "clangd on PATH" bash -c 'command -v clangd'
check "scip-clang on PATH" bash -c 'command -v scip-clang'

reportResults
