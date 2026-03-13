use super::{RoleConfig, reviewer_prompt, reviewer_tools};

pub(super) fn build() -> RoleConfig {
    RoleConfig {
        name: "task_reviewer",
        dispatch_role: "task_reviewer",
        preserves_session: false,
        is_project_scoped: false,
        mid_session_compaction_prompt: reviewer_prompt(),
        pre_resume_compaction_prompt: reviewer_prompt(),
        tool_schemas: reviewer_tools,
    }
}
