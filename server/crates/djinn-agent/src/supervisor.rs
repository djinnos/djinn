//! Thin re-export shim for Phase 2 PR 2.
//!
//! The supervisor body was moved into its own crate
//! [`djinn-supervisor`](../djinn_supervisor/index.html) so the future
//! `djinn-agent-worker` binary can link against the orchestration loop
//! without pulling in `AgentContext`, the coordinator, the actor
//! framework, or the reply loop.
//!
//! This file re-exports every public symbol from that crate under the old
//! `djinn_agent::supervisor::*` paths so existing consumers keep compiling
//! unchanged.  The per-stage executor and the PR-open body still live
//! in-tree — see [`crate::supervisor_impl`] — and they are wired into
//! `djinn-supervisor` through `SupervisorServices`' closure seams by
//! [`crate::actors::slot::supervisor_runner::run_supervisor_dispatch`].
//!
//! ## Option chosen: **A (callback closures)**
//!
//! `djinn-supervisor::SupervisorServices` is a concrete struct holding three
//! `Arc<dyn Fn ...>` callbacks: `load_task_fn`, `execute_stage_fn`,
//! `open_pr_fn`.  `supervisor_runner.rs` binds them to the in-tree bodies
//! that compose the lifecycle helpers (`resolve_model_and_credential`,
//! `resolve_mcp_and_skills`, `resolve_setup_and_verification_context`,
//! `build_prompt_context`, `run_reply_loop`, `spawn_post_session_work`,
//! `squash_merge_via_mirror`, …).  Extracting those helpers into a new
//! `djinn-lifecycle` crate is deferred to a follow-up (tentatively PR 3's
//! companion change) so PR 2's diff stays focused on the crate split.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

// Re-export every supervisor symbol.  Consumers that imported
// `djinn_agent::supervisor::{TaskRunSupervisor, SupervisorServices,
// SupervisorError, StageOutcome, StageError, TaskRunSpec, TaskRunOutcome,
// TaskRunReport, RoleKind, SupervisorFlow, trigger_as_str, role_sequence}`
// keep resolving through this shim.
pub use djinn_supervisor::*;

use crate::context::AgentContext;
use crate::provider::LlmProvider;
use crate::supervisor_impl::SupervisorCallbackContext;
use crate::supervisor_impl::{execute_stage, supervisor_pr_open};

/// Build a [`SupervisorServices`] pre-wired with the in-tree `djinn-agent`
/// lifecycle bodies.
///
/// Replaces the old `SupervisorServices::new(agent_context, cancel)`
/// associated function — that constructor can no longer live on the struct
/// because the struct is defined in `djinn-supervisor`, which cannot see
/// `AgentContext`.  Call sites that need a `SupervisorServices` backed by a
/// real `AgentContext` (the supervisor runner, the `phase1_supervisor`
/// integration test) call this free function instead.
pub fn services_for_agent_context(
    agent_context: AgentContext,
    cancel: CancellationToken,
) -> SupervisorServices {
    build_services(agent_context, cancel, None)
}

/// Same as [`services_for_agent_context`] but installs a test-only
/// [`LlmProvider`] override on the stage executor, bypassing the catalog /
/// vault credential lookup inside `execute_stage`.
pub fn services_for_agent_context_with_provider_override(
    agent_context: AgentContext,
    cancel: CancellationToken,
    provider: Arc<dyn LlmProvider>,
) -> SupervisorServices {
    build_services(agent_context, cancel, Some(provider))
}

fn build_services(
    agent_context: AgentContext,
    cancel: CancellationToken,
    provider_override: Option<Arc<dyn LlmProvider>>,
) -> SupervisorServices {
    let callbacks = SupervisorCallbackContext {
        agent_context: agent_context.clone(),
        cancel: cancel.clone(),
        provider_override,
    };

    // ── load_task closure ────────────────────────────────────────────────
    let load_ctx = agent_context.clone();
    let load_task_fn: LoadTaskFn = Arc::new(move |task_id: String| {
        let ctx = load_ctx.clone();
        Box::pin(async move {
            crate::actors::slot::helpers::load_task(&task_id, &ctx)
                .await
                .map_err(|e| e.to_string())
        })
    });

    // ── execute_stage closure ────────────────────────────────────────────
    let stage_cb = callbacks.clone();
    let execute_stage_fn: ExecuteStageFn = Arc::new(
        move |task, workspace, role_kind, task_run_id, spec, services| {
            let cb = stage_cb.clone();
            Box::pin(async move {
                execute_stage(task, workspace, role_kind, task_run_id, spec, &cb, services).await
            })
        },
    );

    // ── open_pr closure ──────────────────────────────────────────────────
    let pr_cb = callbacks.clone();
    let open_pr_fn: OpenPrFn = Arc::new(move |spec, task, services| {
        let cb = pr_cb.clone();
        Box::pin(async move { supervisor_pr_open(spec, task, &cb, services).await })
    });

    SupervisorServices {
        cancel,
        load_task_fn,
        execute_stage_fn,
        open_pr_fn,
    }
}
