#!/bin/sh
# Self-test harness for scripts/check-resize-reachability.sh.
#
# A guard nobody has watched fail is not a guard. This drives the production
# guard against synthetic trees under a scratch directory (always torn down) via
# its GUARD_ROOT hook, and proves it fires on each way reachability can be lost:
#
#   * the production call site is DELETED (the named mutation in 0ppk-1b AC2);
#   * the call site still exists but only under `#[cfg(test)]`;
#   * the call site still exists but only in a `*_tests.rs` file;
#   * the call site still exists but only under a `tests/` directory;
#   * the caller exists and nothing composes it (the trait-override failure);
#   * an anchor file was renamed away, which must fail rather than pass vacuously;
#   * `0ppk-3`'s reconciler spawn is deleted from `become_leader`;
#   * `list_nonterminal_resize` loses its only production caller, which is the
#     state main was in before `0ppk-3` — a durable read with no reader.
#
# Run from anywhere:
#
#   sh scripts/test-check-resize-reachability.sh
#
# Exits 0 on success, 1 if any assertion failed. Pure POSIX shell; no cargo.

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
GUARD="$SCRIPT_DIR/check-resize-reachability.sh"
SCRATCH="$REPO_ROOT/.resize-reachability-guard-selftest"

STATE=server/src/server/state/mod.rs
BRIDGE=server/src/task_run_resize_bootstrap.rs
SEAM=server/crates/djinn-agent/src/actors/slot/supervisor_runner.rs
RECONCILE=server/src/task_run_resize_reconcile.rs

cleanup() {
    rm -rf -- "$SCRATCH" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

if [ ! -f "$GUARD" ]; then
    printf 'FATAL: production guard not found at %s\n' "$GUARD" >&2
    exit 2
fi

PASS=0
FAIL=0

pass() {
    PASS=$((PASS + 1))
    printf '  ok   %s\n' "$1"
}

fail() {
    FAIL=$((FAIL + 1))
    printf '  FAIL %s\n' "$1" >&2
}

write_state() {
    mkdir -p -- "$SCRATCH/$(dirname "$STATE")"
    cat >"$SCRATCH/$STATE" <<'EOF'
// Composition root fixture.
fn new_inner() {
    let resize_admission =
        Arc::new(TaskRunResizeAdmissionBridge::from_env(db.clone()));
}
fn agent_context() {
    AgentContext {
        resize_admission: Some(self.inner.resize_admission.clone()),
    }
}
pub async fn become_leader(&self) {
    crate::task_run_resize_reconcile::spawn(self.clone());
}
EOF
}

write_reconcile() {
    mkdir -p -- "$SCRATCH/$(dirname "$RECONCILE")"
    cat >"$SCRATCH/$RECONCILE" <<'EOF'
// Reconciler fixture: the durable read the external reconciler is FOR.
use djinn_db::BuildPodPermitRepository;

impl TaskRunResizeReconciler {
    async fn run_pass(&self) {
        let rows = self.permits.list_nonterminal_resize().await;
    }
}

pub fn spawn(state: AppState) {}
EOF
}

write_bridge() {
    mkdir -p -- "$SCRATCH/$(dirname "$BRIDGE")"
    cat >"$SCRATCH/$BRIDGE" <<'EOF'
// Bridge fixture: names TaskRunResizeBootstrap, DispatchGate and
// BuildPodPermitRepository, and calls into all three.
pub struct TaskRunResizeBootstrap;
pub struct DispatchGate;
use djinn_db::BuildPodPermitRepository;

impl TaskRunResizeBootstrap {
    async fn capture(&self) {
        self.permits.capture_resize_identity(a, b, c, d).await;
    }
}

async fn admit_dispatch(&self) {
    bootstrap.bootstrap(&permit, protocol).await;
    self.gate.admit(&permit.task_run_id);
}
EOF
}

write_seam() {
    mkdir -p -- "$SCRATCH/$(dirname "$SEAM")"
    cat >"$SCRATCH/$SEAM" <<'EOF'
// Dispatch seam fixture.
use djinn_db::BuildPodPermitRepository;

async fn acquire_build_pod_permit(app_state: &AgentContext, spec: &TaskRunSpec) {
    let permits = BuildPodPermitRepository::new(app_state.db.clone());
    permits.acquire(&spec.task_run_id, limit).await;
}

async fn bind_build_pod_permit_job_uid() {}
async fn admit_task_run_dispatch() {}

async fn execute_runtime_report_phase() {
    let permit = acquire_build_pod_permit(app_state, spec).await;
    bind_build_pod_permit_job_uid(app_state, permit, &handle).await;
    admit_task_run_dispatch(app_state, permit, spec, &handle, bound).await;
    admission.record_dispatch_started(&spec.task_run_id);
}
EOF
}

fixture() {
    rm -rf -- "$SCRATCH"
    write_state
    write_bridge
    write_seam
    write_reconcile
}

run_guard() {
    GUARD_ROOT="$SCRATCH" sh "$GUARD" >"$SCRATCH/.out" 2>&1
}

expect_pass() {
    if run_guard; then
        pass "$1"
    else
        fail "$1 (guard rejected a clean tree)"
        sed 's/^/       /' "$SCRATCH/.out" >&2 || true
    fi
}

expect_fail_naming() {
    label=$1
    needle=$2
    if run_guard; then
        fail "$label (guard passed when it should have failed)"
    elif grep -q -- "$needle" "$SCRATCH/.out"; then
        pass "$label"
    else
        fail "$label (failed, but the message never named '$needle')"
        sed 's/^/       /' "$SCRATCH/.out" >&2 || true
    fi
}

printf 'check-resize-reachability self-test\n'

# 1. A clean tree passes. Without this every other case is meaningless.
fixture
expect_pass "a clean tree passes"

# 2. THE NAMED MUTATION: delete the production `acquire` call site.
fixture
grep -v 'permits.acquire(' "$SCRATCH/$SEAM" >"$SCRATCH/.tmp"
mv "$SCRATCH/.tmp" "$SCRATCH/$SEAM"
expect_fail_naming \
    "deleting the acquire call site fails, naming the symbol" \
    "BuildPodPermitRepository::acquire has ZERO production callers"

# 3. The call site exists but only below a `#[cfg(test)]` marker.
fixture
{
    printf '// Bridge fixture with everything demoted to test code.\n'
    printf 'pub struct TaskRunResizeBootstrap;\n'
    printf 'pub struct DispatchGate;\n'
    printf 'use djinn_db::BuildPodPermitRepository;\n'
    printf '#[cfg(test)]\n'
    printf 'mod tests {\n'
    printf '    async fn t() {\n'
    printf '        bootstrap.bootstrap(&permit, protocol).await;\n'
    printf '        gate.admit(&id);\n'
    printf '        permits.capture_resize_identity(a, b, c, d).await;\n'
    printf '    }\n'
    printf '}\n'
} >"$SCRATCH/$BRIDGE"
expect_fail_naming \
    "a call site below #[cfg(test)] does not count" \
    "TaskRunResizeBootstrap::bootstrap has ZERO production callers"

# 4. The call site exists but only in a `*_tests.rs` sibling.
fixture
grep -v 'capture_resize_identity' "$SCRATCH/$BRIDGE" >"$SCRATCH/.tmp"
mv "$SCRATCH/.tmp" "$SCRATCH/$BRIDGE"
cat >"$SCRATCH/server/src/task_run_resize_bootstrap_tests.rs" <<'EOF'
use djinn_db::BuildPodPermitRepository;
async fn t() {
    permits.capture_resize_identity(a, b, c, d).await;
}
EOF
expect_fail_naming \
    "a call site in a *_tests.rs file does not count" \
    "BuildPodPermitRepository::capture_resize_identity has ZERO production callers"

# 5. The call site exists but only under a `tests/` directory.
fixture
grep -v 'permits.acquire(' "$SCRATCH/$SEAM" >"$SCRATCH/.tmp"
mv "$SCRATCH/.tmp" "$SCRATCH/$SEAM"
mkdir -p -- "$SCRATCH/server/tests"
cat >"$SCRATCH/server/tests/seam.rs" <<'EOF'
use djinn_db::BuildPodPermitRepository;
async fn t() {
    permits.acquire(&id, limit).await;
}
EOF
expect_fail_naming \
    "a call site under tests/ does not count" \
    "BuildPodPermitRepository::acquire has ZERO production callers"

# 6. Every symbol is called, but nothing composes the caller. This is the
#    trait-override failure: reachable-looking code behind an object that is
#    never constructed.
fixture
grep -v 'TaskRunResizeAdmissionBridge::from_env' "$SCRATCH/$STATE" >"$SCRATCH/.tmp"
mv "$SCRATCH/.tmp" "$SCRATCH/$STATE"
expect_fail_naming \
    "a caller nobody composes fails on the composition anchor" \
    "TaskRunResizeAdmissionBridge::from_env"

# 7. The bridge is built but never threaded into the agent context.
fixture
grep -v 'resize_admission: Some(' "$SCRATCH/$STATE" >"$SCRATCH/.tmp"
mv "$SCRATCH/.tmp" "$SCRATCH/$STATE"
expect_fail_naming \
    "a bridge that reaches no AgentContext fails on the composition anchor" \
    "resize_admission: Some"

# 8. An anchor file was renamed away.
fixture
rm -f -- "$SCRATCH/$STATE"
expect_fail_naming \
    "a missing anchor file fails rather than passing vacuously" \
    "composition anchor file is missing"

# 9. The dispatch site stops reporting itself, so the gate's absence would be
#    unobservable even though the gate is still called.
fixture
grep -v 'record_dispatch_started' "$SCRATCH/$SEAM" >"$SCRATCH/.tmp"
mv "$SCRATCH/.tmp" "$SCRATCH/$SEAM"
expect_fail_naming \
    "dropping record_dispatch_started fails on the dispatch-site anchor" \
    "record_dispatch_started"

# 10. THE 0ppk-3 NAMED MUTATION: delete the reconciler spawn from become_leader.
#     A worker death would then strand its Pod forever, and nothing else in the
#     process would ever notice.
fixture
grep -v 'task_run_resize_reconcile::spawn(self.clone())' "$SCRATCH/$STATE" >"$SCRATCH/.tmp"
mv "$SCRATCH/.tmp" "$SCRATCH/$STATE"
expect_fail_naming \
    "deleting the reconciler spawn fails, naming the symbol" \
    "task_run_resize_reconcile::spawn has ZERO production callers"

# 11. `list_nonterminal_resize` loses its only production caller. This is
#     VERBATIM the state main was in before 0ppk-3: a durable read written for a
#     reconciler that did not exist, with zero callers anywhere but one
#     repository test.
fixture
grep -v 'list_nonterminal_resize' "$SCRATCH/$RECONCILE" >"$SCRATCH/.tmp"
mv "$SCRATCH/.tmp" "$SCRATCH/$RECONCILE"
expect_fail_naming \
    "a nonterminal-resize scan nobody calls does not count" \
    "BuildPodPermitRepository::list_nonterminal_resize has ZERO production callers"

# 12. The reconciler module exists and calls the scan, but nothing arms it from
#     become_leader. Reachable-looking code behind a loop nobody spawns.
fixture
grep -v 'task_run_resize_reconcile::spawn(self.clone())' "$SCRATCH/$STATE" >"$SCRATCH/.tmp"
mv "$SCRATCH/.tmp" "$SCRATCH/$STATE"
printf 'crate::task_run_resize_reconcile::spawn(other);\n' >>"$SCRATCH/$STATE"
expect_fail_naming \
    "a reconciler armed from anywhere but become_leader fails the anchor" \
    "task_run_resize_reconcile::spawn"

printf '\n%s passed, %s failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ] || exit 1
