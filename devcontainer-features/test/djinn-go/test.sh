#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=/dev/null
source dev-container-features-test-lib

check "go on PATH" bash -c 'command -v go'
check "gopls installed" bash -c 'command -v gopls'
check "scip-go installed" bash -c 'command -v scip-go'

reportResults
