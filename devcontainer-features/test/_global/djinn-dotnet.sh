#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=/dev/null
source dev-container-features-test-lib
# shellcheck source=./lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

assert_worker_present
check "dotnet on PATH" bash -c 'command -v dotnet'
check "dotnet sdk listed" bash -c 'dotnet --list-sdks | grep -q .'

reportResults
