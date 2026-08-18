//! The production call sites for [`ProposalAttemptLifecycle`].
//!
//! `proposal_graduate` calls [`DjinnMcpServer::start_proposal_build_attempt`]
//! before it creates the breakdown task, and `proposal_stop_build`'s abort
//! cascade calls [`DjinnMcpServer::stop_proposal_build_attempt`] before it
//! disposes the build. Deleting either call is the mutation the regressions in
//! `attempt_wiring_tests.rs` are written to catch.
//!
//! Both helpers begin with the `direct_delivery_v1` epoch probe and return
//! without resolving a project, minting an installation token or issuing a
//! single forge request while that epoch is `disabled` — the shipped default.

use djinn_core::models::{Proposal, ProposalBuildAttempt};
use djinn_db::{
    DirectDeliveryCapabilityRepository, ProjectRepository, ProposalBuildAttemptRepository,
    ProposalRepository,
};
use djinn_provider::github_api::GitHubApiClient;

use crate::proposal_attempt_lifecycle::{
    AttemptLifecycleOutcome, ProposalAttemptLifecycle, StartAttemptInput,
};
use crate::server::DjinnMcpServer;

impl DjinnMcpServer {
    /// Reserve, publish and activate this graduation's attempt branch and its
    /// single draft attempt PR.
    ///
    /// Returns `Ok(None)` when the epoch is disabled, which is the only
    /// outcome on a default deployment. An already-active attempt is adopted
    /// rather than duplicated, so a repeat graduation cannot mint a second
    /// branch identity for the same proposal.
    pub(super) async fn start_proposal_build_attempt(
        &self,
        proposal: &Proposal,
    ) -> Result<Option<ProposalBuildAttempt>, String> {
        let Some(lifecycle) = self.proposal_attempt_lifecycle(&proposal.id).await? else {
            return Ok(None);
        };
        let attempts = ProposalBuildAttemptRepository::new(self.state.db().clone());
        if let Some(active) = attempts
            .active_attempt(&proposal.id)
            .await
            .map_err(|e| format!("failed to read the active proposal build attempt: {e}"))?
        {
            return Ok(Some(active));
        }
        let build_attempt_id = uuid::Uuid::now_v7();
        let outcome = lifecycle
            .start(StartAttemptInput {
                proposal_id: proposal.id.clone(),
                proposal_short_id: proposal.short_id.clone(),
                build_attempt_id: build_attempt_id.to_string(),
                build_attempt_short_id: attempt_short_id(&build_attempt_id),
                title: format!("Proposal {}: {}", proposal.short_id, proposal.title),
                body: format!(
                    "Draft attempt PR for proposal `{}` ({}). Build tasks append \
                     commits to this attempt branch; it is never force-pushed.",
                    proposal.short_id, proposal.id
                ),
            })
            .await
            .map_err(|e| format!("failed to start the proposal build attempt: {e}"))?;
        match outcome {
            AttemptLifecycleOutcome::Disabled => Ok(None),
            AttemptLifecycleOutcome::Ready(attempt) => Ok(Some(attempt)),
            AttemptLifecycleOutcome::Parked { attempt, reason } => Err(format!(
                "proposal build attempt {} parked as {reason}; resolve the attempt branch \
                 or PR identity before kicking off",
                attempt.short_id
            )),
            AttemptLifecycleOutcome::Retired(attempt) => Err(format!(
                "proposal build attempt {} is retired and cannot be started",
                attempt.short_id
            )),
        }
    }

    /// Close the unmerged draft attempt PR with `build_attempt_stopped`, retire
    /// the attempt branch as an immutable tag, and retire the attempt row.
    ///
    /// A no-op when the epoch is disabled or the proposal owns no active
    /// attempt. A parked stop is an error: the abort cascade must not dispose
    /// the build while its attempt branch is still live.
    pub(super) async fn stop_proposal_build_attempt(
        &self,
        proposal: &Proposal,
        reason: &str,
    ) -> Result<(), String> {
        let Some(lifecycle) = self.proposal_attempt_lifecycle(&proposal.id).await? else {
            return Ok(());
        };
        let attempts = ProposalBuildAttemptRepository::new(self.state.db().clone());
        let Some(active) = attempts
            .active_attempt(&proposal.id)
            .await
            .map_err(|e| format!("failed to read the active proposal build attempt: {e}"))?
        else {
            return Ok(());
        };
        let outcome = lifecycle
            // `close_attempt_draft_pr` records this as
            // `build_attempt_stopped: <reason>` on the PR before closing it.
            .stop(active, reason)
            .await
            .map_err(|e| format!("failed to stop the proposal build attempt: {e}"))?;
        match outcome {
            AttemptLifecycleOutcome::Retired(_) | AttemptLifecycleOutcome::Disabled => Ok(()),
            AttemptLifecycleOutcome::Parked { attempt, reason } => Err(format!(
                "proposal build attempt {} parked as {reason}; its branch and PR were not \
                 retired, so the build was left in place",
                attempt.short_id
            )),
            AttemptLifecycleOutcome::Ready(attempt) => Err(format!(
                "proposal build attempt {} was not retired",
                attempt.short_id
            )),
        }
    }

    /// Build the lifecycle for a proposal's primary target repository, or
    /// `None` when the `direct_delivery_v1` epoch does not permit it.
    async fn proposal_attempt_lifecycle(
        &self,
        proposal_id: &str,
    ) -> Result<Option<ProposalAttemptLifecycle>, String> {
        // The epoch probe comes first and on its own: while the epoch is
        // disabled nothing below runs, so graduation and abort keep exactly
        // the shape they had before this path existed.
        if !DirectDeliveryCapabilityRepository::new(self.state.db().clone())
            .probe()
            .await
            .map_err(|e| format!("failed to probe the direct-delivery epoch: {e}"))?
            .permits_direct_delivery()
        {
            return Ok(None);
        }
        let proposals = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let targets = proposals
            .targets(proposal_id)
            .await
            .map_err(|e| format!("failed to read proposal targets: {e}"))?;
        let Some(primary) = targets.into_iter().find(|t| t.role == "primary") else {
            return Err(
                "the direct-delivery epoch is active but this proposal has no primary target"
                    .to_string(),
            );
        };
        let project_repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        let project = project_repo
            .get(&primary.project_id)
            .await
            .map_err(|e| format!("failed to read the primary target project: {e}"))?
            .ok_or_else(|| format!("primary target project {} not found", primary.project_id))?;
        let installation_id = project_repo
            .get_installation_id(&primary.project_id)
            .await
            .map_err(|e| format!("failed to read the project GitHub installation: {e}"))?
            .ok_or_else(|| {
                format!(
                    "project {} has no GitHub App installation, so no attempt branch can be created",
                    primary.project_id
                )
            })?;
        Ok(Some(ProposalAttemptLifecycle::new(
            self.state.db().clone(),
            installation_client(installation_id),
            project.github_owner,
            project.github_repo,
            process_incarnation_id().to_owned(),
        )))
    }
}

/// A 8-hex suffix of the attempt UUID. Attempt short IDs only have to be
/// distinct within one proposal — `proposal_build_attempts_proposal_short_id_unique`
/// enforces that — and the branch name derives from this value.
fn attempt_short_id(build_attempt_id: &uuid::Uuid) -> String {
    let simple = build_attempt_id.simple().to_string();
    simple[simple.len() - 8..].to_owned()
}

/// One stable identity per server process, presented to the attempt fencing
/// lease so a competing driver is always a different owner.
fn process_incarnation_id() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| uuid::Uuid::now_v7().to_string())
}

/// Route attempt-lifecycle requests at a deterministic server in tests. The
/// real project lookup, installation-token cache, client authentication and
/// provider request path are all retained.
#[cfg(test)]
static ATTEMPT_CLIENT_BASE_URL: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

#[cfg(test)]
pub(super) fn set_attempt_client_base_url_for_test(base_url: Option<String>) {
    *ATTEMPT_CLIENT_BASE_URL.lock().unwrap() = base_url;
}

fn installation_client(installation_id: u64) -> GitHubApiClient {
    #[cfg(test)]
    if let Some(base_url) = ATTEMPT_CLIENT_BASE_URL.lock().unwrap().clone() {
        return GitHubApiClient::for_installation_with_base_url(installation_id, base_url);
    }

    GitHubApiClient::for_installation(installation_id)
}
