pub(crate) mod compaction;
pub mod config;
pub(crate) mod extension;
pub mod file_time;
pub mod message;
pub mod oauth;
pub(crate) mod output_parser;
pub mod prompts;
pub mod provider;
pub(crate) mod roles;
pub mod sandbox;

// ─── Agent type ───────────────────────────────────────────────────────────────

/// Role an agent is playing within Djinn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentType {
    /// Background worker that implements a task (writes code, etc.).
    Worker,
    /// Resolves a merge conflict after reviewer-approved merge failed.
    ConflictResolver,
    /// Reviews a single task's diff and approves or rejects it.
    TaskReviewer,
    /// PM agent that grooms backlog and handles intervention for stuck tasks.
    PM,
    /// Groomer agent for backlog grooming workflows.
    Groomer,
}

impl AgentType {
    pub(crate) fn role_config(&self) -> &'static roles::RoleConfig {
        roles::config_for(*self)
    }

    pub fn as_str(&self) -> &'static str {
        self.role_config().name
    }

    /// Determine the agent type from a task's status string.
    ///
    /// When `has_conflict_context` is true and the status is `"open"`, returns
    /// `ConflictResolver` instead of `Worker`.
    pub fn for_task_status(status: &str, has_conflict_context: bool) -> Self {
        match status {
            "needs_task_review" | "in_task_review" => Self::TaskReviewer,
            "needs_pm_intervention" | "in_pm_intervention" => Self::PM,
            "open" if has_conflict_context => Self::ConflictResolver,
            _ => Self::Worker,
        }
    }

    /// The dispatch role used for model pool / settings lookup.
    ///
    /// `ConflictResolver` shares the `"worker"` model pool, so both map to
    /// `"worker"`.
    pub fn dispatch_role(&self) -> &'static str {
        self.role_config().dispatch_role
    }

    #[allow(dead_code)]
    pub(crate) fn preserves_session(&self) -> bool {
        self.role_config().preserves_session
    }

    #[allow(dead_code)]
    pub(crate) fn is_project_scoped(&self) -> bool {
        self.role_config().is_project_scoped
    }

    #[allow(dead_code)]
    pub(crate) fn initial_message(&self) -> &'static str {
        self.role_config().initial_message
    }

    #[allow(dead_code)]
    pub(crate) fn mid_session_compaction_prompt(&self) -> &'static str {
        self.role_config().compaction.mid_session
    }

    #[allow(dead_code)]
    pub(crate) fn mid_session_compaction_system_prompt(&self) -> &'static str {
        self.role_config().compaction.mid_session_system
    }

    #[allow(dead_code)]
    pub(crate) fn pre_resume_compaction_prompt(&self) -> &'static str {
        self.role_config().compaction.pre_resume
    }

    #[allow(dead_code)]
    pub(crate) fn pre_resume_compaction_system_prompt(&self) -> &'static str {
        self.role_config().compaction.pre_resume_system
    }

    #[allow(dead_code)]
    pub(crate) fn tool_schemas(&self) -> Vec<serde_json::Value> {
        (self.role_config().tool_schemas)()
    }

    /// The transition action to claim/start a task for this agent type, given
    /// the task's current status.  Returns `None` if the task is already in the
    /// active state (e.g. `in_progress` for a worker).
    pub fn start_action(&self, task_status: &str) -> Option<crate::models::TransitionAction> {
        use crate::models::TransitionAction;
        match (self, task_status) {
            (Self::Worker | Self::ConflictResolver, "open") => Some(TransitionAction::Start),
            (Self::TaskReviewer, "needs_task_review") => Some(TransitionAction::TaskReviewStart),
            (Self::PM, "needs_pm_intervention") => Some(TransitionAction::PmInterventionStart),
            (Self::Groomer, _) => None,
            _ => None,
        }
    }

    /// The transition action to release/interrupt a task held by this agent type.
    pub fn release_action(&self) -> crate::models::TransitionAction {
        use crate::models::TransitionAction;
        match self {
            Self::Worker | Self::ConflictResolver => TransitionAction::Release,
            Self::TaskReviewer => TransitionAction::ReleaseTaskReview,
            Self::PM => TransitionAction::PmInterventionRelease,
            Self::Groomer => TransitionAction::Release,
        }
    }
}

impl std::str::FromStr for AgentType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "worker" => Ok(Self::Worker),
            "conflict_resolver" => Ok(Self::ConflictResolver),
            "task_reviewer" => Ok(Self::TaskReviewer),
            "pm" => Ok(Self::PM),
            "groomer" => Ok(Self::Groomer),
            _ => Err(format!("unknown agent type: {s}")),
        }
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
mod tests {
    use super::AgentType;
    use crate::agent::roles;
    use crate::models::TransitionAction;

    fn assert_equivalent_to_role_config(agent_type: AgentType) {
        let cfg = roles::config_for(agent_type);

        assert_eq!(agent_type.as_str(), cfg.name);
        assert_eq!(agent_type.dispatch_role(), cfg.dispatch_role);
        assert_eq!(agent_type.preserves_session(), cfg.preserves_session);
        assert_eq!(agent_type.is_project_scoped(), cfg.is_project_scoped);

        assert_eq!(
            agent_type.mid_session_compaction_prompt(),
            cfg.compaction.mid_session
        );
        assert_eq!(
            agent_type.mid_session_compaction_system_prompt(),
            cfg.compaction.mid_session_system
        );
        assert_eq!(
            agent_type.pre_resume_compaction_prompt(),
            cfg.compaction.pre_resume
        );
        assert_eq!(
            agent_type.pre_resume_compaction_system_prompt(),
            cfg.compaction.pre_resume_system
        );

        assert_eq!(agent_type.tool_schemas(), (cfg.tool_schemas)());
    }

    #[test]
    fn role_config_equivalence_for_all_agent_types() {
        for agent_type in [
            AgentType::Worker,
            AgentType::ConflictResolver,
            AgentType::TaskReviewer,
            AgentType::PM,
            AgentType::Groomer,
        ] {
            assert_equivalent_to_role_config(agent_type);
        }
    }

    #[test]
    fn conflict_resolver_dispatch_role_maps_to_worker_pool() {
        assert_eq!(AgentType::ConflictResolver.dispatch_role(), "worker");
    }

    #[test]
    fn for_task_status_covers_all_expected_paths() {
        assert_eq!(AgentType::for_task_status("open", false), AgentType::Worker);
        assert_eq!(
            AgentType::for_task_status("open", true),
            AgentType::ConflictResolver
        );
        assert_eq!(
            AgentType::for_task_status("needs_task_review", false),
            AgentType::TaskReviewer
        );
        assert_eq!(
            AgentType::for_task_status("in_task_review", false),
            AgentType::TaskReviewer
        );
        assert_eq!(
            AgentType::for_task_status("needs_pm_intervention", false),
            AgentType::PM
        );
        assert_eq!(
            AgentType::for_task_status("in_pm_intervention", false),
            AgentType::PM
        );
        assert_eq!(
            AgentType::for_task_status("backlog", false),
            AgentType::Worker
        );
    }

    #[test]
    fn dispatch_role_for_all_variants() {
        assert_eq!(AgentType::Worker.dispatch_role(), "worker");
        assert_eq!(AgentType::ConflictResolver.dispatch_role(), "worker");
        assert_eq!(AgentType::TaskReviewer.dispatch_role(), "task_reviewer");
        assert_eq!(AgentType::PM.dispatch_role(), "pm");
        assert_eq!(AgentType::Groomer.dispatch_role(), "groomer");
    }

    #[test]
    fn start_action_for_each_variant_and_status() {
        assert_eq!(
            AgentType::Worker.start_action("open"),
            Some(TransitionAction::Start)
        );
        assert_eq!(
            AgentType::ConflictResolver.start_action("open"),
            Some(TransitionAction::Start)
        );
        assert_eq!(
            AgentType::TaskReviewer.start_action("needs_task_review"),
            Some(TransitionAction::TaskReviewStart)
        );
        assert_eq!(
            AgentType::PM.start_action("needs_pm_intervention"),
            Some(TransitionAction::PmInterventionStart)
        );
        assert_eq!(AgentType::Groomer.start_action("open"), None);
    }

    #[test]
    fn release_action_for_all_variants() {
        assert_eq!(
            AgentType::Worker.release_action(),
            TransitionAction::Release
        );
        assert_eq!(
            AgentType::ConflictResolver.release_action(),
            TransitionAction::Release
        );
        assert_eq!(
            AgentType::TaskReviewer.release_action(),
            TransitionAction::ReleaseTaskReview
        );
        assert_eq!(
            AgentType::PM.release_action(),
            TransitionAction::PmInterventionRelease
        );
        assert_eq!(
            AgentType::Groomer.release_action(),
            TransitionAction::Release
        );
    }
}
