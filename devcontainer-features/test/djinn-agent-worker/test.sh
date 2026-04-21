#!/usr/bin/env bash
# Smoke test for djinn-agent-worker Feature.
# Runs inside a built devcontainer.

set -euo pipefail

# Dev Container Features test library.
# shellcheck source=/dev/null
source dev-container-features-test-lib

check "PATH has /opt/djinn/bin" bash -c 'echo "$PATH" | tr ":" "\n" | grep -q "^/opt/djinn/bin$"'
check "DJINN_WORKER_BIN env is set" bash -c '[[ -n "${DJINN_WORKER_BIN:-}" ]]'
check "git is installed" bash -c 'command -v git'
check "bash is installed" bash -c 'command -v bash'
check "tini is installed" bash -c 'command -v tini || ls /usr/bin/tini'
# Worker binary may be absent in CI when bin/ is not yet populated — we only
# verify the directory was provisioned.
check "/opt/djinn/bin exists" test -d /opt/djinn/bin

reportResults
