use crate::agent::compaction::{GENERIC_PROMPT, SUMMARISER_SYSTEM_GENERIC};
use crate::agent::prompts::PM_TEMPLATE;
use crate::tooling::schemas::pm_tool_schemas;

use super::{CompactionPrompts, RoleConfig};

pub(super) const PM_CONFIG: RoleConfig = RoleConfig {
    name: "pm",
    dispatch_role: "pm",
    tool_schemas: pm_tool_schemas,
    initial_message: PM_TEMPLATE,
    compaction: CompactionPrompts {
        mid_session: GENERIC_PROMPT,
        mid_session_system: SUMMARISER_SYSTEM_GENERIC,
        pre_resume: GENERIC_PROMPT,
        pre_resume_system: SUMMARISER_SYSTEM_GENERIC,
    },
    preserves_session: false,
    is_project_scoped: false,
};
