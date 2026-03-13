use crate::agent::compaction::{
    CONFLICT_RESOLVER_PROMPT, SUMMARISER_SYSTEM_CONFLICT_RESOLVER,
};
use crate::agent::prompts::CONFLICT_RESOLVER_TEMPLATE;
use crate::tooling::schemas::worker_tool_schemas;

use super::{CompactionPrompts, RoleConfig};

pub(super) const CONFLICT_RESOLVER_CONFIG: RoleConfig = RoleConfig {
    name: "conflict_resolver",
    dispatch_role: "worker",
    tool_schemas: worker_tool_schemas,
    initial_message: CONFLICT_RESOLVER_TEMPLATE,
    compaction: CompactionPrompts {
        mid_session: CONFLICT_RESOLVER_PROMPT,
        mid_session_system: SUMMARISER_SYSTEM_CONFLICT_RESOLVER,
        pre_resume: CONFLICT_RESOLVER_PROMPT,
        pre_resume_system: SUMMARISER_SYSTEM_CONFLICT_RESOLVER,
    },
    preserves_session: true,
    is_project_scoped: false,
};
