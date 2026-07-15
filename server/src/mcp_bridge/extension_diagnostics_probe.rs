//! Cycle-free outer-server adapter for the agent-owned diagnostics probe.

use std::path::Path;

use async_trait::async_trait;
use djinn_control_plane::bridge::ExtensionDiagnosticsProbeOps;
use djinn_core::extension_diagnostics::ExtensionLoadDiagnosticV1;

pub(super) struct ExtensionDiagnosticsProbeBridge {
    agent_context: djinn_agent::context::AgentContext,
}

impl ExtensionDiagnosticsProbeBridge {
    pub(super) fn new(agent_context: djinn_agent::context::AgentContext) -> Self {
        Self { agent_context }
    }
}

#[async_trait]
impl ExtensionDiagnosticsProbeOps for ExtensionDiagnosticsProbeBridge {
    async fn probe_project_extensions(
        &self,
        project_id: &str,
        canonical_workspace: &Path,
    ) -> Result<Vec<ExtensionLoadDiagnosticV1>, String> {
        djinn_agent::extension_diagnostics_probe::probe_project_extensions(
            project_id,
            canonical_workspace,
            &self.agent_context,
        )
        .await
    }
}
