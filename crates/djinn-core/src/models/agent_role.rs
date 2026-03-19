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
    /// JSON array of MCP server refs
    pub mcp_servers: String,
    /// JSON array of skill refs
    pub skills: String,
    /// Whether this is the default instance for its base_role
    pub is_default: bool,
    pub created_at: String,
    pub updated_at: String,
}
