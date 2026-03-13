use crate::agent::compaction::{REVIEWER_PROMPT, SUMMARISER_SYSTEM_TASK_REVIEWER};
use crate::agent::extension;

use super::{CompactionPrompts, RoleConfig};

pub(crate) const TASK_REVIEWER_CONFIG: RoleConfig = RoleConfig {
    name: "task_reviewer",
    dispatch_role: "task_reviewer",
    tool_schemas: || extension::tool_schemas(crate::agent::AgentType::TaskReviewer),
    initial_message: crate::agent::prompts::TASK_REVIEWER_TEMPLATE,
    compaction: CompactionPrompts {
        mid_session: REVIEWER_PROMPT,
        mid_session_system: SUMMARISER_SYSTEM_TASK_REVIEWER,
        pre_resume: REVIEWER_PROMPT,
        pre_resume_system: SUMMARISER_SYSTEM_TASK_REVIEWER,
    },
    preserves_session: false,
    is_project_scoped: false,
};
