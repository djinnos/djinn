use std::path::Path;

use crate::context::AgentContext;
use crate::extension;
use crate::output_parser::ParsedAgentOutput;
use crate::prompts::TaskContext;
use djinn_core::models::{Task, TransitionAction};
use futures::future::BoxFuture;

use super::{AgentRole, RoleConfig};

pub(crate) struct ConflictResolverRole;

impl AgentRole for ConflictResolverRole {
    fn config(&self) -> &RoleConfig {
        &CONFLICT_RESOLVER_CONFIG
    }

    fn render_prompt(&self, task: &Task, ctx: &TaskContext) -> String {
        crate::prompts::render_prompt_for_role(self.config(), task, ctx)
    }

    fn on_complete<'a>(
        &'a self,
        _task_id: &'a str,
        _output: &'a ParsedAgentOutput,
        _app_state: &'a AgentContext,
    ) -> BoxFuture<'a, Option<(TransitionAction, Option<String>)>> {
        Box::pin(async { Some((TransitionAction::SubmitVerification, None)) })
    }

    fn prepare_worktree<'a>(
        &'a self,
        worktree: &'a Path,
        task: &'a Task,
        app_state: &'a AgentContext,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let conflict_ctx =
                crate::actors::slot::conflict_context_for_dispatch(&task.id, app_state).await;
            if let Some(ref ctx) = conflict_ctx {
                let target_ref = format!("origin/{}", ctx.merge_target);
                if let Ok(wt_git) = app_state.git_actor(worktree).await {
                    let _ = wt_git
                        .run_command(vec![
                            "fetch".into(),
                            "origin".into(),
                            ctx.merge_target.clone(),
                        ])
                        .await;
                    let merge_result = wt_git
                        .run_command(vec![
                            "merge".into(),
                            target_ref.clone(),
                            "--no-commit".into(),
                        ])
                        .await;
                    if merge_result.is_ok() {
                        let _ = wt_git
                            .run_command(vec!["merge".into(), "--abort".into()])
                            .await;
                    } else {
                        tracing::info!(
                            task_id = %task.short_id,
                            target_ref = %target_ref,
                            "ConflictResolverRole: started merge for conflict markers"
                        );
                    }
                }
            }
            Ok(())
        })
    }
}

pub(crate) const CONFLICT_RESOLVER_CONFIG: RoleConfig = RoleConfig {
    name: "conflict_resolver",
    display_name: "Conflict Resolver",
    dispatch_role: "worker",
    tool_schemas: extension::tool_schemas_worker,
    start_action: |status| match status {
        "open" => Some(TransitionAction::Start),
        _ => None,
    },
    release_action: || TransitionAction::Release,
    initial_message: crate::prompts::CONFLICT_RESOLVER_TEMPLATE,
    preserves_session: true,
    is_project_scoped: false,
};
