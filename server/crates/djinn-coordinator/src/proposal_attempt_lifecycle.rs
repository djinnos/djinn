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
        // Emit adjacent to the real request, before its response can be
        // classified as ProviderFailure. The recorder is a production no-op.
        crate::direct_delivery::observe_boundary_operation("attempt_pr_create_or_adopt_request");
        let attempt_pr_request = self.github.create_or_adopt_attempt_draft_pr(
            &self.owner,
            &self.repo,
            CreateAttemptDraftPrParams {
                title: input.title,
                body: input.body,
                head: attempt.branch_name.clone(),
                expected_head_sha: head.clone(),
            },
        );
        let pr = match attempt_pr_request.await {
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
        // A prior stop may have closed the PR before a failure creating its
        // retirement tag. That closed, unmerged, exact draft is durable stop
        // progress and must be adopted on retry rather than closed again.
        if pr.state == djinn_provider::github_api::PrState::Open {
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
        } else if pr.state != djinn_provider::github_api::PrState::Closed
            || pr.merged == Some(true)
            || pr.draft != Some(true)
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
    use super::{
        AttemptLifecycleOutcome, ProposalAttemptLifecycle, StartAttemptInput, retirement_tag,
    };
    use crate::direct_delivery::{
        BoundaryOperation, clear_boundary_operations, take_boundary_operations,
    };
    use djinn_core::events::EventBus;
    use djinn_core::models::ProposalBuildAttemptLifecycle;
    use djinn_db::{
        Database, ProposalBuildAttemptRepository, ProposalCreateInput, ProposalRepository,
        ReserveProposalBuildAttemptInput, test_support::activate_direct_delivery_epoch_for_test,
    };
    use djinn_provider::github_api::GitHubApiClient;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path, query_param},
    };

    const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn pr(number: u64, branch: &str, state: &str) -> serde_json::Value {
        serde_json::json!({"number":number,"title":"attempt","state":state,"merged":false,
            "html_url":format!("https://example.test/pull/{number}"),"head":{"ref":branch,"sha":SHA},
            "base":{"ref":"main","sha":SHA},"auto_merge":null,"node_id":format!("PR_{number}"),"draft":true})
    }
    async fn db_and_proposal() -> (Database, ProposalRepository, djinn_core::models::Proposal) {
        let db = Database::open_in_memory().expect("ephemeral database");
        activate_direct_delivery_epoch_for_test(&db).await;
        // Keep this repository (and its cloned test-db handle) alive for the
        // test. Dropping the last template-clone owner deletes the database.
        let proposals = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = proposals
            .create(ProposalCreateInput {
                title: "attempt",
                body: "body",
                acceptance_criteria: None,
                status: None,
                body_format: None,
            })
            .await
            .expect("proposal");
        (db, proposals, proposal)
    }
    async fn mount_start(server: &MockServer, branch: &str, number: u64) {
        Mock::given(method("POST"))
            .and(path("/repos/o/r/git/refs"))
            .respond_with(ResponseTemplate::new(201))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/repos/o/r/git/ref/heads/{branch}")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"object":{"sha":SHA}})),
            )
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls"))
            .and(query_param("state", "open"))
            .and(query_param("head", format!("o:{branch}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/pulls"))
            .respond_with(ResponseTemplate::new(201).set_body_json(pr(number, branch, "open")))
            .mount(server)
            .await;
    }
    fn service(db: Database, server: &MockServer) -> ProposalAttemptLifecycle {
        ProposalAttemptLifecycle::new(
            db,
            GitHubApiClient::for_user_token_with_base_url("test".into(), server.uri()),
            "o".into(),
            "r".into(),
        )
    }
    fn input(proposal: &djinn_core::models::Proposal, id: &str, short: &str) -> StartAttemptInput {
        StartAttemptInput {
            reservation: ReserveProposalBuildAttemptInput {
                proposal_id: proposal.id.clone(),
                proposal_short_id: proposal.short_id.clone(),
                build_attempt_id: id.into(),
                build_attempt_short_id: short.into(),
                observed_base_sha: SHA.into(),
            },
            title: "attempt".into(),
            body: "body".into(),
        }
    }

    #[tokio::test]
    async fn stop_retires_history_and_regraduation_gets_distinct_branch_and_pr() {
        let (db, _proposals, proposal) = db_and_proposal().await;
        let branch = format!("proposal/{}/a1", proposal.short_id);
        let first_server = MockServer::start().await;
        mount_start(&first_server, &branch, 1).await;
        let first = match service(db.clone(), &first_server)
            .start(input(&proposal, "attempt-1", "a1"))
            .await
            .expect("start")
        {
            AttemptLifecycleOutcome::Ready(value) => value,
            other => panic!("unexpected: {other:?}"),
        };
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(pr(1, &branch, "open")))
            .mount(&first_server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues/1/comments"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&first_server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/repos/o/r/pulls/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(pr(1, &branch, "closed")))
            .expect(1)
            .mount(&first_server)
            .await;
        assert!(
            matches!(service(db.clone(), &first_server).stop(first.clone(), "regraduated").await.expect("stop"), AttemptLifecycleOutcome::Retired(ref value) if value.lifecycle == ProposalBuildAttemptLifecycle::Retired)
        );
        let retained = ProposalBuildAttemptRepository::new(db.clone())
            .get(&first.id)
            .await
            .expect("load retained attempt")
            .expect("retained attempt");
        assert_eq!(retained.lifecycle, ProposalBuildAttemptLifecycle::Retired);
        let second_branch = format!("proposal/{}/a2", proposal.short_id);
        let second_server = MockServer::start().await;
        mount_start(&second_server, &second_branch, 2).await;
        let second = match service(db, &second_server)
            .start(input(&proposal, "attempt-2", "a2"))
            .await
            .expect("regraduate")
        {
            AttemptLifecycleOutcome::Ready(value) => value,
            other => panic!("unexpected: {other:?}"),
        };
        assert_ne!(first.branch_name, second.branch_name);
        assert_ne!(first.proposal_pr_number, second.proposal_pr_number);
    }

    #[tokio::test]
    async fn stop_retry_adopts_closed_exact_pr_after_tag_failure() {
        let (db, _proposals, proposal) = db_and_proposal().await;
        let branch = format!("proposal/{}/a1", proposal.short_id);
        let start_server = MockServer::start().await;
        mount_start(&start_server, &branch, 1).await;
        let attempt = match service(db.clone(), &start_server)
            .start(input(&proposal, "attempt-retry", "a1"))
            .await
            .expect("start")
        {
            AttemptLifecycleOutcome::Ready(value) => value,
            other => panic!("unexpected: {other:?}"),
        };
        let failed = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(pr(1, &branch, "open")))
            .mount(&failed)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues/1/comments"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({})))
            .mount(&failed)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/repos/o/r/pulls/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(pr(1, &branch, "closed")))
            .mount(&failed)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/git/refs"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&failed)
            .await;
        assert!(
            service(db.clone(), &failed)
                .stop(attempt.clone(), "retry")
                .await
                .is_err()
        );
        let retry = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(pr(1, &branch, "closed")))
            .mount(&retry)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/git/refs"))
            .respond_with(ResponseTemplate::new(201))
            .expect(1)
            .mount(&retry)
            .await;
        assert!(matches!(
            service(db, &retry)
                .stop(attempt, "retry")
                .await
                .expect("retry stop"),
            AttemptLifecycleOutcome::Retired(_)
        ));
    }

    /// A failed draft-PR POST still proves the attempt-scoped request boundary
    /// was crossed because observation occurs before result classification.
    #[tokio::test]
    async fn attempt_pr_request_observation_survives_provider_failure() {
        let (db, _proposals, proposal) = db_and_proposal().await;
        let branch = format!("proposal/{}/failed", proposal.short_id);
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/git/refs"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/repos/o/r/git/ref/heads/{branch}")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"object":{"sha":SHA}})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls"))
            .and(query_param("state", "open"))
            .and(query_param("head", format!("o:{branch}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/pulls"))
            .respond_with(ResponseTemplate::new(500).set_body_string("provider unavailable"))
            .expect(1)
            .mount(&server)
            .await;

        clear_boundary_operations();
        assert!(
            service(db, &server)
                .start(input(&proposal, "attempt-failure", "failed"))
                .await
                .is_err()
        );
        assert_eq!(
            take_boundary_operations(),
            vec![BoundaryOperation::AttemptPrCreateOrAdoptRequest],
            "the request observation must survive ProviderFailure classification"
        );
    }

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
