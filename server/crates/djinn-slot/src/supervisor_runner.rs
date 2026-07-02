//! Default slot runner: delegates to [`SlotHostCallbacks::run_task_dispatch`].
//!
//! After the djinn-slot extraction, the supervisor dispatch logic
//! (runtime_bridge, supervisor, lifecycle stages, reply_loop) remains in
//! `djinn-agent` and is invoked through the host callback trait.  This module
//! provides the `run_supervisor_dispatch` function that the slot actor calls,
//! which simply delegates to the host.

use tokio_util::sync::CancellationToken;

use crate::host::SlotContext;

/// Default slot runner entry point.
///
/// Delegates to the host's `run_task_dispatch` callback, which encapsulates
/// the full supervisor dispatch logic (runtime selection, task-run lifecycle,
/// stage execution, reply loop, teardown). The final `resume_lifecycle_metadata`
/// argument is the optional coordinator-selected resume-via-git selection
/// blob; `None` for the default/off path so the legacy call sites compile
/// unchanged and the host callback receives `None` when resume selection is
/// disabled.
pub async fn run_supervisor_dispatch(
    task_id: String,
    project_path: String,
    model_id: String,
    ctx: SlotContext,
    kill: CancellationToken,
    pause: CancellationToken,
    resume_lifecycle_metadata: Option<serde_json::Value>,
) -> anyhow::Result<()> {
    ctx.callbacks
        .run_task_dispatch(
            task_id,
            project_path,
            model_id,
            ctx.clone(),
            kill,
            pause,
            resume_lifecycle_metadata,
        )
        .await
}
