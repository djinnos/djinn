use super::{RoleConfig, generic_mid_prompt, generic_pre_prompt, groomer_tools};

pub(super) fn build() -> RoleConfig {
    RoleConfig {
        name: "groomer",
        dispatch_role: "groomer",
        preserves_session: true,
        is_project_scoped: true,
        mid_session_compaction_prompt: generic_mid_prompt(),
        pre_resume_compaction_prompt: generic_pre_prompt(),
        tool_schemas: groomer_tools,
    }
}
