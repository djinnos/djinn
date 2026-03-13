use crate::agent::compaction::{GENERIC_PROMPT, SUMMARISER_SYSTEM_GENERIC};
use crate::agent::prompts::GROOMER_TEMPLATE;
use crate::tooling::schemas::groomer_tool_schemas;

use super::{CompactionPrompts, RoleConfig};

pub(super) const GROOMER_CONFIG: RoleConfig = RoleConfig {
    name: "groomer",
    dispatch_role: "groomer",
    tool_schemas: groomer_tool_schemas,
    initial_message: GROOMER_TEMPLATE,
    compaction: CompactionPrompts {
        mid_session: GENERIC_PROMPT,
        mid_session_system: SUMMARISER_SYSTEM_GENERIC,
        pre_resume: GENERIC_PROMPT,
        pre_resume_system: SUMMARISER_SYSTEM_GENERIC,
    },
    preserves_session: false,
    is_project_scoped: true,
};
