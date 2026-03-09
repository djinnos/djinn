// AgentConfig construction helpers.
//
// Djinn-specific configuration bundles used when spawning Goose agents.
// Full provider wiring and AgentConfig assembly is implemented in d9s4
// (AgentSupervisor with session tracking and capacity).

use super::AgentType;

/// Lightweight config bundle passed to AgentSupervisor::dispatch().
///
/// Holds the provider/model selection and the agent role. The supervisor
/// uses this to build a full `goose::agents::AgentConfig` in d9s4.
pub struct DispatchConfig {
    pub agent_type: AgentType,
    pub provider_name: String,
    pub model_name: String,
}

impl DispatchConfig {
    pub fn worker(provider_name: impl Into<String>, model_name: impl Into<String>) -> Self {
        Self {
            agent_type: AgentType::Worker,
            provider_name: provider_name.into(),
            model_name: model_name.into(),
        }
    }

    pub fn task_reviewer(provider_name: impl Into<String>, model_name: impl Into<String>) -> Self {
        Self {
            agent_type: AgentType::TaskReviewer,
            provider_name: provider_name.into(),
            model_name: model_name.into(),
        }
    }
}
