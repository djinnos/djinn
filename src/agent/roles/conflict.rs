use crate::agent::compaction::{CONFLICT_RESOLVER_PROMPT, SUMMARISER_SYSTEM_CONFLICT_RESOLVER};
use crate::agent::extension;
use crate::models::TransitionAction;

use super::{CompactionPrompts, RoleConfig};

pub(crate) const CONFLICT_RESOLVER_CONFIG: RoleConfig = RoleConfig {
    name: "conflict_resolver",
    display_name: "Conflict Resolver",
    dispatch_role: "worker",
    tool_schemas: || extension::tool_schemas(crate::agent::AgentType::ConflictResolver),
    start_action: |status| match status {
        "open" => Some(TransitionAction::Start),
        _ => None,
    },
    release_action: || TransitionAction::Release,
    initial_message: crate::agent::prompts::CONFLICT_RESOLVER_TEMPLATE,
    compaction: CompactionPrompts {
        mid_session: CONFLICT_RESOLVER_PROMPT,
        mid_session_system: SUMMARISER_SYSTEM_CONFLICT_RESOLVER,
        pre_resume: CONFLICT_RESOLVER_PROMPT,
        pre_resume_system: SUMMARISER_SYSTEM_CONFLICT_RESOLVER,
    },
    preserves_session: true,
    is_project_scoped: false,
};
