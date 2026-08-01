//! Authenticated agent-local routing for one-shot evidence-plan capture.
//!
//! Callers can provide only check definitions. Identity and worktree
//! provenance come from trusted reply-loop context, matching evidence_exec.

use std::path::Path;

use djinn_control_plane::tools::evidence_plan::{EvidencePlanCapture, capture_evidence_plan};
use djinn_db::EvidenceRepository;

use super::evidence_exec::evidence_identity;

pub(super) async fn call_evidence_plan(
    state: &crate::context::AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    clone_root: &Path,
    session_task_id: Option<&str>,
    authenticated_session_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let capture: EvidencePlanCapture = serde_json::from_value(serde_json::Value::Object(
        arguments
            .clone()
            .ok_or("evidence_plan requires an argument object")?,
    ))
    .map_err(|error| format!("invalid evidence_plan request: {error}"))?;
    let task_id = session_task_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or("evidence_plan requires an authenticated task session")?;
    let session_id = authenticated_session_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or("evidence_plan requires an authenticated active session id")?;
    let clone_root = clone_root
        .canonicalize()
        .map_err(|error| format!("evidence_plan cannot canonicalize clone root: {error}"))?;
    let identity = evidence_identity(task_id, session_id, &clone_root).await?;
    let repository = EvidenceRepository::new(state.db.clone());
    let plan_id = capture_evidence_plan(&repository, identity, capture)
        .await
        .map_err(|error| error.to_string())?;
    Ok(serde_json::json!({ "plan_id": plan_id }))
}
