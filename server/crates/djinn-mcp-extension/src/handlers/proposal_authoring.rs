//! Shared repository-result mapping for proposal authoring handlers.

use djinn_db::ProposalRepository;

/// Return lint data from the immutable revision that was actually committed.
/// The repository validates its cache against that exact stored snapshot.
pub(super) async fn committed_latest_lint(
    proposal_repo: &ProposalRepository,
    proposal: &djinn_core::models::proposal::Proposal,
) -> Result<serde_json::Value, String> {
    let revision = proposal_repo
        .revisions(&proposal.id)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .rev()
        .find(|revision| {
            revision.seq == proposal.latest_revision_seq
                && revision.body == proposal.body
                && revision.body_format == proposal.body_format
        })
        .ok_or_else(|| {
            format!(
                "committed revision not found for proposal {}/{}",
                proposal.id, proposal.latest_revision_seq
            )
        })?;
    serde_json::to_value(
        proposal_repo
            .lint_for_revision(&revision)
            .await
            .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

/// Preserve structured repository lint rejections for correction loops.
/// `SpecLintRejected` contains only error violations, and its established
/// source-span ordering is deliberately retained in the JSON response.
pub(super) fn proposal_authoring_error(
    error: djinn_db::Error,
) -> Result<serde_json::Value, String> {
    match error {
        djinn_db::Error::SpecLintRejected(rejection) => {
            let readable_error = rejection.code.clone();
            Ok(serde_json::json!({
                "ok": false,
                "error": readable_error,
                "code": rejection.code,
                "violations": rejection.violations.into_iter().map(|violation| serde_json::json!({
                    "code": violation.code,
                    "message": violation.message,
                    "severity": "error",
                    "span": { "start": violation.span_start, "end": violation.span_end },
                })).collect::<Vec<_>>(),
            }))
        }
        other => Err(other.to_string()),
    }
}
