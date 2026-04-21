#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=/dev/null
source dev-container-features-test-lib

check "python on PATH" bash -c 'command -v python || command -v python3'
check "pyright installed" bash -c 'command -v pyright'
check "scip-python installed" bash -c 'command -v scip-python'
check "uv installed (default pm)" bash -c 'command -v uv'

reportResults
