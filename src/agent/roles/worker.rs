use crate::agent::compaction::{
    MID_SESSION_WORKER_PROMPT, PRE_RESUME_WORKER_PROMPT, SUMMARISER_SYSTEM_WORKER_MID_SESSION,
    SUMMARISER_SYSTEM_WORKER_PRE_RESUME,
};
use crate::agent::prompts::DEV_TEMPLATE;
use crate::tooling::schemas::worker_tool_schemas;

use super::{CompactionPrompts, RoleConfig};

pub(super) const WORKER_CONFIG: RoleConfig = RoleConfig {
    name: "worker",
    dispatch_role: "worker",
    tool_schemas: worker_tool_schemas,
    initial_message: DEV_TEMPLATE,
    compaction: CompactionPrompts {
        mid_session: MID_SESSION_WORKER_PROMPT,
        mid_session_system: SUMMARISER_SYSTEM_WORKER_MID_SESSION,
        pre_resume: PRE_RESUME_WORKER_PROMPT,
        pre_resume_system: SUMMARISER_SYSTEM_WORKER_PRE_RESUME,
    },
    preserves_session: true,
    is_project_scoped: false,
};
