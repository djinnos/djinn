#!/usr/bin/env bash
# Mutation regressions for the mold Debian snapshot repository contract.
#
# Copies the production guard and its two image-installation inputs into a
# repository-shaped temporary fixture for every case. The cases use no Docker,
# network, installed mold binary, or external service; they only execute the
# contract's static checks against deliberately mutated copies.
#
# Run from the repository root:
#
#   bash scripts/test-mold-debian-snapshot-contract-regressions.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CONTRACT="$SCRIPT_DIR/test-mold-debian-snapshot-contract.sh"
RUST_INSTALLER="$REPO_ROOT/server/crates/djinn-image-builder/scripts/install-rust.sh"
RUNTIME_DOCKERFILE="$REPO_ROOT/server/docker/djinn-agent-runtime-base.Dockerfile"

for required_file in "$CONTRACT" "$RUST_INSTALLER" "$RUNTIME_DOCKERFILE"; do
    [[ -f "$required_file" ]] || {
        printf 'FATAL: required contract input is missing: %s\n' "$required_file" >&2
        exit 2
    }
done

# Prefer disk-backed /var/tmp because some task sandboxes intentionally forbid
# /tmp. The fallback paths also make this usable from ordinary CI and laptops.
FIXTURE_DIR="$(mktemp -d /var/tmp/djinn-mold-snapshot-regressions.XXXXXX 2>/dev/null \
    || mktemp -d "$HOME/.cache/djinn/mold-snapshot-regressions.XXXXXX" 2>/dev/null \
    || mktemp -d "${TMPDIR:-.}/djinn-mold-snapshot-regressions.XXXXXX")"
cleanup() {
    rm -rf -- "$FIXTURE_DIR"
}
trap cleanup EXIT INT TERM

pass_count=0
fail_count=0

pass() {
    pass_count=$((pass_count + 1))
    printf '  ok   %s\n' "$1"
}

fail() {
    fail_count=$((fail_count + 1))
    printf '  FAIL %s\n' "$1" >&2
    if [[ -n "${2:-}" ]]; then
        printf '       %s\n' "$2" >&2
    fi
}

# Make a separate minimal repository-shaped tree so the production guard's
# REPO_ROOT-relative paths exercise exactly the same resolution they use in CI.
make_fixture() {
    local name="$1"
    local fixture="$FIXTURE_DIR/$name/repo"

    mkdir -p "$fixture/scripts" \
        "$fixture/server/crates/djinn-image-builder/scripts" \
        "$fixture/server/docker"
    cp "$CONTRACT" "$fixture/scripts/test-mold-debian-snapshot-contract.sh"
    cp "$RUST_INSTALLER" "$fixture/server/crates/djinn-image-builder/scripts/install-rust.sh"
    cp "$RUNTIME_DOCKERFILE" "$fixture/server/docker/djinn-agent-runtime-base.Dockerfile"
    printf '%s\n' "$fixture"
}

run_case() {
    local label="$1"
    local expected_status="$2"
    local expected_output="$3"
    local fixture="$4"
    local log="$fixture/contract.log"
    local status

    if bash "$fixture/scripts/test-mold-debian-snapshot-contract.sh" >"$log" 2>&1; then
        status=0
    else
        status=$?
    fi

    if [[ "$expected_status" == success && "$status" -eq 0 ]]; then
        pass "$label"
    elif [[ "$expected_status" == failure && "$status" -ne 0 ]]; then
        if grep -Fq -- "$expected_output" "$log"; then
            pass "$label"
        else
            fail "$label (failed without expected diagnostic)" "expected '$expected_output'; output: $(cat "$log")"
        fi
    else
        fail "$label" "expected $expected_status, got exit=$status; output: $(cat "$log")"
    fi
}

# A fixture matching the committed inputs must remain accepted before the
# mutations below can demonstrate a regression in one precise contract clause.
fixture="$(make_fixture committed-valid)"
run_case "accepts committed HTTP dated snapshot and additional source" success "" "$fixture"

fixture="$(make_fixture protocol-drift)"
sed -i 's|ARG DEBIAN_SNAPSHOT_URL=http://|ARG DEBIAN_SNAPSHOT_URL=https://|' \
    "$fixture/server/docker/djinn-agent-runtime-base.Dockerfile"
run_case "rejects cross-path snapshot protocol drift" failure "Debian snapshot drift" "$fixture"

fixture="$(make_fixture date-drift)"
sed -i 's/20250401T000000Z/20250402T000000Z/' \
    "$fixture/server/crates/djinn-image-builder/scripts/install-rust.sh"
run_case "rejects cross-path snapshot date drift" failure "Debian snapshot drift" "$fixture"

fixture="$(make_fixture url-drift)"
sed -i 's|archive/debian/20250401T000000Z|archive/debian-security/20250401T000000Z|' \
    "$fixture/server/docker/djinn-agent-runtime-base.Dockerfile"
run_case "rejects cross-path snapshot URL drift" failure "Debian snapshot drift" "$fixture"

fixture="$(make_fixture source-list-bypass)"
sed -i 's|mold-snapshot\.list|mold-bypass.list|' \
    "$fixture/server/docker/djinn-agent-runtime-base.Dockerfile"
run_case "rejects source command redirected from mold-snapshot.list" failure "alternate apt source configuration" "$fixture"

fixture="$(make_fixture literal-alternate-source)"
sed -i 's|"\$DEBIAN_SNAPSHOT_URL"|"http://snapshot.debian.org/archive/debian/20250402T000000Z"|' \
    "$fixture/server/docker/djinn-agent-runtime-base.Dockerfile"
run_case "rejects literal alternate snapshot source" failure "alternate apt source configuration" "$fixture"

fixture="$(make_fixture version-declaration-drift)"
sed -i 's/ARG MOLD_VERSION=2\.37\.1+dfsg-1/ARG MOLD_VERSION=2.37.2+dfsg-1/' \
    "$fixture/server/docker/djinn-agent-runtime-base.Dockerfile"
run_case "rejects cross-path mold package-version declaration drift" failure "mold version drift" "$fixture"

fixture="$(make_fixture version-install-drift)"
sed -i 's/mold=2\.37\.1+dfsg-1/mold=2.37.2+dfsg-1/' \
    "$fixture/server/docker/djinn-agent-runtime-base.Dockerfile"
run_case "rejects divergent mold package install pin" failure "literal exact version" "$fixture"

fixture="$(make_fixture bare-mold)"
sed -i 's/mold=2\.37\.1+dfsg-1/mold=2.37.1+dfsg-1 mold/' \
    "$fixture/server/crates/djinn-image-builder/scripts/install-rust.sh"
run_case "rejects bare mold apt token" failure "unpinned or divergent mold install" "$fixture"

fixture="$(make_fixture nproc-hint)"
printf '\nexport NPROC=4\n' >> "$fixture/server/crates/djinn-image-builder/scripts/install-rust.sh"
run_case "rejects NPROC hint" failure "forbidden NPROC or MOLD_JOBS hint" "$fixture"

fixture="$(make_fixture mold-jobs-hint)"
printf '\nENV MOLD_JOBS=4\n' >> "$fixture/server/docker/djinn-agent-runtime-base.Dockerfile"
run_case "rejects MOLD_JOBS hint" failure "forbidden NPROC or MOLD_JOBS hint" "$fixture"

if [[ "$fail_count" -ne 0 ]]; then
    printf 'FAIL: %d regression case(s) failed; %d passed\n' "$fail_count" "$pass_count" >&2
    exit 1
fi

printf 'ok: %d mold snapshot contract regression cases passed\n' "$pass_count"
