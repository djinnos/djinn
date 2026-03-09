// AgentConfig construction helpers.
//
// Djinn-specific configuration bundles used when spawning agent sessions.

use super::AgentType;

/// Lightweight config bundle passed to the slot pool for dispatching.
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
