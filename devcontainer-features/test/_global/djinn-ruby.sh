#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=/dev/null
source dev-container-features-test-lib
# shellcheck source=./lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

assert_worker_present
check "rbenv on PATH" bash -c 'command -v rbenv'
check "ruby on PATH" bash -c 'command -v ruby'
check "ruby-lsp gem installed" bash -c 'gem list -i ruby-lsp'

reportResults
