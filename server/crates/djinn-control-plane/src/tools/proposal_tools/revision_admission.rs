use crate::server::DjinnMcpServer;
use crate::tools::refinement_tools::admit_refinement_run;
use djinn_db::{ProposalRepository, RefinementAdmissionSource};

/// Admit a revision-triggered refinement only after its immutable spec revision
/// has committed. The source is the row UUID rather than its sequence so a
/// retry of the same committed write resolves to the same durable run.
pub(super) async fn admit_committed_revision_resume(
    server: &DjinnMcpServer,
    repo: &ProposalRepository,
    before: &djinn_core::models::Proposal,
    updated: &djinn_core::models::Proposal,
) -> Result<(), String> {
    if updated.latest_revision_seq == before.latest_revision_seq {
        return Ok(());
    }

    let revision_id = repo
        .revisions(&updated.id)
        .await
        .map_err(|_| "Persistence".to_owned())?
        .into_iter()
        .rev()
        .find(|revision| {
            revision.event_kind == "spec_revision"
                && revision.seq == updated.latest_revision_seq
                && revision.body == updated.body
                && revision.body_format == updated.body_format
        })
        .map(|revision| revision.id)
        .ok_or_else(|| "InvalidState".to_owned())?;

    let pending_dispatch = admit_refinement_run(
        server,
        repo,
        &updated.id,
        RefinementAdmissionSource::Revision { revision_id },
        None,
    )
    .await?;
    if pending_dispatch {
        // The intent was committed by reap_and_admit and remains retryable;
        // ProposalSingleResponse has no dispatch field to extend compatibly.
        tracing::warn!(proposal_id = %updated.id, "revision refinement admission accepted; dispatch pending");
    }
    Ok(())
}
