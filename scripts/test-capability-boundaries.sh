#!/bin/sh
# Capability-boundary guard plumbing self-tests.
#
# Exercises the shared guard end-to-end using synthetic fixture files under the
# repository tree.  Pure POSIX shell; no cargo, no python, no network.
#
# Run from the repository root:
#
#   sh scripts/test-capability-boundaries.sh
#
# Exits 0 on success.  The EXIT trap removes every fixture path and scratch dir.

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
GUARD="$SCRIPT_DIR/check-capability-boundaries.sh"
ALLOWLIST="$SCRIPT_DIR/capability-boundary-allowlist.toml"
FIXTURE_BASE="server/crates/djinn_capability_guard_fixture"

cleanup() {
    if [ -n "${ALLOWLIST_ORIG:-}" ] && [ -f "$ALLOWLIST_ORIG" ]; then
        cp -- "$ALLOWLIST_ORIG" "$ALLOWLIST"
    fi
    rm -rf -- "$REPO_ROOT/$FIXTURE_BASE" 2>/dev/null || true
    if [ -n "${LOG_DIR:-}" ] && [ -d "$LOG_DIR" ]; then
        rm -rf -- "$LOG_DIR"
    fi
}
trap cleanup EXIT INT TERM

if [ ! -f "$GUARD" ]; then
    printf 'FATAL: production guard not found at %s\n' "$GUARD" >&2
    exit 2
fi

PASS=0
FAIL=0

LOG_DIR=$(mktemp -d /var/tmp/djinn-capability-guard-test.XXXXXX 2>/dev/null || \
          mktemp -d "$HOME/.cache/djinn/djinn-capability-guard-test.XXXXXX" 2>/dev/null || \
          mktemp -d "${TMPDIR:-.}/djinn-capability-guard-test.XXXXXX")
if [ ! -d "$LOG_DIR" ]; then
    printf 'FATAL: could not create scratch log dir\n' >&2
    exit 2
fi

pass() {
    PASS=$((PASS + 1))
    printf '  ok   %s\n' "$1"
}

fail() {
    FAIL=$((FAIL + 1))
    printf '  FAIL %s\n' "$1" >&2
    if [ -n "${2:-}" ]; then
        printf '       %s\n' "$2" >&2
    fi
}

# run_guard <label> [paths...]
#
# Pipes the supplied paths (one per line) into the production guard via
# --files-from-stdin mode.  When paths is empty, the guard sees an empty stdin.
# CAPABILITY/OWNER/REMEDIATION/PATTERN are set by the caller.
run_guard() {
    label=$1
    shift

    log="$LOG_DIR/$label.log"
    out="$LOG_DIR/$label.log.out"

    if [ "$#" -eq 0 ]; then
        : > "$log"
    else
        printf '%s\n' "$@" > "$log"
    fi

    cd "$REPO_ROOT" && env \
        CAPABILITY_BOUNDARY_MODE=files-from-stdin \
        sh "$GUARD" --files-from-stdin < "$log" > "$out" 2>&1
    return $?
}

assert_exit() {
    label=$1
    expected=$2
    actual=$3
    log_path=$4

    if [ "$expected" -eq 0 ] && [ "$actual" -eq 0 ]; then
        pass "$label"
    elif [ "$expected" -ne 0 ] && [ "$actual" -ne 0 ]; then
        pass "$label (exit=$actual)"
    else
        fail "$label" "expected exit=$expected, got exit=$actual
output:
$(cat "$log_path")"
    fi
}

assert_output_contains() {
    label=$1
    needle=$2
    log_path=$3

    if grep -q -- "$needle" "$log_path"; then
        pass "$label"
    else
        fail "$label" "expected output to contain '$needle'
actual output:
$(cat "$log_path")"
    fi
}

assert_output_lacks() {
    label=$1
    needle=$2
    log_path=$3

    if grep -q -- "$needle" "$log_path"; then
        fail "$label" "expected output to NOT contain '$needle'
actual output:
$(cat "$log_path")"
    else
        pass "$label"
    fi
}

# Always start from a clean slate.
rm -rf -- "$REPO_ROOT/$FIXTURE_BASE"

printf '== running self-tests for scripts/check-capability-boundaries.sh ==\n'

# Common environment for a "git" capability guard.
export CAPABILITY=git
export OWNER=server/crates/djinn-git
export REMEDIATION=djinn-git
export PATTERN='(git2::|use git2|Command::new\("git"\)|tokio::process::Command::new\("git"\))'

# ── T1: empty stdin exits 0 ───────────────────────────────────────────
set +e
run_guard t1_empty
t1_actual=$?
set -e
assert_exit "T1 empty stdin exits 0" 0 "$t1_actual" "$LOG_DIR/t1_empty.log.out"
assert_output_contains "T1 reports no violations" \
    "no git boundary violations" "$LOG_DIR/t1_empty.log.out"

# ── T2: non-Rust files are ignored ────────────────────────────────────
set +e
run_guard t2_non_rust \
    "scripts/check-capability-boundaries.sh" \
    "docs/architecture.md" \
    "ui/src/api/client.ts"
t2_actual=$?
set -e
assert_exit "T2 non-Rust files exit 0" 0 "$t2_actual" "$LOG_DIR/t2_non_rust.log.out"

# ── T3: nonexistent files are skipped ─────────────────────────────────
set +e
run_guard t3_nonexistent \
    "server/crates/fake-crate/src/does_not_exist.rs"
t3_actual=$?
set -e
assert_exit "T3 nonexistent file exits 0" 0 "$t3_actual" "$LOG_DIR/t3_nonexistent.log.out"

# ── T4: violation — git2:: outside owner crate ────────────────────────
FIXTURE_VIOLATION="$REPO_ROOT/$FIXTURE_BASE/src/lib.rs"
mkdir -p -- "$(dirname -- "$FIXTURE_VIOLATION")"
cat > "$FIXTURE_VIOLATION" <<'FIXTURE'
use git2::Repository;

pub fn open_repo(path: &str) -> git2::Repository {
    git2::Repository::open(path).unwrap()
}
FIXTURE

VIOLATION_PATH="$FIXTURE_BASE/src/lib.rs"
set +e
run_guard t4_violation "$VIOLATION_PATH"
t4_actual=$?
set -e
assert_exit "T4 git2:: violation exits non-zero" 1 "$t4_actual" "$LOG_DIR/t4_violation.log.out"
assert_output_contains "T4 reports the violating file" \
    "file=$FIXTURE_BASE/src/lib.rs,line=4" "$LOG_DIR/t4_violation.log.out"
assert_output_contains "T4 mentions remediation owner" \
    "Remediation owner: djinn-git" "$LOG_DIR/t4_violation.log.out"

# ── T5: Command::new("git") violation ─────────────────────────────────
FIXTURE_CMD="$REPO_ROOT/$FIXTURE_BASE/src/cmd.rs"
cat > "$FIXTURE_CMD" <<'FIXTURE'
use std::process::Command;

pub fn git_status() {
    let _ = Command::new("git").arg("status").output();
}
FIXTURE

CMD_PATH="$FIXTURE_BASE/src/cmd.rs"
set +e
run_guard t5_cmd "$CMD_PATH"
t5_actual=$?
set -e
assert_exit "T5 Command::new(git) violation exits non-zero" 1 "$t5_actual" "$LOG_DIR/t5_cmd.log.out"
assert_output_contains "T5 reports Command::new(git)" \
    'Command::new("git")' "$LOG_DIR/t5_cmd.log.out"

# ── T6: comment-only match is ignored ─────────────────────────────────
FIXTURE_COMMENT="$REPO_ROOT/$FIXTURE_BASE/src/comment.rs"
cat > "$FIXTURE_COMMENT" <<'FIXTURE'
// use git2::Repository;

pub fn noop() {}
FIXTURE

COMMENT_PATH="$FIXTURE_BASE/src/comment.rs"
set +e
run_guard t6_comment "$COMMENT_PATH"
t6_actual=$?
set -e
assert_exit "T6 comment-only match exits 0" 0 "$t6_actual" "$LOG_DIR/t6_comment.log.out"
assert_output_lacks "T6 does not flag comment" \
    "file=$COMMENT_PATH" "$LOG_DIR/t6_comment.log.out"

# ── T7: owner-crate path is exempted ──────────────────────────────────
OWNER_PATH="server/crates/djinn-git/src/lib.rs"
set +e
run_guard t7_owner "$OWNER_PATH"
t7_actual=$?
set -e
assert_exit "T7 owner crate path exits 0" 0 "$t7_actual" "$LOG_DIR/t7_owner.log.out"

# ── T8: allowlist exempts an exact path+matcher ──────────────────────
FIXTURE_ALLOWED="$REPO_ROOT/$FIXTURE_BASE/src/allowed.rs"
cat > "$FIXTURE_ALLOWED" <<'FIXTURE'
use git2::Repository;

pub fn allowed_repo(path: &str) -> git2::Repository {
    git2::Repository::open(path).unwrap()
}
FIXTURE

ALLOWED_PATH="$FIXTURE_BASE/src/allowed.rs"

ALLOWLIST_ORIG="$LOG_DIR/allowlist.orig"
cp -- "$ALLOWLIST" "$ALLOWLIST_ORIG"
cat >> "$ALLOWLIST" <<EOF

[[entries]]
capability = "git"
path = "$ALLOWED_PATH"
matcher = "git2::"
owner = "team/capability-guard-self-test"
rationale = "Synthetic allowlist fixture for the shared guard self-test."
expires = "2099-12-31"
EOF

set +e
run_guard t8_allowlisted "$ALLOWED_PATH"
t8_actual=$?
set -e
assert_exit "T8 allowlisted file exits 0" 0 "$t8_actual" "$LOG_DIR/t8_allowlisted.log.out"
assert_output_lacks "T8 does not flag allowlisted file" \
    "file=$ALLOWED_PATH" "$LOG_DIR/t8_allowlisted.log.out"

# ── T9: allowlist rejects broad globs as config errors ──────────────
# Temporarily swap the allowlist to one containing a forbidden broad glob.
BAD_ALLOWLIST="$LOG_DIR/bad-allowlist.toml"
cat > "$BAD_ALLOWLIST" <<'EOF'
[[entries]]
capability = "git"
path = "server/crates/**"
matcher = "git2::"
owner = "team/test"
rationale = "Broad glob should be rejected."
expires = "2099-12-31"
EOF

set +e
cd "$REPO_ROOT" && env \
    CAPABILITY=git OWNER=server/crates/djinn-git REMEDIATION=djinn-git \
    PATTERN='(git2::|use git2)' \
    CAPABILITY_BOUNDARY_MODE=files-from-stdin \
    ALLOWLIST="$BAD_ALLOWLIST" \
    sh "$GUARD" --files-from-stdin < /dev/null > "$LOG_DIR/t9_broad_glob.log.out" 2>&1
t9_actual=$?
set -e
assert_exit "T9 broad allowlist glob exits 2" 2 "$t9_actual" "$LOG_DIR/t9_broad_glob.log.out"
assert_output_contains "T9 reports forbidden broad glob" \
    "forbidden broad glob" "$LOG_DIR/t9_broad_glob.log.out"

# ── T10: allowlist rejects missing required fields ────────────────────
BAD_ALLOWLIST2="$LOG_DIR/bad-allowlist2.toml"
cat > "$BAD_ALLOWLIST2" <<'EOF'
[[entries]]
capability = "git"
path = "server/crates/foo/src/lib.rs"
owner = "team/test"
rationale = "Missing matcher and expiration."
EOF

set +e
cd "$REPO_ROOT" && env \
    CAPABILITY=git OWNER=server/crates/djinn-git REMEDIATION=djinn-git \
    PATTERN='(git2::|use git2)' \
    CAPABILITY_BOUNDARY_MODE=files-from-stdin \
    ALLOWLIST="$BAD_ALLOWLIST2" \
    sh "$GUARD" --files-from-stdin < /dev/null > "$LOG_DIR/t10_missing_fields.log.out" 2>&1
t10_actual=$?
set -e
assert_exit "T10 missing required fields exits 2" 2 "$t10_actual" "$LOG_DIR/t10_missing_fields.log.out"

# ── T11: --help exits 0 ───────────────────────────────────────────────
set +e
cd "$REPO_ROOT" && env \
    CAPABILITY=git OWNER=server/crates/djinn-git REMEDIATION=djinn-git \
    PATTERN='git2::' sh "$GUARD" --help > "$LOG_DIR/t11_help.log.out" 2>&1
t11_actual=$?
set -e
assert_exit "T11 --help exits 0" 0 "$t11_actual" "$LOG_DIR/t11_help.log.out"
assert_output_contains "T11 help mentions files-from-stdin" \
    "files-from-stdin" "$LOG_DIR/t11_help.log.out"

# ── T12: mixed files — violation + clean + owner + comment ────────────
set +e
run_guard t12_mixed "$VIOLATION_PATH" "$COMMENT_PATH" "$OWNER_PATH"
t12_actual=$?
set -e
assert_exit "T12 mixed files exits non-zero (violation present)" 1 "$t12_actual" "$LOG_DIR/t12_mixed.log.out"
assert_output_contains "T12 reports violation file" \
    "file=$FIXTURE_BASE/src/lib.rs" "$LOG_DIR/t12_mixed.log.out"
assert_output_lacks "T12 does not flag owner path" \
    "file=$OWNER_PATH" "$LOG_DIR/t12_mixed.log.out"
assert_output_lacks "T12 does not flag comment file" \
    "file=$COMMENT_PATH" "$LOG_DIR/t12_mixed.log.out"

# ── T13: synthetic fixture glob is allowed in allowlist ───────────────
GOOD_FIXTURE_ALLOWLIST="$LOG_DIR/good-fixture-allowlist.toml"
cat > "$GOOD_FIXTURE_ALLOWLIST" <<'EOF'
[[entries]]
capability = "git"
path = "server/crates/djinn_capability_guard_fixture/**"
matcher = "git2::"
owner = "team/test"
rationale = "Synthetic fixture glob is permitted by self-tests."
expires = "2099-12-31"
EOF

set +e
cd "$REPO_ROOT" && env \
    CAPABILITY=git OWNER=server/crates/djinn-git REMEDIATION=djinn-git \
    PATTERN='(git2::|use git2)' \
    CAPABILITY_BOUNDARY_MODE=files-from-stdin \
    ALLOWLIST="$GOOD_FIXTURE_ALLOWLIST" \
    sh "$GUARD" --files-from-stdin < /dev/null > "$LOG_DIR/t13_fixture_glob.log.out" 2>&1
t13_actual=$?
set -e
assert_exit "T13 synthetic fixture glob exits 0" 0 "$t13_actual" "$LOG_DIR/t13_fixture_glob.log.out"

# ── summary ────────────────────────────────────────────────────────────
printf -- '------------------------------------------\n'
printf 'passed: %d   failed: %d\n' "$PASS" "$FAIL"

if [ "$FAIL" -ne 0 ]; then
    exit 1
fi
exit 0
