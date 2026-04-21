#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=/dev/null
source dev-container-features-test-lib

check "dotnet on PATH" bash -c 'command -v dotnet'
check "dotnet sdk listed" bash -c 'dotnet --list-sdks | grep -q .'

reportResults
