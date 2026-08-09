//! Dark attempt branch and draft-PR lifecycle; no live graduation path calls it.

use anyhow::{Result, anyhow};
use djinn_core::models::{
    DirectDeliveryParkReason, ProposalBuildAttempt, ProposalBuildAttemptLifecycle,
};
use djinn_db::{
    ActivateProposalBuildAttemptInput, DirectDeliveryCapabilityRepository,
    PersistAttemptPrIdentityInput, ProposalBuildAttemptRepository, ReconcileAttemptBranchHeadInput,
    ReserveProposalBuildAttemptInput, ReserveProposalBuildAttemptResult,
    RetireProposalBuildAttemptInput,
};
use djinn_provider::github_api::{
    AttemptDraftPrResult, CloseAttemptDraftPrResult, CreateAttemptDraftPrParams,
    ExactRefObservation, ExpectedAbsentRefResult, GitHubApiClient,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttemptLifecycleOutcome {
    Disabled,
    Ready(ProposalBuildAttempt),
    Retired(ProposalBuildAttempt),
    Parked {
        attempt: ProposalBuildAttempt,
        reason: DirectDeliveryParkReason,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartAttemptInput {
    pub reservation: ReserveProposalBuildAttemptInput,
    pub title: String,
    pub body: String,
}

/// Uses only attempt-scoped provider operations, never task-PR APIs.
pub struct ProposalAttemptLifecycle {
    db: djinn_db::Database,
    github: GitHubApiClient,
    owner: String,
    repo: String,
}
impl ProposalAttemptLifecycle {
    pub fn new(
        db: djinn_db::Database,
        github: GitHubApiClient,
        owner: String,
        repo: String,
    ) -> Self {
        Self {
            db,
            github,
            owner,
            repo,
        }
    }
    pub async fn start(&self, input: StartAttemptInput) -> Result<AttemptLifecycleOutcome> {
        if !self.enabled().await? {
            return Ok(AttemptLifecycleOutcome::Disabled);
        }
        let repo = ProposalBuildAttemptRepository::new(self.db.clone());
        let mut attempt = match repo.reserve(&input.reservation).await? {
            ReserveProposalBuildAttemptResult::Reserved(a)
            | ReserveProposalBuildAttemptResult::Replayed(a) => a,
            ReserveProposalBuildAttemptResult::CompetingIdentity { existing } => {
                return self
                    .park(
                        &repo,
                        existing,
                        DirectDeliveryParkReason::BranchIdentityMismatch,
                    )
                    .await;
            }
        };
        match self
            .github
            .create_ref_expected_absent(
                &self.owner,
                &self.repo,
                &format!("refs/heads/{}", attempt.branch_name),
                &attempt.base_sha,
            )
            .await
        {
            ExpectedAbsentRefResult::Created | ExpectedAbsentRefResult::AdoptedExact { .. } => {}
            ExpectedAbsentRefResult::BranchIdentityMismatch { .. } => {
                return self
                    .park(
                        &repo,
                        attempt,
                        DirectDeliveryParkReason::BranchIdentityMismatch,
                    )
                    .await;
            }
            ExpectedAbsentRefResult::ProviderFailure(e) => return Err(anyhow!(e)),
        }
        let observed = match self
            .github
            .observe_exact_ref(
                &self.owner,
                &self.repo,
                &format!("heads/{}", attempt.branch_name),
            )
            .await
        {
            ExactRefObservation::Found { sha } => sha,
            ExactRefObservation::NotFound => {
                return Err(anyhow!("attempt branch disappeared after create"));
            }
            ExactRefObservation::ProviderFailure(e) => return Err(anyhow!(e)),
        };
        attempt = match repo
            .reconcile_branch_head(&ReconcileAttemptBranchHeadInput {
                build_attempt_id: attempt.id.clone(),
                expected_branch_head_sha: attempt.branch_head_sha.clone(),
                observed_branch_head_sha: observed,
            })
            .await?
        {
            djinn_db::ReconcileAttemptBranchHeadResult::Reconciled(a)
            | djinn_db::ReconcileAttemptBranchHeadResult::Replayed(a) => a,
            djinn_db::ReconcileAttemptBranchHeadResult::Parked { attempt, reason } => {
                return Ok(AttemptLifecycleOutcome::Parked { attempt, reason });
            }
            djinn_db::ReconcileAttemptBranchHeadResult::Stale { current } => {
                return Ok(AttemptLifecycleOutcome::Parked {
                    attempt: current,
                    reason: DirectDeliveryParkReason::BranchIdentityMismatch,
                });
            }
        };
        let head = attempt
            .branch_head_sha
            .clone()
            .ok_or_else(|| anyhow!("missing reconciled attempt head"))?;
        let pr = match self
            .github
            .create_or_adopt_attempt_draft_pr(
                &self.owner,
                &self.repo,
                CreateAttemptDraftPrParams {
                    title: input.title,
                    body: input.body,
                    head: attempt.branch_name.clone(),
                    expected_head_sha: head.clone(),
                },
            )
            .await
        {
            AttemptDraftPrResult::Created(pr) | AttemptDraftPrResult::AdoptedExact(pr) => pr,
            AttemptDraftPrResult::ProposalPrIdentityMismatch { .. } => {
                return self
                    .park(
                        &repo,
                        attempt,
                        DirectDeliveryParkReason::ProposalPrIdentityMismatch,
                    )
                    .await;
            }
            AttemptDraftPrResult::ProviderFailure(e) => return Err(anyhow!(e)),
        };
        attempt = match repo
            .persist_pr_identity(&PersistAttemptPrIdentityInput {
                build_attempt_id: attempt.id.clone(),
                proposal_pr_number: i64::try_from(pr.number)
                    .map_err(|_| anyhow!("PR number exceeds i64"))?,
                proposal_pr_url: pr.html_url,
            })
            .await?
        {
            djinn_db::PersistAttemptPrIdentityResult::Persisted(a)
            | djinn_db::PersistAttemptPrIdentityResult::Replayed(a) => a,
            djinn_db::PersistAttemptPrIdentityResult::Parked { attempt, reason } => {
                return Ok(AttemptLifecycleOutcome::Parked { attempt, reason });
            }
        };
        match repo
            .activate(&ActivateProposalBuildAttemptInput {
                build_attempt_id: attempt.id.clone(),
                expected_lifecycle: ProposalBuildAttemptLifecycle::Reserved,
                expected_branch_head_sha: Some(head.clone()),
                branch_head_sha: head,
            })
            .await?
        {
            djinn_db::ActivateProposalBuildAttemptResult::Activated(a)
            | djinn_db::ActivateProposalBuildAttemptResult::Replayed(a) => {
                Ok(AttemptLifecycleOutcome::Ready(a))
            }
            djinn_db::ActivateProposalBuildAttemptResult::Stale { current } => {
                Ok(AttemptLifecycleOutcome::Parked {
                    attempt: current,
                    reason: DirectDeliveryParkReason::ProposalPrIdentityMismatch,
                })
            }
        }
    }
    pub async fn stop(
        &self,
        attempt: ProposalBuildAttempt,
        reason: &str,
    ) -> Result<AttemptLifecycleOutcome> {
        if !self.enabled().await? {
            return Ok(AttemptLifecycleOutcome::Disabled);
        }
        if attempt.lifecycle == ProposalBuildAttemptLifecycle::Retired {
            return Ok(AttemptLifecycleOutcome::Retired(attempt));
        }
        let repo = ProposalBuildAttemptRepository::new(self.db.clone());
        let head = attempt
            .branch_head_sha
            .clone()
            .ok_or_else(|| anyhow!("cannot retire a headless attempt"))?;
        let number = u64::try_from(
            attempt
                .proposal_pr_number
                .ok_or_else(|| anyhow!("cannot retire an attempt without a PR"))?,
        )
        .map_err(|_| anyhow!("negative PR number"))?;
        let (pr, _) = self
            .github
            .get_pull_request(&self.owner, &self.repo, number)
            .await?;
        if pr.head.ref_name != attempt.branch_name
            || pr.head.sha != head
            || pr.base.ref_name != "main"
        {
            return self
                .park(
                    &repo,
                    attempt,
                    DirectDeliveryParkReason::ProposalPrIdentityMismatch,
                )
                .await;
        }
        match self
            .github
            .close_attempt_draft_pr(&self.owner, &self.repo, &pr, reason)
            .await
        {
            CloseAttemptDraftPrResult::Closed(_) => {}
            CloseAttemptDraftPrResult::ProposalPrIdentityMismatch => {
                return self
                    .park(
                        &repo,
                        attempt,
                        DirectDeliveryParkReason::ProposalPrIdentityMismatch,
                    )
                    .await;
            }
            CloseAttemptDraftPrResult::ProviderFailure(e) => return Err(anyhow!(e)),
        }
        match self
            .github
            .create_ref_expected_absent(
                &self.owner,
                &self.repo,
                &retirement_tag(&attempt.branch_name),
                &head,
            )
            .await
        {
            ExpectedAbsentRefResult::Created | ExpectedAbsentRefResult::AdoptedExact { .. } => {}
            ExpectedAbsentRefResult::BranchIdentityMismatch { .. } => {
                return self
                    .park(
                        &repo,
                        attempt,
                        DirectDeliveryParkReason::BranchIdentityMismatch,
                    )
                    .await;
            }
            ExpectedAbsentRefResult::ProviderFailure(e) => return Err(anyhow!(e)),
        }
        let retired = match repo
            .retire(&RetireProposalBuildAttemptInput {
                build_attempt_id: attempt.id,
            })
            .await?
        {
            djinn_db::RetireProposalBuildAttemptResult::Retired(a)
            | djinn_db::RetireProposalBuildAttemptResult::Replayed(a) => a,
        };
        Ok(AttemptLifecycleOutcome::Retired(retired))
    }
    async fn enabled(&self) -> Result<bool> {
        Ok(DirectDeliveryCapabilityRepository::new(self.db.clone())
            .probe()
            .await?
            .permits_direct_delivery())
    }
    async fn park(
        &self,
        repo: &ProposalBuildAttemptRepository,
        attempt: ProposalBuildAttempt,
        reason: DirectDeliveryParkReason,
    ) -> Result<AttemptLifecycleOutcome> {
        Ok(AttemptLifecycleOutcome::Parked {
            attempt: repo.park(&attempt.id, reason).await?,
            reason,
        })
    }
}
/// An immutable retained tag, never branch deletion or force push.
pub fn retirement_tag(branch: &str) -> String {
    format!("refs/tags/{branch}/retired")
}
#[cfg(test)]
mod tests {
    use super::retirement_tag;
    #[test]
    fn tags_are_attempt_distinct() {
        assert_eq!(
            retirement_tag("proposal/dser/a1"),
            "refs/tags/proposal/dser/a1/retired"
        );
        assert_ne!(
            retirement_tag("proposal/dser/a1"),
            retirement_tag("proposal/dser/a2")
        );
    }
}
