use serde::{Deserialize, Serialize};

/// A configurable agent role, either a default base role or a user-defined specialist.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct AgentRole {
    pub id: String,
    pub project_id: String,
    pub name: String,
    /// One of: "worker", "lead", "planner", "architect", "reviewer", "resolver"
    pub base_role: String,
    pub description: String,
    pub system_prompt_extensions: String,
    pub model_preference: Option<String>,
    pub verification_command: Option<String>,
    /// JSON array of MCP server references (`[]` when none).
    pub mcp_servers: String,
    /// JSON array of skill names assigned to this role, e.g. `'["rust-expert","tdd"]'`.
    pub skills: String,
    /// `true` when this is the project-level default for its `base_role`.
    pub is_default: bool,
    /// Auto-improvement loop amendments — never modified by users directly.
    /// Appended after system_prompt_extensions in the session prompt.
    pub learned_prompt: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}
