use crate::agent::compaction::{GENERIC_PROMPT, SUMMARISER_SYSTEM_GENERIC};
use crate::agent::extension;
use crate::models::TransitionAction;

use super::{CompactionPrompts, RoleConfig};

pub(crate) const PM_CONFIG: RoleConfig = RoleConfig {
    name: "pm",
    display_name: "PM Intervention",
    dispatch_role: "pm",
    tool_schemas: || extension::tool_schemas(crate::agent::AgentType::PM),
    start_action: |status| match status {
        "needs_pm_intervention" => Some(TransitionAction::PmInterventionStart),
        _ => None,
    },
    release_action: || TransitionAction::PmInterventionRelease,
    initial_message: crate::agent::prompts::PM_TEMPLATE,
    compaction: CompactionPrompts {
        mid_session: GENERIC_PROMPT,
        mid_session_system: SUMMARISER_SYSTEM_GENERIC,
        pre_resume: GENERIC_PROMPT,
        pre_resume_system: SUMMARISER_SYSTEM_GENERIC,
    },
    preserves_session: false,
    is_project_scoped: false,
};
