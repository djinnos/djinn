#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=/dev/null
source dev-container-features-test-lib
# shellcheck source=./lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

assert_worker_present
check "clang on PATH" bash -c 'command -v clang'
check "clangd on PATH" bash -c 'command -v clangd'
check "scip-clang on PATH" bash -c 'command -v scip-clang'

reportResults
