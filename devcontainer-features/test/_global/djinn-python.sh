#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=/dev/null
source dev-container-features-test-lib
# shellcheck source=./lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

assert_worker_present
check "python on PATH" bash -c 'command -v python || command -v python3'
# pyright + scip-python are Node-based tools installed via `npm install
# -g`. The djinn-python install.sh soft-skips them when npm is absent,
# and this scenario doesn't stack djinn-typescript. Leave those checks
# to the djinn-typescript+djinn-python combination scenario if/when we
# add one.
check "uv installed (default pm)" bash -c 'command -v uv'

reportResults
