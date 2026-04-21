#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=/dev/null
source dev-container-features-test-lib

check "rustup on PATH" bash -c 'command -v rustup'
check "rustc on PATH" bash -c 'command -v rustc'
check "cargo on PATH" bash -c 'command -v cargo'
# rust-analyzer is installed as a rustup component; expose via rustup which.
check "rust-analyzer component present" bash -c 'rustup which rust-analyzer'

reportResults
