use super::{RoleConfig, worker_mid_prompt, worker_pre_prompt, worker_tools};

pub(super) fn build() -> RoleConfig {
    RoleConfig {
        name: "worker",
        dispatch_role: "worker",
        preserves_session: true,
        is_project_scoped: false,
        mid_session_compaction_prompt: worker_mid_prompt(),
        pre_resume_compaction_prompt: worker_pre_prompt(),
        tool_schemas: worker_tools,
    }
}
