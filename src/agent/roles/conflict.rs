use super::{RoleConfig, conflict_prompt, conflict_tools};

pub(super) fn build() -> RoleConfig {
    RoleConfig {
        name: "conflict_resolver",
        dispatch_role: "worker",
        preserves_session: true,
        is_project_scoped: false,
        mid_session_compaction_prompt: conflict_prompt(),
        pre_resume_compaction_prompt: conflict_prompt(),
        tool_schemas: conflict_tools,
    }
}
