use super::{RoleConfig, generic_mid_prompt, generic_pre_prompt, pm_tools};

pub(super) fn build() -> RoleConfig {
    RoleConfig {
        name: "pm",
        dispatch_role: "pm",
        preserves_session: true,
        is_project_scoped: false,
        mid_session_compaction_prompt: generic_mid_prompt(),
        pre_resume_compaction_prompt: generic_pre_prompt(),
        tool_schemas: pm_tools,
    }
}
