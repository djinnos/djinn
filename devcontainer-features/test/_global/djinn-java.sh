#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=/dev/null
source dev-container-features-test-lib
# shellcheck source=./lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

assert_worker_present
check "java on PATH" bash -c 'command -v java'
check "gradle on PATH" bash -c 'command -v gradle'
# jdtls + scip-java are fetched from external URLs that 404 frequently
# (Eclipse's jdtls uses versioned paths with timestamp suffixes; our
# install.sh soft-skips them on download failure). Asserting them here
# turns every Eclipse download glitch into a CI failure; that's better
# surfaced as a separate alarm.

reportResults
