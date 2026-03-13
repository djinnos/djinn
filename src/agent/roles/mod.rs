use crate::agent::extension::ToolSchema;

#[derive(Debug, Clone)]
pub struct RoleConfig {
    pub name: &'static str,
    pub dispatch_role: &'static str,
    pub tool_schemas: Vec<ToolSchema>,
    pub initial_message: &'static str,
    pub compaction: CompactionPrompts,
    pub preserves_session: bool,
    pub is_project_scoped: bool,
}

#[derive(Debug, Clone)]
pub struct CompactionPrompts {
    pub mid_session: &'static str,
    pub mid_session_system: &'static str,
    pub pre_resume: &'static str,
    pub pre_resume_system: &'static str,
}
