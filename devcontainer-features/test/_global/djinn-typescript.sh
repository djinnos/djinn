#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=/dev/null
source dev-container-features-test-lib
# shellcheck source=./lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

assert_worker_present
check "node on PATH" bash -c 'command -v node'
check "typescript-language-server installed" bash -c 'command -v typescript-language-server'
check "scip-typescript installed" bash -c 'command -v scip-typescript'
check "pnpm installed (default pm)" bash -c 'command -v pnpm'

reportResults
