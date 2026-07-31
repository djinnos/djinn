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
# It also pins the inverse, which is what the guard got WRONG: production code
# that merely SITS AFTER a `#[cfg(test)]` attribute — on a struct field, or
# after a closed test module — is still production code. The first-marker
# heuristic reported "ZERO production callers" about a call the guard's own
# anchor check found in the same file in the same run.
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
CUTOVER=server/src/authority_cutover.rs
CUTOVER_BIN=server/src/bin/authority_cutover.rs
ROLLOUT=server/src/task_run_resize_rollout.rs
MANIFEST=server/Cargo.toml

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

# The composition root as it ACTUALLY looks: a `#[cfg(test)]` attribute on a
# struct field near the top, and a closed `#[cfg(test)] mod tests { }` block,
# both ABOVE the production call site. Every line below them is production.
write_state_with_test_markers_above_the_call_site() {
    mkdir -p -- "$SCRATCH/$(dirname "$STATE")"
    cat >"$SCRATCH/$STATE" <<'EOF'
// Composition root fixture, with test markers above the call site.
struct Inner {
    #[cfg(test)]
    pub image_controller: RwLock<Option<Arc<ImageController>>>,
    pub resize_admission: Arc<TaskRunResizeAdmissionBridge>,
}
fn new_inner() {
    let resize_admission =
        Arc::new(TaskRunResizeAdmissionBridge::from_env(db.clone()));
}
#[cfg(test)]
mod early_tests {
    #[test]
    fn t() {
        crate::task_run_resize_reconcile::spawn(fake);
    }
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

write_cutover() {
    mkdir -p -- "$SCRATCH/$(dirname "$CUTOVER")"
    cat >"$SCRATCH/$CUTOVER" <<'EOF'
// Operator cutover driver fixture: the ONLY production caller of
// ResizeRollout::production, running both sequences through ResizeRollout.
use crate::task_run_resize_rollout::{ResizeRollout, RolloutPlan};

pub async fn run(db: Database, request: &CutoverRequest) -> Result<(), CutoverFailure> {
    let rollout = ResizeRollout::production(db, events, runtime, url, paused_by, &sources)?;
    let outcome = match request.direction {
        CutoverDirection::Activate => rollout.activate(&plan).await,
        CutoverDirection::Rollback => rollout.rollback(&plan).await,
    };
}
EOF

    mkdir -p -- "$SCRATCH/$(dirname "$CUTOVER_BIN")"
    cat >"$SCRATCH/$CUTOVER_BIN" <<'EOF'
// Operator binary fixture.
use djinn_server::authority_cutover::{CutoverFailure, CutoverRequest, run};

async fn drive() -> Result<(), CutoverFailure> {
    let report = run(db, events, runtime, &request).await?;
    Ok(())
}
EOF

    mkdir -p -- "$SCRATCH/$(dirname "$ROLLOUT")"
    cat >"$SCRATCH/$ROLLOUT" <<'EOF'
// Rollout fixture: the preflight gates both flips.
impl ResizeRollout {
    pub async fn activate(&self, plan: &RolloutPlan<'_>) -> Result<i64, RolloutBlocked> {
        self.prove_drained().await?;
        self.clear_preflight(LauncherAuthorityProtocol::ResizeV2).await?;
        self.flip_authority_mode(plan.expected_epoch, LauncherAuthorityProtocol::ResizeV2).await
    }
    pub async fn rollback(&self, plan: &RolloutPlan<'_>) -> Result<i64, RolloutBlocked> {
        self.prove_drained().await?;
        self.clear_preflight(LauncherAuthorityProtocol::LeafV1).await?;
        self.flip_authority_mode(plan.expected_epoch, LauncherAuthorityProtocol::LeafV1).await
    }
}
EOF

    mkdir -p -- "$SCRATCH/$(dirname "$MANIFEST")"
    cat >"$SCRATCH/$MANIFEST" <<'EOF'
[[bin]]
name = "authority-cutover"
path = "src/bin/authority_cutover.rs"
EOF
}

fixture() {
    rm -rf -- "$SCRATCH"
    write_state
    write_bridge
    write_seam
    write_reconcile
    write_cutover
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

# 13. THE GUARD'S OWN BUG: production code after a `#[cfg(test)]` attribute is
#     still production code. `server/src/server/state/mod.rs` carries a
#     `#[cfg(test)]` on a STRUCT FIELD 1800 lines above `become_leader`, and a
#     first-marker heuristic reported "ZERO production callers" about a call the
#     guard's own ANCHOR check found in that same file in that same run.
fixture
write_state_with_test_markers_above_the_call_site
expect_pass "production code after a #[cfg(test)] block is still production"

# 14. And the tracker has not simply stopped looking at test code: with ONLY the
#     in-test call site left, the guard must still fail. Case 13 would pass
#     vacuously if the scanner had been widened to count test callers too.
fixture
write_state_with_test_markers_above_the_call_site
grep -v 'task_run_resize_reconcile::spawn(self.clone())' "$SCRATCH/$STATE" >"$SCRATCH/.tmp"
mv "$SCRATCH/.tmp" "$SCRATCH/$STATE"
expect_fail_naming \
    "a spawn that survives only inside a #[cfg(test)] mod does not count" \
    "task_run_resize_reconcile::spawn has ZERO production callers"

# 15. THE `eeky-2` NAMED MUTATION: delete the ResizeRollout::production call.
#     This is verbatim the state main was in before the operator entry point
#     landed — the staged activation composed, tested, and callable by nobody.
fixture
grep -v 'ResizeRollout::production(' "$SCRATCH/$CUTOVER" >"$SCRATCH/.tmp"
mv "$SCRATCH/.tmp" "$SCRATCH/$CUTOVER"
expect_fail_naming \
    "deleting the ResizeRollout::production call fails, naming the symbol" \
    "ResizeRollout::production has ZERO production callers"

# 16. The call survives only inside a `#[cfg(test)]` module — which is where it
#     effectively lived before, as an assertion that the constructor existed.
fixture
cat >"$SCRATCH/$CUTOVER" <<'EOF'
// Driver fixture with the composition demoted to a test.
use crate::task_run_resize_rollout::{ResizeRollout, RolloutPlan};

#[cfg(test)]
mod tests {
    #[test]
    fn the_constructor_exists() {
        let rollout = ResizeRollout::production(db, events, runtime, url, paused_by, &sources);
        rollout.activate(&plan);
        rollout.rollback(&plan);
    }
}
EOF
expect_fail_naming \
    "a ResizeRollout::production call that survives only in #[cfg(test)] does not count" \
    "ResizeRollout::production has ZERO production callers"

# 17. The driver exists and calls `production`, but no binary calls the driver.
#     Reachable-looking code inside a function no `main` reaches.
fixture
grep -v 'let report = run(' "$SCRATCH/$CUTOVER_BIN" >"$SCRATCH/.tmp"
mv "$SCRATCH/.tmp" "$SCRATCH/$CUTOVER_BIN"
expect_fail_naming \
    "a cutover driver no binary calls fails on the composition anchor" \
    "let report = run"

# 18. The binary source exists but cargo does not build it. A file under
#     `src/bin/` that is not a declared target ships nothing.
fixture
grep -v 'authority-cutover' "$SCRATCH/$MANIFEST" >"$SCRATCH/.tmp"
mv "$SCRATCH/.tmp" "$SCRATCH/$MANIFEST"
expect_fail_naming \
    "an undeclared binary target fails on the manifest anchor" \
    "authority-cutover"

# 19. The driver runs the sequences through something other than ResizeRollout —
#     `set_mode` directly, say, which has its own fence but refuses with a bare
#     census and names no row.
fixture
grep -v 'rollout.activate(&plan)' "$SCRATCH/$CUTOVER" >"$SCRATCH/.tmp"
mv "$SCRATCH/.tmp" "$SCRATCH/$CUTOVER"
expect_fail_naming \
    "a driver that does not run activate through ResizeRollout fails the anchor" \
    "must run the forward sequence through ResizeRollout"

# 19b. …and the reverse, guarded separately: rollback is the path that must never
#      start an incompatible Pod, and `set_mode` alone refuses with a bare census.
fixture
grep -v 'rollout.rollback(&plan)' "$SCRATCH/$CUTOVER" >"$SCRATCH/.tmp"
mv "$SCRATCH/.tmp" "$SCRATCH/$CUTOVER"
expect_fail_naming \
    "a driver that does not run rollback through ResizeRollout fails the anchor" \
    "must run the reverse sequence through ResizeRollout"

# 20. The forward flip stops running the preflight. The gate would still exist,
#     still be tested, and gate nothing.
fixture
grep -v 'clear_preflight(LauncherAuthorityProtocol::ResizeV2)' "$SCRATCH/$ROLLOUT" >"$SCRATCH/.tmp"
mv "$SCRATCH/.tmp" "$SCRATCH/$ROLLOUT"
expect_fail_naming \
    "dropping the forward preflight call fails the anchor" \
    "clear_preflight"

# 21. …and the reverse flip likewise. Rollback is the path that must never start
#     an incompatible Pod, so its gate is guarded separately rather than assumed
#     to travel with the forward one.
fixture
grep -v 'clear_preflight(LauncherAuthorityProtocol::LeafV1)' "$SCRATCH/$ROLLOUT" >"$SCRATCH/.tmp"
mv "$SCRATCH/.tmp" "$SCRATCH/$ROLLOUT"
expect_fail_naming \
    "dropping the reverse preflight call fails the anchor" \
    "clear_preflight"

printf '\n%s passed, %s failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ] || exit 1
