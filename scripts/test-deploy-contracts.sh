#!/usr/bin/env bash
# Runs the deploy contract suites that live OUTSIDE deploy/helm/djinn/tests/.
#
# scripts/test-helm-chart-contracts.sh globs exactly one directory, so its
# "drop a script in and CI runs it" guarantee stopped at the chart's own tests.
# Five suites under deploy/node/k3s/tests/, deploy/runbooks/tests/ and
# deploy/preflight/tests/ were written, reviewed, merged — and then never
# executed by CI even once. One of them
# (deploy/runbooks/tests/bzpt-production-activation.sh) was an acceptance
# criterion of proposal d308. This runner is the missing half of that
# guarantee.
#
# The directories are globbed, never enumerated: a new contract script dropped
# into one of them is picked up here (and therefore by CI) with no edit to this
# file. Any non-zero exit fails the whole run, and every script's output is
# printed so a CI failure is diagnosable from the first log without a rerun.
#
# Usage:
#   bash scripts/test-deploy-contracts.sh              # the shell-only suites
#   bash scripts/test-deploy-contracts.sh DIR [DIR...] # explicit directories
#
# With no arguments this runs the suites that need nothing but bash and the
# repository checkout, which is why the local-dev-contracts CI lane can host
# them. deploy/preflight/tests/ is NOT in that default set: it builds
# server/crates/djinn-k8s's render-gate binary and shells out to `helm`, so it
# is passed explicitly by a lane that already has a warm Rust toolchain. It is
# still a globbed DIRECTORY there, not an enumerated file — same guarantee.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Adding a directory here commits the local-dev-contracts lane to running it.
#
# deploy/node/k3s/tests and deploy/runbooks/tests are shell-only. deploy/kueue/
# tests is NOT: since the byte-vendored Kueue fork was replaced by a pinned
# upstream chart, its contract has to render deploy/helm/djinn-prereqs, so it
# needs `helm` and python3 with PyYAML. That is deliberate — validating a static
# file instead of the rendered artifact is the defect the rewrite removed. The
# hosting lane already installs both (it runs `helm lint` and the chart
# contracts in the same job), and the suite FAILS rather than skips if either
# tool is missing.
#
# deploy/helm/djinn-prereqs/tests has the same needs as deploy/kueue/tests and
# for the same reason: it renders the wrapper chart. It lives next to the chart
# rather than under deploy/kueue/tests because it asserts chart VALUES (the
# Kueue feature gates that set the Kubernetes floor), not the cutover scoping.
DEFAULT_DIRS=(
    deploy/helm/djinn-prereqs/tests
    deploy/kueue/tests
    deploy/node/k3s/tests
    deploy/runbooks/tests
)

if [ "$#" -gt 0 ]; then
    requested=("$@")
else
    requested=("${DEFAULT_DIRS[@]}")
fi

scripts=()
for requested_dir in "${requested[@]}"; do
    # Repo-relative is the intended spelling (that is what CI passes), but an
    # absolute path must resolve to itself rather than be silently concatenated
    # onto the repo root into a nonsense path that only fails as "missing".
    case "$requested_dir" in
        /*) tests_dir="$requested_dir" ;;
        *) tests_dir="$REPO_ROOT/$requested_dir" ;;
    esac

    [ -d "$tests_dir" ] || {
        printf 'FAIL: deploy contract directory is missing: %s\n' "$tests_dir" >&2
        exit 1
    }

    shopt -s nullglob
    # Non-recursive, matching scripts/test-helm-chart-contracts.sh: a
    # subdirectory is a deliberate quarantine, not a place to hide work from
    # the runner. If a suite needs one, give it its own top-level directory.
    found=("$tests_dir"/*.sh)
    shopt -u nullglob

    # An empty roster is the exact failure this runner exists to fix. A runner
    # that reports success because it globbed nothing is worse than no runner:
    # it manufactures the evidence that the suites ran.
    if [ "${#found[@]}" -eq 0 ]; then
        printf 'FAIL: no deploy contract scripts found in %s\n' "$tests_dir" >&2
        exit 1
    fi

    scripts+=("${found[@]}")
done

names=()
results=()
failures=0
capture=$(mktemp)
trap 'rm -f "$capture"' EXIT

for script in "${scripts[@]}"; do
    name=${script#"$REPO_ROOT/"}
    names+=("$name")

    # Honour each script's own shebang rather than assuming bash: these suites
    # are independent of each other and nothing stops a POSIX sh one landing.
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

printf '\n=== Deploy contract roster (%d scripts) ===\n' "${#scripts[@]}"
for index in "${!names[@]}"; do
    printf '%-14s %s\n' "${results[$index]}" "${names[$index]}"
done

if [ "$failures" -ne 0 ]; then
    printf '\nFAIL: %d of %d deploy contract scripts failed\n' \
        "$failures" "${#scripts[@]}" >&2
    exit 1
fi

printf '\n=== All %d deploy contract scripts passed ===\n' "${#scripts[@]}"
