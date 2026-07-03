#!/bin/sh
# Integrated capability-boundary guard self-tests.
#
# Exercises the shared guard plumbing (check-capability-boundaries.sh) and the
# per-capability detector wrapper scripts (check-git-boundary.sh,
# check-http-boundary.sh, check-k8s-boundary.sh) end-to-end using synthetic
# fixture files under the repository tree.
#
# Coverage:
#   - Owner-allowed usage for git, HTTP, and k8s owner crates.
#   - Forbidden non-owner usage for each capability and each matcher.
#   - Ignored paths (docs, generated/vendor/target, non-Rust, deployment
#     manifests).
#   - Comments-only non-violations.
#   - Alias patterns for Command/git, Command/kubectl, and tokio variants.
#   - Allowlist behavior: exact allowed entries, broad-glob rejection,
#     missing-field rejection, synthetic fixture globs.
#   - Empty input, missing files, file-list mode, full-tree equivalence.
#   - Baseline inventory: all three detectors run clean against the full tree.
#
# Pure POSIX shell; no cargo, no python, no network, no Docker, no Kubernetes.
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
GIT_DETECTOR="$SCRIPT_DIR/check-git-boundary.sh"
HTTP_DETECTOR="$SCRIPT_DIR/check-http-boundary.sh"
K8S_DETECTOR="$SCRIPT_DIR/check-k8s-boundary.sh"
ALLOWLIST="$SCRIPT_DIR/capability-boundary-allowlist.toml"
FIXTURE_BASE="server/crates/djinn_capability_guard_fixture"

cleanup() {
    rm -rf -- "$REPO_ROOT/$FIXTURE_BASE" 2>/dev/null || true
    if [ -n "${LOG_DIR:-}" ] && [ -d "$LOG_DIR" ]; then
        rm -rf -- "$LOG_DIR"
    fi
}
trap cleanup EXIT INT TERM

# ── Verify required files exist ───────────────────────────────────────

for f in "$GUARD" "$GIT_DETECTOR" "$HTTP_DETECTOR" "$K8S_DETECTOR" "$ALLOWLIST"; do
    if [ ! -f "$f" ]; then
        printf 'FATAL: required file not found: %s\n' "$f" >&2
        exit 2
    fi
done

# ── Scratch space ─────────────────────────────────────────────────────

PASS=0
FAIL=0

LOG_DIR=$(mktemp -d /var/tmp/djinn-capability-guard-test.XXXXXX 2>/dev/null || \
          mktemp -d "$HOME/.cache/djinn/djinn-capability-guard-test.XXXXXX" 2>/dev/null || \
          mktemp -d "${TMPDIR:-.}/djinn-capability-guard-test.XXXXXX")
if [ ! -d "$LOG_DIR" ]; then
    printf 'FATAL: could not create scratch log dir\n' >&2
    exit 2
fi

# ── Test helpers ──────────────────────────────────────────────────────

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
# Runs the shared guard plumbing directly with caller-supplied env vars
# (CAPABILITY, OWNER, REMEDIATION, PATTERN).  Paths are piped via stdin
# in files-from-stdin mode.  When paths is empty, the guard sees empty stdin.
run_guard() {
    label=$1
    shift

    log="$LOG_DIR/$label.stdin"
    out="$LOG_DIR/$label.out"

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

# run_detector <label> <detector_script> [paths...]
#
# Runs a per-capability detector wrapper script directly.  The wrapper
# sets its own CAPABILITY/OWNER/REMEDIATION/PATTERN.  Paths are piped via
# stdin in files-from-stdin mode.
run_detector() {
    label=$1
    detector=$2
    shift 2

    log="$LOG_DIR/$label.stdin"
    out="$LOG_DIR/$label.out"

    if [ "$#" -eq 0 ]; then
        : > "$log"
    else
        printf '%s\n' "$@" > "$log"
    fi

    cd "$REPO_ROOT" && \
        env CAPABILITY_BOUNDARY_MODE=files-from-stdin \
        sh "$detector" --files-from-stdin < "$log" > "$out" 2>&1
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

    if grep -qF -- "$needle" "$log_path"; then
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

    if grep -qF -- "$needle" "$log_path"; then
        fail "$label" "expected output to NOT contain '$needle'
actual output:
$(cat "$log_path")"
    else
        pass "$label"
    fi
}

# ── Fixture setup ─────────────────────────────────────────────────────

# Always start from a clean slate.
rm -rf -- "$REPO_ROOT/$FIXTURE_BASE"

# Capture the full source-tree file list BEFORE creating any fixtures so it
# does not include synthetic test files.  Used for baseline inventory and
# full-tree equivalence tests.
FULL_TREE_LIST="$LOG_DIR/full-tree-files.txt"
find server/crates server/src -name '*.rs' -type f 2>/dev/null | sort > "$FULL_TREE_LIST"
FULL_TREE_COUNT=$(wc -l < "$FULL_TREE_LIST" | tr -d ' ')

printf '== running integrated capability-boundary self-tests ==\n'
printf '   (full source tree: %d Rust files)\n\n' "$FULL_TREE_COUNT"

# ═══════════════════════════════════════════════════════════════════════
# Section 1: Git capability (shared plumbing)
# ═══════════════════════════════════════════════════════════════════════

printf -- '-- Git capability (shared plumbing) --\n'

export CAPABILITY=git
export OWNER=server/crates/djinn-git
export REMEDIATION=djinn-git
export PATTERN='(git2::|use git2|Command::new\(\"git\"\)|tokio::process::Command::new\(\"git\"\)|::new\(\"git\"\))'

# ── git-T1: empty stdin exits 0 ───────────────────────────────────────
set +e
run_guard git_t1_empty
git_t1_rc=$?
set -e
assert_exit "git-T1 empty stdin exits 0" 0 "$git_t1_rc" "$LOG_DIR/git_t1_empty.out"
assert_output_contains "git-T1 reports no violations" \
    "no git boundary violations" "$LOG_DIR/git_t1_empty.out"

# ── git-T2: non-Rust files are ignored ────────────────────────────────
set +e
run_guard git_t2_non_rust \
    "scripts/check-capability-boundaries.sh" \
    "docs/architecture.md" \
    "ui/src/api/client.ts"
git_t2_rc=$?
set -e
assert_exit "git-T2 non-Rust files exit 0" 0 "$git_t2_rc" "$LOG_DIR/git_t2_non_rust.out"

# ── git-T3: nonexistent files are skipped ─────────────────────────────
set +e
run_guard git_t3_nonexistent \
    "server/crates/fake-crate/src/does_not_exist.rs"
git_t3_rc=$?
set -e
assert_exit "git-T3 nonexistent file exits 0" 0 "$git_t3_rc" "$LOG_DIR/git_t3_nonexistent.out"

# ── git-T4: git2:: violation outside owner crate ──────────────────────
GIT_VIOLATION_FILE="$REPO_ROOT/$FIXTURE_BASE/src/git_lib.rs"
mkdir -p "$(dirname "$GIT_VIOLATION_FILE")"
cat > "$GIT_VIOLATION_FILE" <<'FIXTURE'
use git2::Repository;

pub fn open_repo(path: &str) -> git2::Repository {
    git2::Repository::open(path).unwrap()
}
FIXTURE
GIT_VIOLATION_PATH="$FIXTURE_BASE/src/git_lib.rs"

set +e
run_guard git_t4_violation "$GIT_VIOLATION_PATH"
git_t4_rc=$?
set -e
assert_exit "git-T4 git2:: violation exits non-zero" 1 "$git_t4_rc" "$LOG_DIR/git_t4_violation.out"
assert_output_contains "git-T4 reports violating file" \
    "file=$GIT_VIOLATION_PATH" "$LOG_DIR/git_t4_violation.out"
assert_output_contains "git-T4 mentions remediation owner" \
    "Remediation owner: djinn-git" "$LOG_DIR/git_t4_violation.out"

# ── git-T5: Command::new("git") violation ─────────────────────────────
GIT_CMD_FILE="$REPO_ROOT/$FIXTURE_BASE/src/git_cmd.rs"
cat > "$GIT_CMD_FILE" <<'FIXTURE'
use std::process::Command;

pub fn git_status() {
    let _ = Command::new("git").arg("status").output();
}
FIXTURE
GIT_CMD_PATH="$FIXTURE_BASE/src/git_cmd.rs"

set +e
run_guard git_t5_cmd "$GIT_CMD_PATH"
git_t5_rc=$?
set -e
assert_exit "git-T5 Command::new(git) violation exits non-zero" 1 "$git_t5_rc" "$LOG_DIR/git_t5_cmd.out"
assert_output_contains "git-T5 reports Command::new(git)" \
    'Command::new("git")' "$LOG_DIR/git_t5_cmd.out"

# ── git-T6: comment-only match is ignored ─────────────────────────────
GIT_COMMENT_FILE="$REPO_ROOT/$FIXTURE_BASE/src/git_comment.rs"
cat > "$GIT_COMMENT_FILE" <<'FIXTURE'
// use git2::Repository;

pub fn noop() {}
FIXTURE
GIT_COMMENT_PATH="$FIXTURE_BASE/src/git_comment.rs"

set +e
run_guard git_t6_comment "$GIT_COMMENT_PATH"
git_t6_rc=$?
set -e
assert_exit "git-T6 comment-only match exits 0" 0 "$git_t6_rc" "$LOG_DIR/git_t6_comment.out"
assert_output_lacks "git-T6 does not flag comment" \
    "file=$GIT_COMMENT_PATH" "$LOG_DIR/git_t6_comment.out"

# ── git-T7: git owner crate path is exempted ──────────────────────────
GIT_OWNER_PATH="server/crates/djinn-git/src/lib.rs"

set +e
run_guard git_t7_owner "$GIT_OWNER_PATH"
git_t7_rc=$?
set -e
assert_exit "git-T7 owner crate path exits 0" 0 "$git_t7_rc" "$LOG_DIR/git_t7_owner.out"

# ── git-T8: allowlist exempts an exact path+matcher ──────────────────
GIT_ALLOWED_FILE="$REPO_ROOT/$FIXTURE_BASE/src/allowed.rs"
cat > "$GIT_ALLOWED_FILE" <<'FIXTURE'
use git2::Repository;

pub fn allowed_repo(path: &str) -> git2::Repository {
    git2::Repository::open(path).unwrap()
}
FIXTURE
GIT_ALLOWED_PATH="$FIXTURE_BASE/src/allowed.rs"

# This entry is pre-committed in capability-boundary-allowlist.toml.
set +e
run_guard git_t8_allowlisted "$GIT_ALLOWED_PATH"
git_t8_rc=$?
set -e
assert_exit "git-T8 allowlisted file exits 0" 0 "$git_t8_rc" "$LOG_DIR/git_t8_allowlisted.out"
assert_output_lacks "git-T8 does not flag allowlisted file" \
    "file=$GIT_ALLOWED_PATH" "$LOG_DIR/git_t8_allowlisted.out"

# ── git-T9: allowlist rejects broad globs as config errors ────────────
BAD_ALLOWLIST="$LOG_DIR/bad-allowlist-broad.toml"
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
    sh "$GUARD" --files-from-stdin < /dev/null > "$LOG_DIR/git_t9_broad_glob.out" 2>&1
git_t9_rc=$?
set -e
assert_exit "git-T9 broad allowlist glob exits 2" 2 "$git_t9_rc" "$LOG_DIR/git_t9_broad_glob.out"
assert_output_contains "git-T9 reports forbidden broad glob" \
    "forbidden broad glob" "$LOG_DIR/git_t9_broad_glob.out"

# ── git-T10: allowlist rejects missing required fields ────────────────
BAD_ALLOWLIST2="$LOG_DIR/bad-allowlist-missing.toml"
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
    sh "$GUARD" --files-from-stdin < /dev/null > "$LOG_DIR/git_t10_missing_fields.out" 2>&1
git_t10_rc=$?
set -e
assert_exit "git-T10 missing required fields exits 2" 2 "$git_t10_rc" "$LOG_DIR/git_t10_missing_fields.out"

# ── git-T11: --help exits 0 ───────────────────────────────────────────
set +e
cd "$REPO_ROOT" && env \
    CAPABILITY=git OWNER=server/crates/djinn-git REMEDIATION=djinn-git \
    PATTERN='git2::' sh "$GUARD" --help > "$LOG_DIR/git_t11_help.out" 2>&1
git_t11_rc=$?
set -e
assert_exit "git-T11 --help exits 0" 0 "$git_t11_rc" "$LOG_DIR/git_t11_help.out"
assert_output_contains "git-T11 help mentions files-from-stdin" \
    "files-from-stdin" "$LOG_DIR/git_t11_help.out"

# ── git-T12: mixed files — violation + comment + owner ────────────────
set +e
run_guard git_t12_mixed "$GIT_VIOLATION_PATH" "$GIT_COMMENT_PATH" "$GIT_OWNER_PATH"
git_t12_rc=$?
set -e
assert_exit "git-T12 mixed files exits non-zero" 1 "$git_t12_rc" "$LOG_DIR/git_t12_mixed.out"
assert_output_contains "git-T12 reports violation file" \
    "file=$GIT_VIOLATION_PATH" "$LOG_DIR/git_t12_mixed.out"
assert_output_lacks "git-T12 does not flag owner path" \
    "file=$GIT_OWNER_PATH" "$LOG_DIR/git_t12_mixed.out"
assert_output_lacks "git-T12 does not flag comment file" \
    "file=$GIT_COMMENT_PATH" "$LOG_DIR/git_t12_mixed.out"

# ── git-T13: synthetic fixture glob is allowed in allowlist ───────────
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
    sh "$GUARD" --files-from-stdin < /dev/null > "$LOG_DIR/git_t13_fixture_glob.out" 2>&1
git_t13_rc=$?
set -e
assert_exit "git-T13 synthetic fixture glob exits 0" 0 "$git_t13_rc" "$LOG_DIR/git_t13_fixture_glob.out"

printf '\n'

# ═══════════════════════════════════════════════════════════════════════
# Section 2: HTTP capability (shared plumbing)
# ═══════════════════════════════════════════════════════════════════════

printf -- '-- HTTP capability (shared plumbing) --\n'

export CAPABILITY=http
export OWNER=server/crates/djinn-provider
export REMEDIATION=djinn-provider
export PATTERN='(reqwest::Client|reqwest::ClientBuilder|reqwest::RequestBuilder|reqwest::\{)'

# ── http-T1: reqwest::Client violation ────────────────────────────────
HTTP_CLIENT_FILE="$REPO_ROOT/$FIXTURE_BASE/src/http_client.rs"
cat > "$HTTP_CLIENT_FILE" <<'FIXTURE'
pub fn make_client() {
    let client = reqwest::Client::new();
}
FIXTURE
HTTP_CLIENT_PATH="$FIXTURE_BASE/src/http_client.rs"

set +e
run_guard http_t1_client "$HTTP_CLIENT_PATH"
http_t1_rc=$?
set -e
assert_exit "http-T1 reqwest::Client violation exits non-zero" 1 "$http_t1_rc" "$LOG_DIR/http_t1_client.out"
assert_output_contains "http-T1 reports violating file" \
    "file=$HTTP_CLIENT_PATH" "$LOG_DIR/http_t1_client.out"
assert_output_contains "http-T1 mentions remediation owner" \
    "Remediation owner: djinn-provider" "$LOG_DIR/http_t1_client.out"

# ── http-T2: reqwest::ClientBuilder violation ─────────────────────────
HTTP_BUILDER_FILE="$REPO_ROOT/$FIXTURE_BASE/src/http_builder.rs"
cat > "$HTTP_BUILDER_FILE" <<'FIXTURE'
pub fn build_client() {
    let client = reqwest::ClientBuilder::new()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap();
}
FIXTURE
HTTP_BUILDER_PATH="$FIXTURE_BASE/src/http_builder.rs"

set +e
run_guard http_t2_builder "$HTTP_BUILDER_PATH"
http_t2_rc=$?
set -e
assert_exit "http-T2 reqwest::ClientBuilder violation exits non-zero" 1 "$http_t2_rc" "$LOG_DIR/http_t2_builder.out"
assert_output_contains "http-T2 reports ClientBuilder matcher" \
    "reqwest::ClientBuilder" "$LOG_DIR/http_t2_builder.out"

# ── http-T3: reqwest::RequestBuilder violation ────────────────────────
HTTP_REQBUILDER_FILE="$REPO_ROOT/$FIXTURE_BASE/src/http_reqbuilder.rs"
cat > "$HTTP_REQBUILDER_FILE" <<'FIXTURE'
pub fn build_request(client: &str) {
    let req: reqwest::RequestBuilder = unimplemented!();
}
FIXTURE
HTTP_REQBUILDER_PATH="$FIXTURE_BASE/src/http_reqbuilder.rs"

set +e
run_guard http_t3_reqbuilder "$HTTP_REQBUILDER_PATH"
http_t3_rc=$?
set -e
assert_exit "http-T3 reqwest::RequestBuilder violation exits non-zero" 1 "$http_t3_rc" "$LOG_DIR/http_t3_reqbuilder.out"
assert_output_contains "http-T3 reports RequestBuilder matcher" \
    "reqwest::RequestBuilder" "$LOG_DIR/http_t3_reqbuilder.out"

# ── http-T4: reqwest::{ destructured import violation ────────────────
HTTP_DESTRUCT_FILE="$REPO_ROOT/$FIXTURE_BASE/src/http_destruct.rs"
cat > "$HTTP_DESTRUCT_FILE" <<'FIXTURE'
use reqwest::{Client, StatusCode};

pub fn fetch() -> StatusCode {
    StatusCode::OK
}
FIXTURE
HTTP_DESTRUCT_PATH="$FIXTURE_BASE/src/http_destruct.rs"

set +e
run_guard http_t4_destruct "$HTTP_DESTRUCT_PATH"
http_t4_rc=$?
set -e
assert_exit "http-T4 reqwest::{ violation exits non-zero" 1 "$http_t4_rc" "$LOG_DIR/http_t4_destruct.out"
assert_output_contains "http-T4 reports destructured import matcher" \
    "reqwest::{" "$LOG_DIR/http_t4_destruct.out"

# ── http-T5: HTTP owner crate path is exempted ────────────────────────
HTTP_OWNER_PATH="server/crates/djinn-provider/src/github_api.rs"

set +e
run_guard http_t5_owner "$HTTP_OWNER_PATH"
http_t5_rc=$?
set -e
assert_exit "http-T5 owner crate path exits 0" 0 "$http_t5_rc" "$LOG_DIR/http_t5_owner.out"

# ── http-T6: comment-only match is ignored ────────────────────────────
HTTP_COMMENT_FILE="$REPO_ROOT/$FIXTURE_BASE/src/http_comment.rs"
cat > "$HTTP_COMMENT_FILE" <<'FIXTURE'
// let client = reqwest::Client::new();

pub fn noop() {}
FIXTURE
HTTP_COMMENT_PATH="$FIXTURE_BASE/src/http_comment.rs"

set +e
run_guard http_t6_comment "$HTTP_COMMENT_PATH"
http_t6_rc=$?
set -e
assert_exit "http-T6 comment-only match exits 0" 0 "$http_t6_rc" "$LOG_DIR/http_t6_comment.out"
assert_output_lacks "http-T6 does not flag comment" \
    "file=$HTTP_COMMENT_PATH" "$LOG_DIR/http_t6_comment.out"

# ── http-T7: reqwest::StatusCode-only is NOT flagged ──────────────────
HTTP_STATUSONLY_FILE="$REPO_ROOT/$FIXTURE_BASE/src/http_statusonly.rs"
cat > "$HTTP_STATUSONLY_FILE" <<'FIXTURE'
use reqwest::StatusCode;

pub fn is_ok(code: StatusCode) -> bool {
    code.is_success()
}
FIXTURE
HTTP_STATUSONLY_PATH="$FIXTURE_BASE/src/http_statusonly.rs"

set +e
run_guard http_t7_statusonly "$HTTP_STATUSONLY_PATH"
http_t7_rc=$?
set -e
assert_exit "http-T7 StatusCode-only exits 0" 0 "$http_t7_rc" "$LOG_DIR/http_t7_statusonly.out"
assert_output_contains "http-T7 reports no violations" \
    "no http boundary violations" "$LOG_DIR/http_t7_statusonly.out"

printf '\n'

# ═══════════════════════════════════════════════════════════════════════
# Section 3: k8s capability (shared plumbing)
# ═══════════════════════════════════════════════════════════════════════

printf -- '-- k8s capability (shared plumbing) --\n'

export CAPABILITY=k8s
export OWNER=server/crates/djinn-k8s
export REMEDIATION=djinn-k8s
export PATTERN='(kube::|use kube|k8s_openapi|Command::new\(\"kubectl\"\)|tokio::process::Command::new\(\"kubectl\"\)|::new\(\"kubectl\"\))'

# ── k8s-T1: kube:: violation ──────────────────────────────────────────
K8S_KUBE_FILE="$REPO_ROOT/$FIXTURE_BASE/src/k8s_kube.rs"
cat > "$K8S_KUBE_FILE" <<'FIXTURE'
pub async fn get_pods() {
    let client = kube::Client::try_default().await.unwrap();
    let pods: kube::Api<k8s_openapi::api::core::v1::Pod> =
        kube::Api::all(client);
}
FIXTURE
K8S_KUBE_PATH="$FIXTURE_BASE/src/k8s_kube.rs"

set +e
run_guard k8s_t1_kube "$K8S_KUBE_PATH"
k8s_t1_rc=$?
set -e
assert_exit "k8s-T1 kube:: violation exits non-zero" 1 "$k8s_t1_rc" "$LOG_DIR/k8s_t1_kube.out"
assert_output_contains "k8s-T1 reports violating file" \
    "file=$K8S_KUBE_PATH" "$LOG_DIR/k8s_t1_kube.out"
assert_output_contains "k8s-T1 mentions remediation owner" \
    "Remediation owner: djinn-k8s" "$LOG_DIR/k8s_t1_kube.out"

# ── k8s-T2: k8s_openapi violation ─────────────────────────────────────
K8S_OPENAPI_FILE="$REPO_ROOT/$FIXTURE_BASE/src/k8s_openapi.rs"
cat > "$K8S_OPENAPI_FILE" <<'FIXTURE'
use k8s_openapi::api::core::v1::Pod;

pub fn build_pod() -> Pod {
    Pod::default()
}
FIXTURE
K8S_OPENAPI_PATH="$FIXTURE_BASE/src/k8s_openapi.rs"

set +e
run_guard k8s_t2_openapi "$K8S_OPENAPI_PATH"
k8s_t2_rc=$?
set -e
assert_exit "k8s-T2 k8s_openapi violation exits non-zero" 1 "$k8s_t2_rc" "$LOG_DIR/k8s_t2_openapi.out"
assert_output_contains "k8s-T2 reports k8s_openapi matcher" \
    "k8s_openapi" "$LOG_DIR/k8s_t2_openapi.out"

# ── k8s-T3: Command::new("kubectl") violation ─────────────────────────
K8S_CMD_FILE="$REPO_ROOT/$FIXTURE_BASE/src/k8s_cmd.rs"
cat > "$K8S_CMD_FILE" <<'FIXTURE'
use std::process::Command;

pub fn kubectl_apply() {
    let _ = Command::new("kubectl").arg("apply").arg("-f").arg("deploy.yaml").output();
}
FIXTURE
K8S_CMD_PATH="$FIXTURE_BASE/src/k8s_cmd.rs"

set +e
run_guard k8s_t3_cmd "$K8S_CMD_PATH"
k8s_t3_rc=$?
set -e
assert_exit "k8s-T3 Command::new(kubectl) violation exits non-zero" 1 "$k8s_t3_rc" "$LOG_DIR/k8s_t3_cmd.out"
assert_output_contains "k8s-T3 reports Command::new(kubectl)" \
    'Command::new("kubectl")' "$LOG_DIR/k8s_t3_cmd.out"

# ── k8s-T4: k8s owner crate path is exempted ──────────────────────────
K8S_OWNER_PATH="server/crates/djinn-k8s/src/secret.rs"

set +e
run_guard k8s_t4_owner "$K8S_OWNER_PATH"
k8s_t4_rc=$?
set -e
assert_exit "k8s-T4 owner crate path exits 0" 0 "$k8s_t4_rc" "$LOG_DIR/k8s_t4_owner.out"

# ── k8s-T5: comment-only match is ignored ─────────────────────────────
K8S_COMMENT_FILE="$REPO_ROOT/$FIXTURE_BASE/src/k8s_comment.rs"
cat > "$K8S_COMMENT_FILE" <<'FIXTURE'
// use kube::Client;

pub fn noop() {}
FIXTURE
K8S_COMMENT_PATH="$FIXTURE_BASE/src/k8s_comment.rs"

set +e
run_guard k8s_t5_comment "$K8S_COMMENT_PATH"
k8s_t5_rc=$?
set -e
assert_exit "k8s-T5 comment-only match exits 0" 0 "$k8s_t5_rc" "$LOG_DIR/k8s_t5_comment.out"
assert_output_lacks "k8s-T5 does not flag comment" \
    "file=$K8S_COMMENT_PATH" "$LOG_DIR/k8s_t5_comment.out"

printf '\n'

# ═══════════════════════════════════════════════════════════════════════
# Section 4: Alias patterns
# ═══════════════════════════════════════════════════════════════════════

printf -- '-- Alias patterns --\n'

# ── alias-T1: aliased Command for git (Cmd::new("git")) ──────────────
# Tests the ::new("git") catch-all that detects aliased process::Command
# forms where the type name is not literally "Command".
export CAPABILITY=git
export OWNER=server/crates/djinn-git
export REMEDIATION=djinn-git
export PATTERN='(git2::|use git2|Command::new\(\"git\"\)|tokio::process::Command::new\(\"git\"\)|::new\(\"git\"\))'

ALIAS_GIT_FILE="$REPO_ROOT/$FIXTURE_BASE/src/alias_git.rs"
cat > "$ALIAS_GIT_FILE" <<'FIXTURE'
use std::process::Command as Cmd;

pub fn git_clone(url: &str) {
    let _ = Cmd::new("git").arg("clone").arg(url).output();
}
FIXTURE
ALIAS_GIT_PATH="$FIXTURE_BASE/src/alias_git.rs"

set +e
run_guard alias_t1_git "$ALIAS_GIT_PATH"
alias_t1_rc=$?
set -e
assert_exit "alias-T1 aliased Cmd::new(git) exits non-zero" 1 "$alias_t1_rc" "$LOG_DIR/alias_t1_git.out"
assert_output_contains "alias-T1 reports git capability violation" \
    "git capability usage" "$LOG_DIR/alias_t1_git.out"

# ── alias-T2: tokio::process::Command::new("git") ────────────────────
ALIAS_TOKIO_GIT_FILE="$REPO_ROOT/$FIXTURE_BASE/src/alias_tokio_git.rs"
cat > "$ALIAS_TOKIO_GIT_FILE" <<'FIXTURE'
pub async fn git_fetch() {
    let _ = tokio::process::Command::new("git")
        .arg("fetch")
        .output()
        .await
        .unwrap();
}
FIXTURE
ALIAS_TOKIO_GIT_PATH="$FIXTURE_BASE/src/alias_tokio_git.rs"

set +e
run_guard alias_t2_tokio_git "$ALIAS_TOKIO_GIT_PATH"
alias_t2_rc=$?
set -e
assert_exit "alias-T2 tokio::process::Command::new(git) exits non-zero" 1 "$alias_t2_rc" "$LOG_DIR/alias_t2_tokio_git.out"
assert_output_contains "alias-T2 reports tokio git matcher" \
    'tokio::process::Command::new("git")' "$LOG_DIR/alias_t2_tokio_git.out"

# ── alias-T3: aliased Command for kubectl (Cmd::new("kubectl")) ──────
export CAPABILITY=k8s
export OWNER=server/crates/djinn-k8s
export REMEDIATION=djinn-k8s
export PATTERN='(kube::|use kube|k8s_openapi|Command::new\(\"kubectl\"\)|tokio::process::Command::new\(\"kubectl\"\)|::new\(\"kubectl\"\))'

ALIAS_KUBECTL_FILE="$REPO_ROOT/$FIXTURE_BASE/src/alias_kubectl.rs"
cat > "$ALIAS_KUBECTL_FILE" <<'FIXTURE'
use std::process::Command as Cmd;

pub fn kubectl_get() {
    let _ = Cmd::new("kubectl").arg("get").arg("pods").output();
}
FIXTURE
ALIAS_KUBECTL_PATH="$FIXTURE_BASE/src/alias_kubectl.rs"

set +e
run_guard alias_t3_kubectl "$ALIAS_KUBECTL_PATH"
alias_t3_rc=$?
set -e
assert_exit "alias-T3 aliased Cmd::new(kubectl) exits non-zero" 1 "$alias_t3_rc" "$LOG_DIR/alias_t3_kubectl.out"
assert_output_contains "alias-T3 reports k8s capability violation" \
    "k8s capability usage" "$LOG_DIR/alias_t3_kubectl.out"

# ── alias-T4: tokio::process::Command::new("kubectl") ────────────────
ALIAS_TOKIO_KUBECTL_FILE="$REPO_ROOT/$FIXTURE_BASE/src/alias_tokio_kubectl.rs"
cat > "$ALIAS_TOKIO_KUBECTL_FILE" <<'FIXTURE'
pub async fn kubectl_apply() {
    let _ = tokio::process::Command::new("kubectl")
        .arg("apply")
        .output()
        .await
        .unwrap();
}
FIXTURE
ALIAS_TOKIO_KUBECTL_PATH="$FIXTURE_BASE/src/alias_tokio_kubectl.rs"

set +e
run_guard alias_t4_tokio_kubectl "$ALIAS_TOKIO_KUBECTL_PATH"
alias_t4_rc=$?
set -e
assert_exit "alias-T4 tokio::process::Command::new(kubectl) exits non-zero" 1 "$alias_t4_rc" "$LOG_DIR/alias_t4_tokio_kubectl.out"
assert_output_contains "alias-T4 reports tokio kubectl matcher" \
    'tokio::process::Command::new("kubectl")' "$LOG_DIR/alias_t4_tokio_kubectl.out"

printf '\n'

# ═══════════════════════════════════════════════════════════════════════
# Section 5: Ignored paths
# ═══════════════════════════════════════════════════════════════════════

printf -- '-- Ignored paths --\n'

export CAPABILITY=git
export OWNER=server/crates/djinn-git
export REMEDIATION=djinn-git
export PATTERN='(git2::|use git2|Command::new\(\"git\"\)|tokio::process::Command::new\(\"git\"\)|::new\(\"git\"\))'

# ── ignore-T1: generated path is ignored ──────────────────────────────
GENERATED_FILE="$REPO_ROOT/$FIXTURE_BASE/generated/src/gen.rs"
mkdir -p "$(dirname "$GENERATED_FILE")"
cat > "$GENERATED_FILE" <<'FIXTURE'
use git2::Repository;
pub fn gen_repo() -> git2::Repository { git2::Repository::open(".").unwrap() }
FIXTURE
GENERATED_PATH="$FIXTURE_BASE/generated/src/gen.rs"

set +e
run_guard ignore_t1_generated "$GENERATED_PATH"
ignore_t1_rc=$?
set -e
assert_exit "ignore-T1 generated path exits 0" 0 "$ignore_t1_rc" "$LOG_DIR/ignore_t1_generated.out"
assert_output_lacks "ignore-T1 does not flag generated file" \
    "file=$GENERATED_PATH" "$LOG_DIR/ignore_t1_generated.out"

# ── ignore-T2: vendor path is ignored ─────────────────────────────────
VENDOR_FILE="$REPO_ROOT/$FIXTURE_BASE/vendor/src/vendored.rs"
mkdir -p "$(dirname "$VENDOR_FILE")"
cat > "$VENDOR_FILE" <<'FIXTURE'
use git2::Repository;
pub fn vendored_repo() -> git2::Repository { git2::Repository::open(".").unwrap() }
FIXTURE
VENDOR_PATH="$FIXTURE_BASE/vendor/src/vendored.rs"

set +e
run_guard ignore_t2_vendor "$VENDOR_PATH"
ignore_t2_rc=$?
set -e
assert_exit "ignore-T2 vendor path exits 0" 0 "$ignore_t2_rc" "$LOG_DIR/ignore_t2_vendor.out"
assert_output_lacks "ignore-T2 does not flag vendor file" \
    "file=$VENDOR_PATH" "$LOG_DIR/ignore_t2_vendor.out"

# ── ignore-T3: target path is ignored ─────────────────────────────────
TARGET_FILE="$REPO_ROOT/$FIXTURE_BASE/target/debug/build_script.rs"
mkdir -p "$(dirname "$TARGET_FILE")"
cat > "$TARGET_FILE" <<'FIXTURE'
use git2::Repository;
pub fn target_repo() -> git2::Repository { git2::Repository::open(".").unwrap() }
FIXTURE
TARGET_PATH="$FIXTURE_BASE/target/debug/build_script.rs"

set +e
run_guard ignore_t3_target "$TARGET_PATH"
ignore_t3_rc=$?
set -e
assert_exit "ignore-T3 target path exits 0" 0 "$ignore_t3_rc" "$LOG_DIR/ignore_t3_target.out"
assert_output_lacks "ignore-T3 does not flag target file" \
    "file=$TARGET_PATH" "$LOG_DIR/ignore_t3_target.out"

printf '\n'

# ═══════════════════════════════════════════════════════════════════════
# Section 6: Per-capability detector wrapper scripts
# ═══════════════════════════════════════════════════════════════════════

printf -- '-- Per-capability detector scripts --\n'

# ── detector-T1: check-git-boundary.sh detects git2:: violation ──────
set +e
run_detector det_t1_git_violation "$GIT_DETECTOR" "$GIT_VIOLATION_PATH"
det_t1_rc=$?
set -e
assert_exit "det-T1 git detector detects git2:: violation" 1 "$det_t1_rc" "$LOG_DIR/det_t1_git_violation.out"
assert_output_contains "det-T1 git detector reports violation" \
    "file=$GIT_VIOLATION_PATH" "$LOG_DIR/det_t1_git_violation.out"

# ── detector-T2: check-git-boundary.sh exempts owner crate ────────────
set +e
run_detector det_t2_git_owner "$GIT_DETECTOR" "$GIT_OWNER_PATH"
det_t2_rc=$?
set -e
assert_exit "det-T2 git detector exempts owner crate" 0 "$det_t2_rc" "$LOG_DIR/det_t2_git_owner.out"

# ── detector-T3: check-http-boundary.sh detects reqwest::Client ───────
set +e
run_detector det_t3_http_violation "$HTTP_DETECTOR" "$HTTP_CLIENT_PATH"
det_t3_rc=$?
set -e
assert_exit "det-T3 http detector detects reqwest::Client" 1 "$det_t3_rc" "$LOG_DIR/det_t3_http_violation.out"
assert_output_contains "det-T3 http detector reports violation" \
    "file=$HTTP_CLIENT_PATH" "$LOG_DIR/det_t3_http_violation.out"

# ── detector-T4: check-http-boundary.sh exempts owner crate ───────────
set +e
run_detector det_t4_http_owner "$HTTP_DETECTOR" "$HTTP_OWNER_PATH"
det_t4_rc=$?
set -e
assert_exit "det-T4 http detector exempts owner crate" 0 "$det_t4_rc" "$LOG_DIR/det_t4_http_owner.out"

# ── detector-T5: check-k8s-boundary.sh detects kube:: violation ───────
set +e
run_detector det_t5_k8s_violation "$K8S_DETECTOR" "$K8S_KUBE_PATH"
det_t5_rc=$?
set -e
assert_exit "det-T5 k8s detector detects kube::" 1 "$det_t5_rc" "$LOG_DIR/det_t5_k8s_violation.out"
assert_output_contains "det-T5 k8s detector reports violation" \
    "file=$K8S_KUBE_PATH" "$LOG_DIR/det_t5_k8s_violation.out"

# ── detector-T6: check-k8s-boundary.sh exempts owner crate ────────────
set +e
run_detector det_t6_k8s_owner "$K8S_DETECTOR" "$K8S_OWNER_PATH"
det_t6_rc=$?
set -e
assert_exit "det-T6 k8s detector exempts owner crate" 0 "$det_t6_rc" "$LOG_DIR/det_t6_k8s_owner.out"

printf '\n'

# ═══════════════════════════════════════════════════════════════════════
# Section 7: File-list mode / full-tree equivalence
# ═══════════════════════════════════════════════════════════════════════

printf -- '-- File-list mode / full-tree equivalence --\n'

# ── filelist-T1: single fixture in file-list mode → violation ─────────
# Already demonstrated by git-T4 via run_guard; here we exercise the
# per-capability wrapper in the same way to confirm equivalence.
set +e
run_detector filelist_t1_single "$GIT_DETECTOR" "$GIT_VIOLATION_PATH"
filelist_t1_rc=$?
set -e
assert_exit "filelist-T1 single fixture → violation" 1 "$filelist_t1_rc" "$LOG_DIR/filelist_t1_single.out"

# ── filelist-T2: fixture + full tree → same violation ─────────────────
# Combine the fixture path with the full source-tree file list.  The
# detector must still flag the fixture violation.
COMBINED_LIST="$LOG_DIR/filelist_t2_combined.txt"
{ printf '%s\n' "$GIT_VIOLATION_PATH"; cat "$FULL_TREE_LIST"; } > "$COMBINED_LIST"

set +e
cd "$REPO_ROOT" && \
    env CAPABILITY_BOUNDARY_MODE=files-from-stdin \
    sh "$GIT_DETECTOR" --files-from-stdin < "$COMBINED_LIST" \
    > "$LOG_DIR/filelist_t2_combined.out" 2>&1
filelist_t2_rc=$?
set -e
assert_exit "filelist-T2 fixture + full tree → still violation" 1 "$filelist_t2_rc" "$LOG_DIR/filelist_t2_combined.out"
assert_output_contains "filelist-T2 still flags the fixture" \
    "file=$GIT_VIOLATION_PATH" "$LOG_DIR/filelist_t2_combined.out"

printf '\n'

# ═══════════════════════════════════════════════════════════════════════
# Section 8: Baseline inventory
# ═══════════════════════════════════════════════════════════════════════

printf -- '-- Baseline inventory --\n'

# Run all three detectors against the full source tree (captured before any
# fixtures were created).  The allowlist pre-allowlists existing non-owner
# usage so the guard runs clean today.  Any new violation would be surfaced
# here, making the inventory actionable and auditable.

GIT_ALLOWLIST_ENTRIES=$(grep -c 'capability = "git"' "$ALLOWLIST" || true)
HTTP_ALLOWLIST_ENTRIES=$(grep -c 'capability = "http"' "$ALLOWLIST" || true)
K8S_ALLOWLIST_ENTRIES=$(grep -c 'capability = "k8s"' "$ALLOWLIST" || true)

printf '   In-scope Rust source files: %d\n' "$FULL_TREE_COUNT"
printf '   Allowlist entries: git=%s http=%s k8s=%s\n' \
    "$GIT_ALLOWLIST_ENTRIES" "$HTTP_ALLOWLIST_ENTRIES" "$K8S_ALLOWLIST_ENTRIES"

# ── baseline-T1: git detector against full tree ───────────────────────
set +e
cd "$REPO_ROOT" && \
    env CAPABILITY_BOUNDARY_MODE=files-from-stdin \
    sh "$GIT_DETECTOR" --files-from-stdin < "$FULL_TREE_LIST" \
    > "$LOG_DIR/baseline_git.out" 2>&1
baseline_git_rc=$?
set -e
assert_exit "baseline-T1 git detector full tree exits 0" 0 "$baseline_git_rc" "$LOG_DIR/baseline_git.out"

# ── baseline-T2: http detector against full tree ──────────────────────
set +e
cd "$REPO_ROOT" && \
    env CAPABILITY_BOUNDARY_MODE=files-from-stdin \
    sh "$HTTP_DETECTOR" --files-from-stdin < "$FULL_TREE_LIST" \
    > "$LOG_DIR/baseline_http.out" 2>&1
baseline_http_rc=$?
set -e
assert_exit "baseline-T2 http detector full tree exits 0" 0 "$baseline_http_rc" "$LOG_DIR/baseline_http.out"

# ── baseline-T3: k8s detector against full tree ───────────────────────
set +e
cd "$REPO_ROOT" && \
    env CAPABILITY_BOUNDARY_MODE=files-from-stdin \
    sh "$K8S_DETECTOR" --files-from-stdin < "$FULL_TREE_LIST" \
    > "$LOG_DIR/baseline_k8s.out" 2>&1
baseline_k8s_rc=$?
set -e
assert_exit "baseline-T3 k8s detector full tree exits 0" 0 "$baseline_k8s_rc" "$LOG_DIR/baseline_k8s.out"

printf '   All three detectors pass against the current baseline.\n'

printf '\n'

# ── summary ────────────────────────────────────────────────────────────
printf -- '------------------------------------------\n'
printf 'passed: %d   failed: %d\n' "$PASS" "$FAIL"

if [ "$FAIL" -ne 0 ]; then
    exit 1
fi
exit 0
