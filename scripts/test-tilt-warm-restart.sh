#!/usr/bin/env bash

set -euo pipefail

SOURCE_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/djinn-tilt-cache.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT INT TERM

FIXTURE="$TMP/repo"
ARTIFACTS="$TMP/artifacts"
BIN_DIR="$TMP/bin"
CALL_LOG="$TMP/calls"

mkdir -p \
    "$ARTIFACTS" \
    "$BIN_DIR" \
    "$FIXTURE/ui/src/nested" \
    "$FIXTURE/ui/public" \
    "$FIXTURE/server/src/nested" \
    "$FIXTURE/server/crates/example/src" \
    "$FIXTURE/server/.cargo" \
    "$FIXTURE/server/.sqlx" \
    "$FIXTURE/server/docker" \
    "$FIXTURE/scripts/tilt"

write_fixture() {
    local path="$1"
    mkdir -p "$(dirname "$path")"
    printf 'fixture for %s\n' "${path#"$FIXTURE"/}" > "$path"
}

for path in \
    ui/src/app.ts \
    ui/src/nested/removable.ts \
    ui/public/icon.svg \
    ui/index.html \
    ui/package.json \
    ui/pnpm-lock.yaml \
    ui/pnpm-workspace.yaml \
    ui/.npmrc \
    ui/tsconfig.app.json \
    ui/tsconfig.json \
    ui/tsconfig.node.json \
    ui/vite.config.ts \
    server/src/main.rs \
    server/src/nested/removable.rs \
    server/crates/example/src/lib.rs \
    server/.cargo/config.toml \
    server/.sqlx/query-fixture.json \
    server/Cargo.toml \
    server/Cargo.lock \
    server/rust-toolchain.toml \
    server/build.rs \
    server/docker/djinn-binaries-builder.Dockerfile
do
    write_fixture "$FIXTURE/$path"
done

cp \
    "$SOURCE_ROOT/scripts/tilt/build-binaries.sh" \
    "$SOURCE_ROOT/scripts/tilt/build-ui.sh" \
    "$SOURCE_ROOT/scripts/tilt/input-fingerprint.sh" \
    "$FIXTURE/scripts/tilt/"

cat > "$BIN_DIR/pnpm" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'pnpm %s\n' "$*" >> "$CALL_LOG"
if [[ "${FAIL_PNPM:-0}" == "1" ]]; then
    exit 17
fi
if [[ "${1:-}" == "build" ]]; then
    if [[ "${TERM_PARENT_PNPM:-0}" == "1" ]]; then
        kill -TERM "$PPID"
        exit 0
    fi
    sleep "${PNPM_SLEEP:-0}"
    mkdir -p dist/assets
    printf '<!doctype html><title>fixture</title>\n' > dist/index.html
    cp src/app.ts dist/assets/app.js
fi
EOF
chmod +x "$BIN_DIR/pnpm"

cat > "$BIN_DIR/docker" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'docker %s\n' "$*" >> "$CALL_LOG"
if [[ "${FAIL_DOCKER:-0}" == "1" ]]; then
    exit 42
fi

if [[ "${1:-}" == "build" ]]; then
    args=("$@")
    for ((index = 0; index < ${#args[@]}; index++)); do
        if [[ "${args[$index]}" == "--iidfile" ]]; then
            printf 'sha256:fixture-builder\n' > "${args[$((index + 1))]}"
        fi
    done
fi

if [[ "${1:-} ${2:-} ${3:-}" == "image inspect --format" ]]; then
    printf 'sha256:fixture-custom-builder\n'
fi

if [[ "${1:-}" == "run" ]]; then
    for arg in "$@"; do
        case "$arg" in
            *:/out)
                output_dir="${arg%:/out}"
                printf 'server fixture\n' > "$output_dir/djinn-server"
                printf 'worker fixture\n' > "$output_dir/djinn-agent-worker"
                chmod +x "$output_dir/djinn-server" "$output_dir/djinn-agent-worker"
                ;;
        esac
    done
fi
EOF
chmod +x "$BIN_DIR/docker"

assert_contains() {
    local label="$1"
    local needle="$2"
    local file="$3"
    if ! grep -Fq -- "$needle" "$file"; then
        echo "not ok: $label (missing $needle)" >&2
        exit 1
    fi
    echo "ok: $label"
}

assert_equal() {
    local label="$1"
    local expected="$2"
    local actual="$3"
    if [[ "$expected" != "$actual" ]]; then
        echo "not ok: $label (expected $expected, got $actual)" >&2
        exit 1
    fi
    echo "ok: $label"
}

run_ui() {
    REPO_ROOT="$FIXTURE" \
    ARTIFACTS_DIR="$ARTIFACTS" \
    PNPM_BIN="$BIN_DIR/pnpm" \
    CALL_LOG="$CALL_LOG" \
    FAIL_PNPM="${FAIL_PNPM:-0}" \
    PNPM_SLEEP="${PNPM_SLEEP:-0}" \
    TERM_PARENT_PNPM="${TERM_PARENT_PNPM:-0}" \
        bash "$FIXTURE/scripts/tilt/build-ui.sh" "$@"
}

run_binaries() {
    REPO_ROOT="$FIXTURE" \
    ARTIFACTS_DIR="$ARTIFACTS" \
    DOCKER_BIN="$BIN_DIR/docker" \
    CALL_LOG="$CALL_LOG" \
    FAIL_DOCKER="${FAIL_DOCKER:-0}" \
    CARGO_REGISTRY_VOLUME="${CARGO_REGISTRY_VOLUME:-djinn-cargo-registry}" \
    TARGET_VOLUME="${TARGET_VOLUME:-djinn-cargo-target}" \
    SCCACHE_VOLUME="${SCCACHE_VOLUME:-djinn-sccache}" \
        bash "$FIXTURE/scripts/tilt/build-binaries.sh" "$@"
}

# UI: a cold build records a fingerprint, and an unchanged invocation reuses it.
: > "$CALL_LOG"
run_ui
[[ -f "$ARTIFACTS/.ui-inputs.fingerprint" ]]
[[ -f "$FIXTURE/ui/dist/index.html" ]]
assert_contains 'cold UI build invokes pnpm' 'pnpm build' "$CALL_LOG"

: > "$CALL_LOG"
ui_output="$(run_ui)"
assert_equal 'unchanged UI inputs reuse dist' '' "$(<"$CALL_LOG")"
if [[ "$ui_output" != *'reusing ui/dist'* ]]; then
    echo 'not ok: unchanged UI build did not report reuse' >&2
    exit 1
fi

# Content beats mtimes: an old-dated edit must still invalidate.
printf 'changed despite old mtime\n' > "$FIXTURE/ui/src/app.ts"
touch -t 200001010000 "$FIXTURE/ui/src/app.ts"
: > "$CALL_LOG"
run_ui
assert_contains 'old-mtime UI edit rebuilds' 'pnpm build' "$CALL_LOG"

# Additions and deletions both change the canonical path/content manifest.
write_fixture "$FIXTURE/ui/src/added.ts"
: > "$CALL_LOG"
run_ui
assert_contains 'added UI input rebuilds' 'pnpm build' "$CALL_LOG"

rm "$FIXTURE/ui/src/nested/removable.ts"
: > "$CALL_LOG"
run_ui
assert_contains 'deleted UI input rebuilds' 'pnpm build' "$CALL_LOG"

# Missing outputs cannot take the warm path even when the inputs match.
rm "$FIXTURE/ui/dist/index.html"
: > "$CALL_LOG"
run_ui
assert_contains 'missing UI output rebuilds' 'pnpm build' "$CALL_LOG"

# The full output tree is protected, not just index.html.
printf 'corrupt asset\n' > "$FIXTURE/ui/dist/assets/app.js"
: > "$CALL_LOG"
run_ui
assert_contains 'corrupted UI asset rebuilds' 'pnpm build' "$CALL_LOG"

rm "$FIXTURE/ui/dist/assets/app.js"
: > "$CALL_LOG"
run_ui
assert_contains 'deleted UI asset rebuilds' 'pnpm build' "$CALL_LOG"

# Failed builds retain the last successful fingerprint and retry next time.
ui_fingerprint="$(<"$ARTIFACTS/.ui-inputs.fingerprint")"
printf 'failure probe\n' >> "$FIXTURE/ui/src/app.ts"
: > "$CALL_LOG"
if FAIL_PNPM=1 run_ui >/dev/null 2>&1; then
    echo 'not ok: failed UI build unexpectedly succeeded' >&2
    exit 1
fi
assert_equal 'failed UI build preserves fingerprint' \
    "$ui_fingerprint" "$(<"$ARTIFACTS/.ui-inputs.fingerprint")"
: > "$CALL_LOG"
run_ui
assert_contains 'UI retry rebuilds after failure' 'pnpm build' "$CALL_LOG"

# Concurrent triggers serialize the shared ui/dist transaction. The waiter
# recomputes state after acquiring the lock and reuses the first build.
printf 'concurrency probe\n' >> "$FIXTURE/ui/src/app.ts"
: > "$CALL_LOG"
PNPM_SLEEP=1 run_ui > "$TMP/ui-concurrent-1.log" &
first_ui_pid=$!
PNPM_SLEEP=1 run_ui > "$TMP/ui-concurrent-2.log" &
second_ui_pid=$!
wait "$first_ui_pid"
wait "$second_ui_pid"
assert_equal 'concurrent UI triggers perform one build' \
    '1' "$(grep -Fc 'pnpm build' "$CALL_LOG")"

# Binary cache: establish a successful fixture build, then prove reuse.
: > "$CALL_LOG"
run_binaries
[[ -x "$ARTIFACTS/djinn-server" ]]
[[ -x "$ARTIFACTS/djinn-agent-worker" ]]
[[ -f "$ARTIFACTS/.binaries-inputs.fingerprint" ]]
assert_contains 'cold binary build invokes Docker' 'docker build' "$CALL_LOG"

: > "$CALL_LOG"
binary_output="$(run_binaries)"
assert_equal 'unchanged binary inputs avoid Docker' '' "$(<"$CALL_LOG")"
if [[ "$binary_output" != *'reusing staged djinn binaries'* ]]; then
    echo 'not ok: unchanged binary build did not report reuse' >&2
    exit 1
fi

# Executable existence is insufficient: a modified artifact must rebuild.
printf 'corrupt binary\n' >> "$ARTIFACTS/djinn-server"
: > "$CALL_LOG"
run_binaries
assert_contains 'corrupted binary artifact rebuilds' 'docker build' "$CALL_LOG"

# Inputs that were missing from the old mtime implementation stay covered.
printf 'changed sqlx metadata\n' >> "$FIXTURE/server/.sqlx/query-fixture.json"
: > "$CALL_LOG"
run_binaries
assert_contains 'SQLx metadata change rebuilds binaries' 'docker build' "$CALL_LOG"

printf 'changed build script\n' >> "$FIXTURE/server/build.rs"
touch -t 200001010000 "$FIXTURE/server/build.rs"
: > "$CALL_LOG"
run_binaries
assert_contains 'old-mtime build.rs change rebuilds binaries' 'docker build' "$CALL_LOG"

printf 'new embedded UI\n' >> "$FIXTURE/ui/src/app.ts"
: > "$CALL_LOG"
run_ui
: > "$CALL_LOG"
run_binaries
assert_contains 'embedded UI output change rebuilds binaries' 'docker build' "$CALL_LOG"

# A caller-selected builder image participates in the input fingerprint.
: > "$CALL_LOG"
BUILDER_IMAGE='example.invalid/djinn-builder:test' run_binaries
assert_contains 'builder image change rebuilds binaries' 'docker image inspect' "$CALL_LOG"

# Returning to the default builder restores the default fingerprint.
: > "$CALL_LOG"
run_binaries
assert_contains 'default builder restoration rebuilds binaries' 'docker build' "$CALL_LOG"

# A deleted Rust input must miss the cache. A failed rebuild must not publish
# the new fingerprint; the following successful retry must publish it.
binary_fingerprint="$(<"$ARTIFACTS/.binaries-inputs.fingerprint")"
rm "$FIXTURE/server/src/nested/removable.rs"
: > "$CALL_LOG"
if FAIL_DOCKER=1 run_binaries >/dev/null 2>&1; then
    echo 'not ok: deleted binary input incorrectly reused cached artifacts' >&2
    exit 1
fi
assert_contains 'deleted binary input reaches Docker' 'docker build' "$CALL_LOG"
assert_equal 'failed binary build preserves fingerprint' \
    "$binary_fingerprint" "$(<"$ARTIFACTS/.binaries-inputs.fingerprint")"

: > "$CALL_LOG"
run_binaries
assert_contains 'binary retry rebuilds after failure' 'docker build' "$CALL_LOG"
if [[ "$binary_fingerprint" == "$(<"$ARTIFACTS/.binaries-inputs.fingerprint")" ]]; then
    echo 'not ok: successful binary rebuild did not advance fingerprint' >&2
    exit 1
fi

# Invalid shared-volume names fail clearly before either reuse or Docker.
if CARGO_REGISTRY_VOLUME='bad/volume' run_binaries >/dev/null 2>&1; then
    echo 'not ok: invalid Docker volume name was accepted' >&2
    exit 1
fi
echo 'ok: invalid Docker volume names fail before reuse'

# Exercise the real UI build's signal handlers. A canceled build must release
# the transaction lock without publishing fingerprints for incomplete output.
ui_fingerprint_before_term="$(<"$ARTIFACTS/.ui-inputs.fingerprint")"
ui_output_fingerprint_before_term="$(<"$ARTIFACTS/.ui-output.fingerprint")"
printf 'signal probe\n' >> "$FIXTURE/ui/src/app.ts"
signal_log="$TMP/signal-build.log"
: > "$CALL_LOG"
set +e
TERM_PARENT_PNPM=1 run_ui > "$signal_log" 2>&1
signal_status=$?
set -e
assert_equal 'TERM exits with signal status' '143' "$signal_status"
assert_contains 'TERM reached the real UI build' 'pnpm build' "$CALL_LOG"
assert_equal 'TERM preserves UI input fingerprint' \
    "$ui_fingerprint_before_term" "$(<"$ARTIFACTS/.ui-inputs.fingerprint")"
assert_equal 'TERM preserves UI output fingerprint' \
    "$ui_output_fingerprint_before_term" "$(<"$ARTIFACTS/.ui-output.fingerprint")"
if [[ -e "$FIXTURE/.tilt/build-transaction.lock" ]] \
    || grep -Fq '==> done:' "$signal_log"; then
    echo 'not ok: TERM published output or retained the transaction lock' >&2
    exit 1
fi
echo 'ok: TERM exits the real UI build without publishing incomplete output'

echo 'all Tilt content-cache assertions passed'
