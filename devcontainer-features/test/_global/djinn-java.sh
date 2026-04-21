#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=/dev/null
source dev-container-features-test-lib
# shellcheck source=./lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

assert_worker_present
check "java on PATH" bash -c 'command -v java'
check "gradle on PATH" bash -c 'command -v gradle'
check "jdtls wrapper present" bash -c 'command -v jdtls'
check "scip-java wrapper present" bash -c 'command -v scip-java'

reportResults
