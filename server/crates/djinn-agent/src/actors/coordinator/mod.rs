// Facade: coordinator logic lives in `djinn-coordinator`.
//
// This module re-exports the full coordinator surface so existing
// `djinn_agent::actors::coordinator::*` import paths keep resolving.
//
// `CoordinatorDeps` is intentionally NOT re-exported from djinn-coordinator
// because the host-side construction path uses djinn-agent types
// (`AgentContext`-based `SlotPoolHandle`, `RoleRegistry`) while
// djinn-coordinator expects its own types.  The local adapter below
// bridges that gap.

// Re-export everything except the items we shadow.
pub use djinn_coordinator::{
    AutoMergeTracker, BackgroundWorkTracker, BreakerDebugEntry, CoordinatorDebugSnapshot,
    CoordinatorError, CoordinatorHandle, CoordinatorStatus, DebugCooldown, DebugDispatchState,
    DebugFailureStreak, DebugInflightEntry, DebugSlot, DebugTotals, DispatchPauseView,
    PR_REVIEW_FEEDBACK_EVENT, PrCleanupConfig, record_supervisor_rework_reopen,
};

// Re-export public submodules.
pub use djinn_coordinator::{
    context, dispatch, dispatch_pause, doctor, environment, events, file_time, github_error_render,
    handle, messages, output_stash, pr_poller, resource_monitor, roles, rules, supervisor_impl,
    task_merge,
};

/// Agent-side adapter for [`djinn_coordinator::CoordinatorDeps`].
///
/// The host (djinn-server) constructs deps with djinn-agent types
/// (`SlotPoolHandle` from `actors::slot`, `RoleRegistry` from `roles`,
/// `LspManager` from `lsp`).  The coordinator crate expects its own
/// type-compatible but distinct types from `djinn-slot` and
/// `djinn-coordinator::roles`.
///
/// This adapter accepts the agent-side types and converts them when
/// [`CoordinatorHandle::spawn`] is called through the facade.
pub struct CoordinatorDeps {
    inner: djinn_coordinator::CoordinatorDeps,
}

impl CoordinatorDeps {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        events_tx: tokio::sync::broadcast::Sender<djinn_core::events::DjinnEventEnvelope>,
        cancel: tokio_util::sync::CancellationToken,
        db: djinn_db::Database,
        pool: crate::actors::slot::SlotPoolHandle,
        catalog: djinn_provider::catalog::CatalogService,
        health: djinn_provider::catalog::health::HealthTracker,
        role_registry: std::sync::Arc<crate::roles::RoleRegistry>,
        background_work_tracker: djinn_orchestration_types::coordinator::BackgroundWorkTracker,
        lsp: djinn_lsp::LspManager,
    ) -> Self {
        let djinn_pool = convert_pool_handle(pool);
        let djinn_role_registry = convert_role_registry(role_registry);
        Self {
            inner: djinn_coordinator::CoordinatorDeps::new(
                events_tx,
                cancel,
                db,
                djinn_pool,
                catalog,
                health,
                djinn_role_registry,
                background_work_tracker,
                lsp,
            ),
        }
    }

    pub fn with_graph_warmer(
        mut self,
        warmer: std::sync::Arc<dyn djinn_runtime::GraphWarmerService>,
    ) -> Self {
        self.inner = self.inner.with_graph_warmer(warmer);
        self
    }

    pub fn with_startup_census(
        mut self,
        census: djinn_coordinator::startup_census::StartupCensus,
    ) -> Self {
        self.inner = self.inner.with_startup_census(census);
        self
    }

    pub fn with_mirror(mut self, mirror: std::sync::Arc<djinn_workspace::MirrorManager>) -> Self {
        self.inner = self.inner.with_mirror(mirror);
        self
    }

    pub fn with_runtime_ops(
        mut self,
        ops: std::sync::Arc<dyn djinn_control_plane::bridge::RuntimeOps>,
    ) -> Self {
        self.inner = self.inner.with_runtime_ops(ops);
        self
    }

    pub fn with_rpc_registry(
        mut self,
        registry: std::sync::Arc<djinn_supervisor::ConnectionRegistry>,
    ) -> Self {
        self.inner = self.inner.with_rpc_registry(registry);
        self
    }

    /// Share the leader's provider-action scope (proposal `nafu`, wave 3).
    pub fn with_provider_action_scope(
        mut self,
        scope: djinn_orchestration_types::ProviderActionScope,
    ) -> Self {
        self.inner = self.inner.with_provider_action_scope(scope);
        self
    }
}

/// Spawn a coordinator actor from agent-side deps.
///
/// This is the facade entry point that replaces
/// `CoordinatorHandle::spawn(CoordinatorDeps)` so the server code
/// continues to compile with djinn-agent types.
pub fn spawn_coordinator(deps: CoordinatorDeps) -> CoordinatorHandle {
    djinn_coordinator::CoordinatorHandle::spawn(deps.inner)
}

// ─── Type conversion helpers ────────────────────────────────────────────────

/// Convert an agent-side `SlotPoolHandle` to a djinn-slot `SlotPoolHandle`.
///
/// Production agent pool handles now wrap the canonical `djinn-slot` handle
/// directly; unwrap that canonical handle for the coordinator crate. Test-only
/// legacy factory handles are intentionally rejected because they do not own a
/// canonical slot-pool actor.
fn convert_pool_handle(handle: crate::actors::slot::SlotPoolHandle) -> djinn_slot::SlotPoolHandle {
    handle.into_djinn_slot().expect(
        "agent slot pool facade was constructed with a test-only legacy factory; \
         coordinator facade requires the canonical djinn-slot pool handle",
    )
}

/// Convert an agent-side `RoleRegistry` to a djinn-coordinator `RoleRegistry`.
///
/// Both types have the same fields: `HashMap<&'static str, AgentType>` and
/// `Vec<DispatchRule>`.  The `AgentType` and `DispatchRule` types are
/// structurally identical across crates.
fn convert_role_registry(
    registry: std::sync::Arc<crate::roles::RoleRegistry>,
) -> std::sync::Arc<djinn_coordinator::roles::RoleRegistry> {
    // SAFETY: Both RoleRegistry types are repr(Rust) structs with identical
    // fields (roles: HashMap, dispatch_rules: Vec).  The contained types
    // (AgentType, DispatchRule) are also structurally identical across crates.
    unsafe { std::mem::transmute(registry) }
}

// ─── Facade smoke tests ────────────────────────────────────────────────────
//
// The canonical coordinator logic and its primary test coverage live in
// `djinn-coordinator` (see `djinn-coordinator/src/tests/`).  These tests
// verify that the agent's coordinator facade re-exports resolve correctly
// through the `djinn_agent::actors::coordinator::*` import paths.

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that the key public re-exports from `djinn-coordinator`
    /// resolve through the `djinn-agent` facade.
    #[test]
    fn facade_reexports_resolve() {
        // Types re-exported from djinn-coordinator
        let _: CoordinatorHandle;
        let _: CoordinatorError;
        let _: CoordinatorStatus;
        let _: CoordinatorDebugSnapshot;
        let _: DebugSlot;
        let _: DispatchPauseView;
        let _: BackgroundWorkTracker;

        // Adapter struct local to the facade
        let _deps_ty = std::any::type_name::<CoordinatorDeps>();

        // Submodule re-exports (module paths resolve at type level)
        let _handle = std::any::type_name::<handle::CoordinatorHandle>();
    }

    /// Verify the facade spawn helper is callable (type-level check).
    #[test]
    fn spawn_coordinator_fn_exists() {
        // Just verify the function signature is visible; don't call it.
        let _: fn(CoordinatorDeps) -> CoordinatorHandle = spawn_coordinator;
    }
}

// ─── The provider-action scope hop (proposal `nafu`, S28) ───────────────────

/// The CI-route provider-action scope handoff across this facade.
///
/// # Why this module carries `ci_routing` in its name
///
/// `nafu`'s acceptance list reaches this crate through
/// `cargo test -p djinn-agent --lib ci_routing`, which filters on the test
/// path. A witness for the scope hop that no acceptance command executes is a
/// witness that cannot fail, which is exactly the gap this test closes. The
/// name is also what the thing is: `CoordinatorDeps::provider_action_scope` is
/// documented in `djinn-coordinator` as "the leader-scoped registry every
/// CI-route provider mutation is admitted into".
#[cfg(test)]
mod ci_routing_scope_handoff_tests {
    use super::*;

    /// The scope handed to this facade is the scope the coordinator holds.
    ///
    /// BEHAVIOURAL, and it can be: `ProviderActionScope` is an `Arc`-backed
    /// handle, so "the same object" is directly observable without adding an
    /// accessor to `CoordinatorHandle` — admit an action through the handle the
    /// caller kept, then read `in_flight()` off the handle that landed inside
    /// `djinn_coordinator::CoordinatorDeps`. A private copy reports 0; the same
    /// object reports 1.
    ///
    /// # What the mutation costs in production
    ///
    /// Replace [`CoordinatorDeps::with_provider_action_scope`]'s body with
    /// `let _ = scope; self` and nothing fails to compile and nothing logs:
    /// `djinn_coordinator::CoordinatorDeps::new` seeds a **private**
    /// `ProviderActionScope::new()`, so the coordinator silently keeps that
    /// one. `rerun_failed_jobs` and `enable_auto_merge` futures are then
    /// admitted into a scope leadership never sees;
    /// `quiesce_provider_actions` finds an empty scope and returns
    /// immediately; and the coordinator advisory lock is released while a
    /// provider future from this incarnation is still live. That is the
    /// exclusion AC3 states in terms, and the evidence acceptance command 6
    /// exists to supply. Command 6's own fixture,
    /// `the_leader_scope_and_the_coordinator_scope_are_one_object`, asserted
    /// only over `server/src/…` and so stopped at `AppState`'s builder call —
    /// the forwarder is one crate further in. It now carries a source-level
    /// cross-check of this same hop; this test is the behavioural half.
    ///
    /// NAMED FAILING MUTATIONS.
    /// (a) `let _ = scope; self`, or deleting the `self.inner = …` assignment:
    ///     `deps` then holds the private scope `CoordinatorDeps::new` seeded,
    ///     `in_flight()` is 0 while a guard is alive, and the first assertion
    ///     fails.
    /// (b) `self.inner.with_provider_action_scope(ProviderActionScope::new())`
    ///     — forwarding *a* scope rather than *the* scope: identical failure.
    /// (c) Making `ProviderActionScope::clone` deep-copy instead of sharing the
    ///     `Arc`: the first assertion fails, and it must — every waiter in
    ///     `nafu` would be watching its own private set of futures.
    ///
    /// The third assertion (the count falls back to 0 when the guard drops) is
    /// what stops a mutation that hands the coordinator a *pre-loaded* scope
    /// from passing: the observation has to track the guard's lifetime, not
    /// merely be non-zero once.
    #[tokio::test]
    async fn the_scope_handed_to_the_facade_is_the_scope_the_coordinator_holds() {
        let db = crate::test_helpers::create_test_db();
        let cancel = tokio_util::sync::CancellationToken::new();
        let agent = crate::test_helpers::agent_context_from_db(db.clone(), cancel.clone());
        let pool = crate::actors::slot::SlotPoolHandle::spawn(
            agent.clone(),
            cancel.clone(),
            crate::actors::slot::SlotPoolConfig {
                models: Vec::new(),
                role_priorities: std::collections::HashMap::new(),
            },
        );
        let (events_tx, _events_rx) = tokio::sync::broadcast::channel(8);

        // The scope `AppState` owns and `leadership.rs` waits on.
        let leader = djinn_orchestration_types::ProviderActionScope::new();
        let deps = CoordinatorDeps::new(
            events_tx,
            cancel,
            db,
            pool,
            agent.catalog.clone(),
            agent.health_tracker.clone(),
            agent.role_registry.clone(),
            agent.background_work_tasks.clone(),
            agent.lsp.clone(),
        )
        .with_provider_action_scope(leader.clone());

        // Vacuity guard first: `in_flight` is an observation, not a constant,
        // and an untouched scope reports nothing. Without this the assertion
        // below would also hold for an accessor that always answered 1.
        assert_eq!(
            deps.inner.provider_action_scope.in_flight(),
            0,
            "control: nothing has been admitted yet",
        );

        let admitted = leader
            .admit()
            .expect("an open scope admits; the fixture never closes it");

        assert_eq!(
            deps.inner.provider_action_scope.in_flight(),
            1,
            "the coordinator must hold the SAME scope object the caller kept: \
             an action admitted through the leader's handle has to be visible \
             through the coordinator's, or leadership waits on an empty scope \
             and releases the advisory lock with a provider future still live",
        );
        assert_eq!(
            leader.counts().admitted_total,
            deps.inner.provider_action_scope.counts().admitted_total,
            "and both handles must report one shared ledger, not two that \
             happen to agree on the live count",
        );

        drop(admitted);
        assert_eq!(
            deps.inner.provider_action_scope.in_flight(),
            0,
            "the coordinator's view must follow the guard's lifetime — that is \
             precisely what `quiesce_provider_actions` waits on before the lock \
             is released",
        );
    }
}
