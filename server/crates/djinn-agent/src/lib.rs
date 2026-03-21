// Agent-domain code extracted from djinn-server.
// Covers: commands, agent context/roles/lifecycle, verification, actors.

pub mod commands;
pub mod events;
pub mod process;

// ─── Agent module (was src/agent/) ───────────────────────────────────────────

pub(crate) mod compaction;
pub mod config;
pub mod context;
pub(crate) mod extension;
pub mod file_time;
pub mod lsp;
pub(crate) mod task_merge;
pub use djinn_provider::message;
pub mod oauth;
pub(crate) mod output_parser;
pub(crate) mod output_stash;
pub(crate) mod patch;
pub mod prompts;
pub use djinn_provider::provider;
pub mod roles;
pub mod sandbox;
pub(crate) mod skills;
pub(crate) mod truncate;

// ─── Verification (was src/verification/) ────────────────────────────────────

pub mod verification;

// ─── Actors (was src/actors/) ────────────────────────────────────────────────

pub mod actors;

// ─── AgentType ────────────────────────────────────────────────────────────────

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
    pub(crate) fn role_config(&self) -> &'static roles::RoleConfig {
        roles::config_for(*self)
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
    pub(crate) fn tool_schemas(&self) -> Vec<serde_json::Value> {
        (self.role_config().tool_schemas)()
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

#[cfg(test)]
pub mod test_helpers;

#[cfg(test)]
mod tests {
    use super::AgentType;
    use crate::roles;
    use djinn_core::models::TransitionAction;

    fn assert_equivalent_to_role_config(agent_type: AgentType) {
        let cfg = roles::config_for(agent_type);
        assert_eq!(agent_type.as_str(), cfg.name);
        assert_eq!(agent_type.dispatch_role(), cfg.dispatch_role);
        assert_eq!(
            agent_type.role_config().preserves_session,
            cfg.preserves_session
        );
        assert_eq!(
            agent_type.role_config().is_project_scoped,
            cfg.is_project_scoped
        );
        assert_eq!(agent_type.tool_schemas(), (cfg.tool_schemas)());
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
        // Tasks with conflict context now route to Worker, not a dedicated conflict resolver
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

    #[test]
    fn start_action_via_role_config() {
        let cfg = AgentType::Worker.role_config();
        assert_eq!((cfg.start_action)("open"), Some(TransitionAction::Start));
        assert_eq!((cfg.start_action)("in_progress"), None);

        let cfg = AgentType::Reviewer.role_config();
        assert_eq!(
            (cfg.start_action)("needs_task_review"),
            Some(TransitionAction::TaskReviewStart)
        );

        let cfg = AgentType::Lead.role_config();
        assert_eq!(
            (cfg.start_action)("needs_lead_intervention"),
            Some(TransitionAction::LeadInterventionStart)
        );

        let cfg = AgentType::Planner.role_config();
        assert_eq!((cfg.start_action)("open"), Some(TransitionAction::Start));

        let cfg = AgentType::Architect.role_config();
        assert_eq!((cfg.start_action)("open"), Some(TransitionAction::Start));
    }

    #[test]
    fn release_action_via_role_config() {
        assert_eq!(
            (AgentType::Worker.role_config().release_action)(),
            TransitionAction::Release
        );
        assert_eq!(
            (AgentType::Reviewer.role_config().release_action)(),
            TransitionAction::ReleaseTaskReview
        );
        assert_eq!(
            (AgentType::Lead.role_config().release_action)(),
            TransitionAction::LeadInterventionRelease
        );
        assert_eq!(
            (AgentType::Planner.role_config().release_action)(),
            TransitionAction::Release
        );
        assert_eq!(
            (AgentType::Architect.role_config().release_action)(),
            TransitionAction::Release
        );
    }
}
