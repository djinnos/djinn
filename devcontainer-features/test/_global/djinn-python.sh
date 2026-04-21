#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=/dev/null
source dev-container-features-test-lib
# shellcheck source=./lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

assert_worker_present
check "python on PATH" bash -c 'command -v python || command -v python3'
check "pyright installed" bash -c 'command -v pyright'
check "scip-python installed" bash -c 'command -v scip-python'
check "uv installed (default pm)" bash -c 'command -v uv'

reportResults
