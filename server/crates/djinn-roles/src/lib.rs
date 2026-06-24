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
    ADVERSARY_TEMPLATE, ADVOCATE_TEMPLATE, ARCHITECT_TEMPLATE, BASE_TEMPLATE, CLUSTER_DOC_TEMPLATE,
    DEV_TEMPLATE, JUDGE_TEMPLATE, LEAD_TEMPLATE, PLANNER_TEMPLATE, REVIEWER_TEMPLATE,
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
    /// Advocate: proposal author/reviser in the tribunal refinement workflow (k9zw).
    Advocate,
    /// Adversary: red-team objection producer in the tribunal refinement workflow (k9zw).
    Adversary,
    /// Judge: independent adjudicator in the tribunal refinement workflow (k9zw).
    Judge,
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

    /// Parse from a DB/wire string, including the `architect` variant
    /// and tribunal refinement roles (k9zw).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "worker" => Some(Self::Worker),
            "reviewer" => Some(Self::Reviewer),
            "lead" => Some(Self::Lead),
            "planner" => Some(Self::Planner),
            "architect" => Some(Self::Architect),
            "advocate" => Some(Self::Advocate),
            "adversary" => Some(Self::Adversary),
            "judge" => Some(Self::Judge),
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
            AgentType::Advocate,
            AgentType::Adversary,
            AgentType::Judge,
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
        assert_eq!(AgentType::Advocate.dispatch_role(), "advocate");
        assert_eq!(AgentType::Adversary.dispatch_role(), "adversary");
        assert_eq!(AgentType::Judge.dispatch_role(), "judge");
    }

    #[test]
    fn tribunal_roles_parse_from_string() {
        assert_eq!(AgentType::parse("advocate"), Some(AgentType::Advocate));
        assert_eq!(AgentType::parse("adversary"), Some(AgentType::Adversary));
        assert_eq!(AgentType::parse("judge"), Some(AgentType::Judge));
    }
}

// ─── Boundary regression checks ─────────────────────────────────────────────
//
// These tests ensure the extraction boundaries introduced in Phase 3 stay
// clean.  They parse `Cargo.toml` at test time so any forbidden dependency
// added to this crate causes an immediate test failure.
//
// Cross-reference: boundary_rules.toml rules `no-roles-imports-agent` and
// `no-roles-imports-extension`.

#[cfg(test)]
mod boundary_regression {
    /// Parse the `[dependencies]` table from the crate's own `Cargo.toml` and
    /// return the set of dependency names.
    fn crate_dependency_names() -> std::collections::BTreeSet<String> {
        let cargo_toml_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let contents = std::fs::read_to_string(&cargo_toml_path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", cargo_toml_path.display()));
        let doc: toml::Value =
            toml::from_str(&contents).unwrap_or_else(|e| panic!("failed to parse Cargo.toml: {e}"));
        let mut names = std::collections::BTreeSet::new();
        // Walk the standard dependency tables.
        for table_key in &["dependencies", "dev-dependencies", "build-dependencies"] {
            if let Some(table) = doc.get(table_key).and_then(|v| v.as_table()) {
                names.extend(table.keys().cloned());
            }
        }
        names
    }

    /// Assert that `djinn-roles` does NOT declare a dependency on the given
    /// crate name.  The test reads the real `Cargo.toml` so it reflects any
    /// future edits.
    fn assert_no_dependency(dep_name: &str) {
        let deps = crate_dependency_names();
        assert!(
            !deps.contains(dep_name),
            "djinn-roles must not depend on `{dep_name}` — add it to \
             boundary_rules.toml and fix the extraction boundary."
        );
    }

    #[test]
    fn no_djinn_agent_dependency() {
        assert_no_dependency("djinn-agent");
    }

    #[test]
    fn no_djinn_mcp_extension_dependency() {
        // Phase 4 owns extension extraction; djinn-roles must remain
        // extension-agnostic.
        assert_no_dependency("djinn-mcp-extension");
    }

    #[test]
    fn no_sqlx_dependency() {
        // djinn-roles owns role config and prompt rendering, not database
        // access.  sqlx must not appear as a direct dependency.
        assert_no_dependency("sqlx");
    }

    #[test]
    fn allowed_djinn_dependency_is_djinn_core_only() {
        let deps = crate_dependency_names();
        let djinn_deps: Vec<&str> = deps
            .iter()
            .filter(|name| name.starts_with("djinn-"))
            .map(|s| s.as_str())
            .collect();
        assert_eq!(
            djinn_deps,
            vec!["djinn-core"],
            "djinn-roles should depend only on djinn-core among djinn-* crates; \
             found: {djinn_deps:?}.  If adding a new djinn-* dep is intentional, \
             update this assertion and add a boundary_rules.toml rule."
        );
    }
}
