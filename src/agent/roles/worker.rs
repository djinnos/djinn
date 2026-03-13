use crate::agent::compaction::{
    MID_SESSION_WORKER_PROMPT, PRE_RESUME_WORKER_PROMPT, SUMMARISER_SYSTEM_WORKER_MID_SESSION,
    SUMMARISER_SYSTEM_WORKER_PRE_RESUME,
};
use crate::agent::extension;
use crate::models::TransitionAction;

use super::{CompactionPrompts, RoleConfig};

pub(crate) const WORKER_CONFIG: RoleConfig = RoleConfig {
    name: "worker",
    display_name: "Developer",
    dispatch_role: "worker",
    tool_schemas: || extension::tool_schemas(crate::agent::AgentType::Worker),
    start_action: |status| match status {
        "open" => Some(TransitionAction::Start),
        _ => None,
    },
    release_action: || TransitionAction::Release,
    initial_message: crate::agent::prompts::DEV_TEMPLATE,
    compaction: CompactionPrompts {
        mid_session: MID_SESSION_WORKER_PROMPT,
        mid_session_system: SUMMARISER_SYSTEM_WORKER_MID_SESSION,
        pre_resume: PRE_RESUME_WORKER_PROMPT,
        pre_resume_system: SUMMARISER_SYSTEM_WORKER_PRE_RESUME,
    },
    preserves_session: true,
    is_project_scoped: false,
};
