#!/usr/bin/env bash
# Deterministic contract checks for image-CI helper input validation.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
for bad in 0 -1 nope; do
    if "$ROOT/scripts/image-ci/run-mold-thread-smoke.sh" --threads "$bad" --evidence-dir /dev/null >/dev/null 2>&1; then
        echo "accepted invalid thread count: $bad" >&2
        exit 1
    fi
done
# This fixture-free check ensures the compatibility parser rejects a missing
# installed mold rather than treating absent evidence as compatible.
if PATH=/nonexistent "$ROOT/scripts/image-ci/probe-mold-compatibility.sh" --evidence-dir "$(mktemp -d)" >/dev/null 2>&1; then
    echo 'compatibility probe accepted missing mold' >&2
    exit 1
fi
printf 'ok: image-ci helper input contracts\n'
