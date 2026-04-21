#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=/dev/null
source dev-container-features-test-lib

check "rbenv on PATH" bash -c 'command -v rbenv'
check "ruby on PATH" bash -c 'command -v ruby'
check "gem on PATH" bash -c 'command -v gem'
check "ruby-lsp gem installed" bash -c 'gem list -i ruby-lsp'

reportResults
