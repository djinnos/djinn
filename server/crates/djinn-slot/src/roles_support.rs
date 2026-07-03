//! Agent role trait and finalize types.
//!
//! Extracted from `djinn-agent::roles` so slot code can reference these
//! types without depending on `djinn-agent`.  The concrete role
//! implementations (WorkerRole, ReviewerRole, etc.) remain in `djinn-agent`.

use djinn_core::models::Task;
use djinn_roles::config::RoleConfig;
use futures::future::BoxFuture;

use crate::host::SlotContext;

/// Thin role trait that every agent role must implement.
///
/// Object-safe: async methods return `BoxFuture` so `dyn AgentRole` works.
pub trait AgentRole: Send + Sync + 'static {
    fn config(&self) -> &RoleConfig;
    fn render_prompt(&self, task: &Task, context_json: &serde_json::Value) -> String;
    /// The primary MCP tool name this role uses to signal session completion.
    fn finalize_tool_name(&self) -> &'static str {
        self.config()
            .finalize_tool_names
            .first()
            .copied()
            .unwrap_or("")
    }
    /// Whether this role should build epic context for the prompt.
    fn needs_epic_context(&self) -> bool {
        true
    }
    /// Build the initial user message for a fresh session.
    fn initial_user_message<'a>(
        &'a self,
        _task_id: &'a str,
        _ctx: &'a SlotContext,
    ) -> BoxFuture<'a, String> {
        Box::pin(async {
            "Start by understanding the task context and execute it fully before stopping."
                .to_string()
        })
    }
}

/// Resolve the concrete `AgentRole` implementation for an `AgentType`.
///
/// This is a placeholder that returns `None` — the real resolution lives in
/// `djinn-agent` via the host callbacks. Slot code should use
/// `ctx.callbacks.resolve_role(agent_type)` instead.
pub fn role_config_for(agent_type: djinn_roles::AgentType) -> &'static RoleConfig {
    djinn_roles::config::config_for(agent_type)
}

/// Decision types extracted from finalize tool calls.
/// These mirror the djinn-agent::roles::finalize types.
pub mod finalize {
    use serde::{Deserialize, Serialize};
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct SubmitWork {
        pub summary: String,
        #[serde(default)]
        pub files_changed: Vec<String>,
        #[serde(default)]
        pub remaining_concerns: Vec<String>,
        #[serde(default)]
        pub commit_title: Option<String>,
    }
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct SubmitReview {
        pub verdict: AcVerdict,
        #[serde(default)]
        pub summary: Option<String>,
    }
    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum AcVerdict {
        Met,
        NotMet,
    }
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct SubmitDecision {
        pub decision: String,
        #[serde(default)]
        pub rationale: Option<String>,
    }
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct SubmitGrooming {
        pub task_id: String,
        pub grooming_notes: String,
    }
}
