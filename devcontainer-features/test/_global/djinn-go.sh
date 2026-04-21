#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=/dev/null
source dev-container-features-test-lib
# shellcheck source=./lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

assert_worker_present
check "go on PATH" bash -c 'command -v go'
check "gopls installed" bash -c 'command -v gopls'
check "scip-go installed" bash -c 'command -v scip-go'

reportResults
