#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=/dev/null
source dev-container-features-test-lib

check "java on PATH" bash -c 'command -v java'
check "javac on PATH" bash -c 'command -v javac'
check "gradle on PATH" bash -c 'command -v gradle'
check "jdtls wrapper present" bash -c 'command -v jdtls'
check "scip-java wrapper present" bash -c 'command -v scip-java'

reportResults
