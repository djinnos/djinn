use crate::compaction::{GENERIC_PROMPT, SUMMARISER_SYSTEM_GENERIC};
use crate::extension;
use crate::output_parser::ParsedAgentOutput;
use crate::prompts::TaskContext;
use djinn_core::models::{Task, TransitionAction};
use crate::context::AgentContext;
use futures::future::BoxFuture;

use super::{AgentRole, CompactionPrompts, RoleConfig};

pub(crate) struct GroomerRole;

#[allow(dead_code)]
impl AgentRole for GroomerRole {
    fn config(&self) -> &RoleConfig {
        &GROOMER_CONFIG
    }

    fn render_prompt(&self, _task: &Task, ctx: &TaskContext) -> String {
        crate::prompts::render_project_prompt(
            crate::AgentType::Groomer,
            &ctx.project_path,
            ctx.verification_commands.as_deref(),
        )
    }

    fn on_complete<'a>(
        &'a self,
        _task_id: &'a str,
        _output: &'a ParsedAgentOutput,
        _app_state: &'a AgentContext,
    ) -> BoxFuture<'a, Option<(TransitionAction, Option<String>)>> {
        Box::pin(async { None })
    }
}

pub(crate) const GROOMER_CONFIG: RoleConfig = RoleConfig {
    name: "groomer",
    display_name: "Groomer",
    dispatch_role: "groomer",
    tool_schemas: extension::tool_schemas_pm_groomer,
    start_action: |_status| None,
    release_action: || TransitionAction::Release,
    initial_message: crate::prompts::GROOMER_TEMPLATE,
    compaction: CompactionPrompts {
        mid_session: GENERIC_PROMPT,
        mid_session_system: SUMMARISER_SYSTEM_GENERIC,
        pre_resume: GENERIC_PROMPT,
        pre_resume_system: SUMMARISER_SYSTEM_GENERIC,
    },
    preserves_session: false,
    is_project_scoped: true,
};
