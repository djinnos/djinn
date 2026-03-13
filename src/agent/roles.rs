use std::sync::LazyLock;

use super::{AgentType, compaction, extension};

mod conflict;
mod groomer;
mod pm;
mod reviewer;
mod worker;

pub(crate) struct RoleConfig {
    pub(crate) name: &'static str,
    pub(crate) dispatch_role: &'static str,
    pub(crate) preserves_session: bool,
    pub(crate) is_project_scoped: bool,
    pub(crate) mid_session_compaction_prompt: &'static str,
    pub(crate) pre_resume_compaction_prompt: &'static str,
    pub(crate) tool_schemas: fn() -> Vec<serde_json::Value>,
}

pub(crate) static WORKER_CONFIG: LazyLock<RoleConfig> = LazyLock::new(worker::build);
pub(crate) static TASK_REVIEWER_CONFIG: LazyLock<RoleConfig> = LazyLock::new(reviewer::build);
pub(crate) static PM_CONFIG: LazyLock<RoleConfig> = LazyLock::new(pm::build);
pub(crate) static GROOMER_CONFIG: LazyLock<RoleConfig> = LazyLock::new(groomer::build);
pub(crate) static CONFLICT_RESOLVER_CONFIG: LazyLock<RoleConfig> = LazyLock::new(conflict::build);

pub(crate) fn config_for(agent_type: AgentType) -> &'static RoleConfig {
    match agent_type {
        AgentType::Worker => &WORKER_CONFIG,
        AgentType::ConflictResolver => &CONFLICT_RESOLVER_CONFIG,
        AgentType::TaskReviewer => &TASK_REVIEWER_CONFIG,
        AgentType::PM => &PM_CONFIG,
        AgentType::Groomer => &GROOMER_CONFIG,
    }
}

fn worker_tools() -> Vec<serde_json::Value> {
    extension::tool_schemas(AgentType::Worker)
}

fn reviewer_tools() -> Vec<serde_json::Value> {
    extension::tool_schemas(AgentType::TaskReviewer)
}

fn pm_tools() -> Vec<serde_json::Value> {
    extension::tool_schemas(AgentType::PM)
}

fn groomer_tools() -> Vec<serde_json::Value> {
    extension::tool_schemas(AgentType::Groomer)
}

fn conflict_tools() -> Vec<serde_json::Value> {
    extension::tool_schemas(AgentType::ConflictResolver)
}

pub(crate) fn generic_mid_prompt() -> &'static str {
    compaction::prompt_mid_generic()
}

pub(crate) fn generic_pre_prompt() -> &'static str {
    compaction::prompt_pre_generic()
}

pub(crate) fn worker_mid_prompt() -> &'static str {
    compaction::prompt_mid_worker()
}

pub(crate) fn worker_pre_prompt() -> &'static str {
    compaction::prompt_pre_worker()
}

pub(crate) fn reviewer_prompt() -> &'static str {
    compaction::prompt_reviewer()
}

pub(crate) fn conflict_prompt() -> &'static str {
    compaction::prompt_conflict_resolver()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_equivalent(agent: AgentType) {
        let config = config_for(agent);
        assert_eq!(config.name, agent.as_str());
        assert_eq!(config.dispatch_role, agent.dispatch_role());
        assert_eq!(config.preserves_session, agent.preserves_session());
        assert_eq!(config.is_project_scoped, agent.is_project_scoped());
        assert_eq!(
            config.mid_session_compaction_prompt,
            agent.mid_session_compaction_prompt()
        );
        assert_eq!(
            config.pre_resume_compaction_prompt,
            agent.pre_resume_compaction_prompt()
        );
        assert_eq!((config.tool_schemas)(), agent.tool_schemas());
    }

    #[test]
    fn worker_config_equivalence() {
        assert_equivalent(AgentType::Worker);
    }

    #[test]
    fn conflict_resolver_config_equivalence() {
        assert_equivalent(AgentType::ConflictResolver);
    }

    #[test]
    fn reviewer_config_equivalence() {
        assert_equivalent(AgentType::TaskReviewer);
    }

    #[test]
    fn pm_config_equivalence() {
        assert_equivalent(AgentType::PM);
    }

    #[test]
    fn groomer_config_equivalence() {
        assert_equivalent(AgentType::Groomer);
    }
}
