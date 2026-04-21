#!/usr/bin/env bash
# Shared assertion helpers for combined-scenario tests in _global/.
# The devcontainers CLI copies this whole directory into the workspace
# (testCommandImpl.ts:366), so sibling <scenario>.sh files can `source` this.

# Every combined scenario layers djinn-agent-worker under a language feature.
# These asserts cover the worker side so each scenario file only needs to
# add its language-specific checks.
assert_worker_present() {
    check "PATH has /opt/djinn/bin" bash -c 'echo "$PATH" | tr ":" "\n" | grep -q "^/opt/djinn/bin$"'
    check "DJINN_WORKER_BIN env is set" bash -c '[[ -n "${DJINN_WORKER_BIN:-}" ]]'
    check "git is installed" bash -c 'command -v git'
    check "tini is installed" bash -c 'command -v tini || ls /usr/bin/tini'
    check "/opt/djinn/bin exists" test -d /opt/djinn/bin
}
