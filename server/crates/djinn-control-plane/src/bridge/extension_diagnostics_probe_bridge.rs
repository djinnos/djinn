//! Bridge between `djinn-control-plane` and the agent-owned fresh extension
//! diagnostics probe.
//!
//! `djinn-agent` already depends on `djinn-control-plane`, so the control plane
//! owns only this narrow async contract. The outer server crate implements it
//! by delegating to the agent, preserving the one-way Cargo dependency graph.

use std::path::Path;

use async_trait::async_trait;
use djinn_core::extension_diagnostics::ExtensionLoadDiagnosticV1;

/// Runs one fresh, project-scoped extension-load diagnostics probe.
///
/// The caller resolves and authorizes `project_id`, then derives
/// `canonical_workspace` from the project record. Implementations must return
/// only canonical V1 records created by this fresh probe. Task/session IDs,
/// prior attempts, logs, and raw extension facts are deliberately not part of
/// this consumer-facing contract.
#[async_trait]
pub trait ExtensionDiagnosticsProbeOps: Send + Sync {
    /// Probe the project's extensions from its already-derived canonical
    /// workspace path and return the persisted canonical diagnostics.
    async fn probe_project_extensions(
        &self,
        project_id: &str,
        canonical_workspace: &Path,
    ) -> Result<Vec<ExtensionLoadDiagnosticV1>, String>;
}
