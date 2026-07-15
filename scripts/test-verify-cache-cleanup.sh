#!/usr/bin/env bash
# Focused self-tests for the cache-cleanup aggregate verifier. These use a
# synthetic tracked inventory and tool shims; no Helm chart or Cargo suite runs.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VERIFIER="$SCRIPT_DIR/verify-cache-cleanup.sh"
SCRATCH="$(mktemp -d "${TMPDIR:-/var/tmp}/test-verify-cache-cleanup.XXXXXX")"
trap 'rm -rf "$SCRATCH"' EXIT

[[ -f "$VERIFIER" ]] || { printf 'FATAL: verifier not found: %s\n' "$VERIFIER" >&2; exit 2; }

required_inventory=(
    "deploy/helm/djinn/tests/cache-cleanup-render.sh"
    "deploy/helm/djinn/tests/fixtures/cache-cleanup/invalid.env"
    "docs/SHARED_CACHE_CLEANUP_ROLLOUT.md"
    "docs/SHARED_CACHE_CLEANUP_RUNBOOK.md"
    "server/crates/djinn-coordinator/src/cargo_warm_base_gc/tests/pressure_execution.rs"
    "server/crates/djinn-coordinator/tests/fixtures/cache_cleanup/three_rung_pressure.json"
    "server/crates/djinn-core/tests/fixtures/cargo_target_runs/conjunctive_both_caps/expected.json"
    "server/crates/djinn-core/tests/fixtures/cargo_target_runs/conjunctive_both_caps/scenario.json"
    "server/crates/djinn-telemetry/tests/fixtures/cache_cleanup/expected_metrics.json"
)

make_fixture_repo() {
    local repo=$1 path
    mkdir -p "$repo/scripts/fixtures/cache-cleanup" "$repo/shims" "$repo/server"
    cp "$VERIFIER" "$repo/scripts/verify-cache-cleanup.sh"
    for path in "${required_inventory[@]}"; do
        mkdir -p "$repo/$(dirname "$path")"
        printf 'fixture: %s\n' "$path" > "$repo/$path"
    done
    cat > "$repo/docs/SHARED_CACHE_CLEANUP_ROLLOUT.md" <<'EOF'
cacheCleanup.mode=delete
--set cacheCleanup.mode=dry_run
DJINN_CACHE_CLEANUP_MODE fails safe to `dry_run`
EOF
    cat > "$repo/docs/SHARED_CACHE_CLEANUP_RUNBOOK.md" <<'EOF'
cacheCleanup.mode` defaults to `delete`
DJINN_CACHE_CLEANUP_MODE uses the fail-safe `dry_run` mode
conjunctive shared-lock 0.15 0.25 8589934592
EOF
    cat > "$repo/deploy/helm/djinn/tests/cache-cleanup-render.sh" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
    chmod +x "$repo/deploy/helm/djinn/tests/cache-cleanup-render.sh"
    (
        cd "$repo"
        : > scripts/fixtures/cache-cleanup/manifest.sha256
        for path in "${required_inventory[@]}"; do sha256sum "$path"; done > scripts/fixtures/cache-cleanup/manifest.sha256
        git init -q
        git config user.email verifier-test@example.invalid
        git config user.name verifier-test
        git add .
        git commit -qm fixture
    )
    cat > "$repo/shims/cargo" <<'EOF'
#!/usr/bin/env bash
set -eu
if [[ "${CARGO_MODE:-pass}" == fail ]]; then
    printf 'forced cargo failure\n' >&2
    exit 73
fi
if [[ -n "${VERIFY_MUTATE_PATH:-}" ]]; then
    printf 'mutated by cargo shim\n' >> "$VERIFY_MUTATE_PATH"
fi
if [[ "${CARGO_MODE:-pass}" == zero ]]; then
    printf 'test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n'
else
    printf 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n'
fi
EOF
    cat > "$repo/shims/helm" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
    chmod +x "$repo/shims/cargo" "$repo/shims/helm"
}

run_verifier() {
    local name=$1 repo=$2
    shift 2
    local log="$SCRATCH/$name.log"
    if env PATH="$repo/shims:$PATH" "$@" bash "$repo/scripts/verify-cache-cleanup.sh" >"$log" 2>&1; then
        printf 'FAIL: %s unexpectedly succeeded\n' "$name" >&2
        cat "$log" >&2
        return 1
    fi
    printf 'ok: %s\n' "$name"
}

# Hash validation must happen before tools or behavioral commands can hide a
# modified committed member.
repo="$SCRATCH/manifest-tamper"
make_fixture_repo "$repo"
printf 'tampered\n' >> "$repo/docs/SHARED_CACHE_CLEANUP_ROLLOUT.md"
run_verifier manifest-tamper "$repo" env

a="$SCRATCH/missing-inventory"
make_fixture_repo "$a"
sed -i '/invalid\.env/d' "$a/scripts/fixtures/cache-cleanup/manifest.sha256"
run_verifier missing-inventory "$a" env

b="$SCRATCH/subcheck-failure"
make_fixture_repo "$b"
run_verifier forced-subcheck-failure "$b" env CARGO_MODE=fail

c="$SCRATCH/baseline-mutation"
make_fixture_repo "$c"
run_verifier baseline-mutation "$c" env VERIFY_MUTATE_PATH="$c/docs/SHARED_CACHE_CLEANUP_ROLLOUT.md"

# This separately proves the no-skips guard: Cargo's successful zero-test
# output cannot satisfy any focused filtered subcheck.
d="$SCRATCH/zero-test-filter"
make_fixture_repo "$d"
run_verifier zero-test-filter "$d" env CARGO_MODE=zero

printf 'PASS: verifier failure-path self-tests completed\n'
