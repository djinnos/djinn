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
pub use djinn_coordinator::build_admission::{BuildAdmissionController, BuildAdmissionMode};
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

    pub fn with_build_admission(
        mut self,
        controller: std::sync::Arc<BuildAdmissionController>,
    ) -> Self {
        self.inner = self.inner.with_build_admission(controller);
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
