#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=/dev/null
source dev-container-features-test-lib

check "node on PATH" bash -c 'command -v node'
check "npm on PATH" bash -c 'command -v npm'
check "typescript-language-server installed" bash -c 'command -v typescript-language-server'
check "tsc installed" bash -c 'command -v tsc'
check "scip-typescript installed" bash -c 'command -v scip-typescript'
check "pnpm installed (default pm)" bash -c 'command -v pnpm'

reportResults
