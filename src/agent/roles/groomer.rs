use crate::agent::compaction::{GENERIC_PROMPT, SUMMARISER_SYSTEM_GENERIC};
use crate::agent::extension;

use super::{CompactionPrompts, RoleConfig};

pub(crate) const GROOMER_CONFIG: RoleConfig = RoleConfig {
    name: "groomer",
    dispatch_role: "groomer",
    tool_schemas: || extension::tool_schemas(crate::agent::AgentType::Groomer),
    initial_message: crate::agent::prompts::GROOMER_TEMPLATE,
    compaction: CompactionPrompts {
        mid_session: GENERIC_PROMPT,
        mid_session_system: SUMMARISER_SYSTEM_GENERIC,
        pre_resume: GENERIC_PROMPT,
        pre_resume_system: SUMMARISER_SYSTEM_GENERIC,
    },
    preserves_session: false,
    is_project_scoped: true,
};
