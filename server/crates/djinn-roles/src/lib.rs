//! Role configuration, prompt rendering, and `AgentType` for Djinn agents.
//!
//! This crate owns the public data model for agent roles (`AgentType`,
//! `RoleConfig`, prompt templates and rendering) so it can be depended on
//! by crates that need role metadata without pulling in the full
//! `djinn-agent` actor/extension stack.
//!
//! The `tool_schemas` function-pointer seam on `RoleConfig` is populated
//! at startup via [`register_tool_schemas`].  The concrete schema
//! providers live in `djinn-agent::extension` (or a future
//! `djinn-mcp-extension` crate) and are registered once during
//! initialization, avoiding a `djinn-roles → extension` dependency.

pub mod config;
pub mod prompts;

use std::collections::HashMap;
use std::sync::OnceLock;

// Re-export the prompt templates so callers can access them through
// `djinn_roles::DEV_TEMPLATE` etc.
pub use prompts::{
    ARCHITECT_TEMPLATE, BASE_TEMPLATE, CLUSTER_DOC_TEMPLATE, DEV_TEMPLATE, LEAD_TEMPLATE,
    PLANNER_TEMPLATE, REVIEWER_TEMPLATE,
};

/// Role an agent is playing within Djinn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentType {
    Worker,
    Reviewer,
    Lead,
    Planner,
    /// Architect: handles spike, review tasks and proactive health monitoring (ADR-034).
    Architect,
}

impl AgentType {
    pub fn role_config(&self) -> &'static config::RoleConfig {
        config::config_for(*self)
    }

    pub fn as_str(&self) -> &'static str {
        self.role_config().name
    }

    pub fn for_task_status(status: &str, _has_conflict_context: bool) -> Self {
        match status {
            "needs_task_review" | "in_task_review" => Self::Reviewer,
            "needs_lead_intervention" | "in_lead_intervention" => Self::Lead,
            _ => Self::Worker,
        }
    }

    pub fn dispatch_role(&self) -> &'static str {
        self.role_config().dispatch_role
    }

    #[cfg(test)]
    pub fn tool_schemas(&self) -> Vec<serde_json::Value> {
        tool_schemas_for(*self)
    }

    /// Parse from a DB/wire string, including the `architect` variant.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "worker" => Some(Self::Worker),
            "reviewer" => Some(Self::Reviewer),
            "lead" => Some(Self::Lead),
            "planner" => Some(Self::Planner),
            "architect" => Some(Self::Architect),
            _ => None,
        }
    }
}

impl std::str::FromStr for AgentType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s).ok_or_else(|| format!("unknown agent type: {s}"))
    }
}

impl serde::Serialize for AgentType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for AgentType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = <String as serde::Deserialize>::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

// ─── Tool schema registry (inverted dependency seam) ────────────────────────

/// Type alias for a tool schema provider function.
type ToolSchemaFn = fn() -> Vec<serde_json::Value>;

/// Registry of tool schema provider functions, keyed by role name.
///
/// Populated once at startup by `djinn-agent` via [`register_tool_schemas`].
/// This avoids a compile-time dependency from `djinn-roles` → `djinn-agent`
/// (or the future `djinn-mcp-extension` crate) while preserving the existing
/// `RoleConfig.tool_schemas: fn()` seam.
static TOOL_SCHEMAS_REGISTRY: OnceLock<HashMap<&'static str, ToolSchemaFn>> = OnceLock::new();

/// Register tool schema provider functions for all roles.
///
/// Must be called once at startup before any prompt rendering that requires
/// tool schemas.  Typically called from `djinn-agent`'s initialization path.
///
/// # Panics
///
/// Panics if called more than once.
pub fn register_tool_schemas(schemas: HashMap<&'static str, ToolSchemaFn>) {
    TOOL_SCHEMAS_REGISTRY
        .set(schemas)
        .expect("tool schemas already registered");
}

/// Resolve the tool schema provider function for an `AgentType`.
///
/// Returns `None` if the registry has not been initialized (call
/// [`register_tool_schemas`] first) or if the role name is not registered.
pub fn tool_schemas_fn_for(agent_type: AgentType) -> Option<ToolSchemaFn> {
    TOOL_SCHEMAS_REGISTRY.get().and_then(|reg| {
        let name = agent_type.role_config().name;
        reg.get(name).copied()
    })
}

/// Convenience: call the registered tool schema provider for `agent_type`.
///
/// Returns an empty vector if the registry is not initialized or the role
/// is not registered.
pub fn tool_schemas_for(agent_type: AgentType) -> Vec<serde_json::Value> {
    tool_schemas_fn_for(agent_type)
        .map(|f| f())
        .unwrap_or_default()
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_equivalent_to_role_config(agent_type: AgentType) {
        let cfg = agent_type.role_config();
        assert_eq!(agent_type.as_str(), cfg.name);
        assert_eq!(agent_type.dispatch_role(), cfg.dispatch_role);
    }

    #[test]
    fn role_config_equivalence_for_all_agent_types() {
        for agent_type in [
            AgentType::Worker,
            AgentType::Reviewer,
            AgentType::Lead,
            AgentType::Planner,
            AgentType::Architect,
        ] {
            assert_equivalent_to_role_config(agent_type);
        }
    }

    #[test]
    fn for_task_status_covers_all_expected_paths() {
        assert_eq!(AgentType::for_task_status("open", false), AgentType::Worker);
        assert_eq!(AgentType::for_task_status("open", true), AgentType::Worker);
        assert_eq!(
            AgentType::for_task_status("needs_task_review", false),
            AgentType::Reviewer
        );
        assert_eq!(
            AgentType::for_task_status("in_task_review", false),
            AgentType::Reviewer
        );
        assert_eq!(
            AgentType::for_task_status("needs_lead_intervention", false),
            AgentType::Lead
        );
        assert_eq!(
            AgentType::for_task_status("in_lead_intervention", false),
            AgentType::Lead
        );
    }

    #[test]
    fn dispatch_role_for_all_variants() {
        assert_eq!(AgentType::Worker.dispatch_role(), "worker");
        assert_eq!(AgentType::Reviewer.dispatch_role(), "reviewer");
        assert_eq!(AgentType::Lead.dispatch_role(), "lead");
        assert_eq!(AgentType::Planner.dispatch_role(), "planner");
        assert_eq!(AgentType::Architect.dispatch_role(), "architect");
    }
}
