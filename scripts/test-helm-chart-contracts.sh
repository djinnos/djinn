#!/usr/bin/env bash
# Runs every Helm chart contract script in deploy/helm/djinn/tests/.
#
# The directory is globbed, never enumerated: a new contract script dropped in
# there is picked up by this runner (and therefore by CI) with no edit here.
# Any non-zero exit fails the whole run, and every script's output is printed
# so a CI failure is diagnosable from the first log without a rerun.
#
# Usage: bash scripts/test-helm-chart-contracts.sh
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TESTS_DIR="$REPO_ROOT/deploy/helm/djinn/tests"

[ -d "$TESTS_DIR" ] || {
    printf 'FAIL: chart contract directory is missing: %s\n' "$TESTS_DIR" >&2
    exit 1
}

shopt -s nullglob
# Non-recursive on purpose: deploy/helm/djinn/tests/deferred/ holds scripts that
# are blocked on a chart defect this runner must not pretend is fixed. Read that
# directory's own header before adding anything to it.
scripts=("$TESTS_DIR"/*.sh)
shopt -u nullglob

if [ "${#scripts[@]}" -eq 0 ]; then
    printf 'FAIL: no chart contract scripts found in %s\n' "$TESTS_DIR" >&2
    exit 1
fi

names=()
results=()
failures=0
capture=$(mktemp)
trap 'rm -f "$capture"' EXIT

for script in "${scripts[@]}"; do
    name=${script#"$REPO_ROOT/"}
    names+=("$name")

    # Honour each script's own shebang. The suite mixes bash and POSIX sh, and
    # the sh members assign UID, which bash refuses as a read-only variable.
    case "$(head -n 1 "$script")" in
        *bash) interpreter=bash ;;
        *) interpreter=sh ;;
    esac

    if "$interpreter" "$script" >"$capture" 2>&1; then
        results+=("PASS")
        printf '::group::PASS %s\n' "$name"
        cat "$capture"
        printf '::endgroup::\n'
    else
        status=$?
        results+=("FAIL(exit $status)")
        failures=$((failures + 1))
        # Deliberately not grouped: a failing script's output must be visible
        # in a collapsed-by-default CI log without expanding anything.
        printf '\n===== FAIL %s (exit %d) =====\n' "$name" "$status"
        cat "$capture"
        printf '===== end %s =====\n\n' "$name"
    fi
done

printf '\n=== Helm chart contract roster (%d scripts) ===\n' "${#scripts[@]}"
for index in "${!names[@]}"; do
    printf '%-14s %s\n' "${results[$index]}" "${names[$index]}"
done

if [ "$failures" -ne 0 ]; then
    printf '\nFAIL: %d of %d Helm chart contract scripts failed\n' \
        "$failures" "${#scripts[@]}" >&2
    exit 1
fi

printf '\n=== All %d Helm chart contract scripts passed ===\n' "${#scripts[@]}"
