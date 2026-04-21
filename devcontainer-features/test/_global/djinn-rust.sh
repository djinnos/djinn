#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=/dev/null
source dev-container-features-test-lib
# shellcheck source=./lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

assert_worker_present
check "rustup on PATH" bash -c 'command -v rustup'
check "cargo on PATH" bash -c 'command -v cargo'
check "rust-analyzer component present" bash -c 'rustup which rust-analyzer'

reportResults
