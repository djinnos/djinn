//! Attempt branch and draft-PR lifecycle for a proposal build attempt.
//!
//! `proposal_graduate` drives [`ProposalAttemptLifecycle::start`] and
//! `proposal_stop_build`'s abort cascade drives [`ProposalAttemptLifecycle::stop`];
//! both call sites live in `tools::proposal_tools::lifecycle::attempt_wiring`.
//! Each entry point is a no-op while the `direct_delivery_v1` epoch is disabled
//! — the shipped default — and neither reaches the forge in that state.

use anyhow::{Result, anyhow};
use djinn_core::models::{
    DirectDeliveryParkReason, ProposalBuildAttempt, ProposalBuildAttemptLifecycle,
};
use djinn_db::{
    AcquireProposalBuildAttemptLeaseInput, AcquireProposalBuildAttemptLeaseResult,
    ActivateProposalBuildAttemptInput, DirectDeliveryCapabilityRepository,
    PersistAttemptPrIdentityInput, ProposalBuildAttemptRepository, ReconcileAttemptBranchHeadInput,
    ReserveProposalBuildAttemptInput, ReserveProposalBuildAttemptResult,
    RetireProposalBuildAttemptInput,
};
use djinn_provider::github_api::{
    AttemptDraftPrResult, CloseAttemptDraftPrResult, CreateAttemptDraftPrParams,
    ExactRefObservation, ExpectedAbsentRefResult, GitHubApiClient,
};

/// The attempt branch forks from this branch and the attempt draft PR targets
/// it. `create_or_adopt_attempt_draft_pr` pins the PR base to the same literal,
/// so one constant keeps the observed base SHA and the PR base in step.
pub const ATTEMPT_BASE_BRANCH: &str = "main";

/// How long one `stop` fences the attempt against a competing driver. Short
/// enough that a crashed stop is retryable by hand, long enough to cover the
/// comment / close / tag / retire sequence.
const ATTEMPT_LEASE_SECONDS: i64 = 120;

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
    pub proposal_id: String,
    pub proposal_short_id: String,
    pub build_attempt_id: String,
    pub build_attempt_short_id: String,
    pub title: String,
    pub body: String,
}

/// Uses only attempt-scoped provider operations, never task-PR APIs.
pub struct ProposalAttemptLifecycle {
    db: djinn_db::Database,
    github: GitHubApiClient,
    owner: String,
    repo: String,
    /// Identity presented to the attempt fencing lease. One value per server
    /// process, so a competing driver is always a different owner.
    owner_incarnation_id: String,
}

impl ProposalAttemptLifecycle {
    pub fn new(
        db: djinn_db::Database,
        github: GitHubApiClient,
        owner: String,
        repo: String,
        owner_incarnation_id: String,
    ) -> Self {
        Self {
            db,
            github,
            owner,
            repo,
            owner_incarnation_id,
        }
    }

    pub async fn start(&self, input: StartAttemptInput) -> Result<AttemptLifecycleOutcome> {
        if !self.enabled().await? {
            return Ok(AttemptLifecycleOutcome::Disabled);
        }
        let repo = ProposalBuildAttemptRepository::new(self.db.clone());
        // The base SHA is observed here rather than accepted from the caller,
        // so the reservation, the expected-absent ref create and the persisted
        // `base_sha` all carry the same exact observation of `main`.
        let observed_base_sha = match self
            .github
            .observe_exact_ref(
                &self.owner,
                &self.repo,
                &format!("heads/{ATTEMPT_BASE_BRANCH}"),
            )
            .await
        {
            ExactRefObservation::Found { sha } => sha,
            ExactRefObservation::NotFound => {
                return Err(anyhow!(
                    "attempt base branch {ATTEMPT_BASE_BRANCH} does not exist"
                ));
            }
            ExactRefObservation::ProviderFailure(e) => return Err(anyhow!(e)),
        };
        let mut attempt = match repo
            .reserve(&ReserveProposalBuildAttemptInput {
                proposal_id: input.proposal_id,
                proposal_short_id: input.proposal_short_id,
                build_attempt_id: input.build_attempt_id,
                build_attempt_short_id: input.build_attempt_short_id,
                observed_base_sha,
            })
            .await?
        {
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
        // Fence the close/tag/retire sequence: a competing driver holding a
        // live lease parks this one rather than racing it on the forge.
        if !self.acquire_attempt_lease(&repo, &attempt.id).await? {
            return self
                .park(&repo, attempt, DirectDeliveryParkReason::LeaseLost)
                .await;
        }
        let (pr, _) = self
            .github
            .get_pull_request(&self.owner, &self.repo, number)
            .await?;
        if pr.head.ref_name != attempt.branch_name
            || pr.head.sha != head
            || pr.base.ref_name != ATTEMPT_BASE_BRANCH
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

    /// Take the attempt fencing lease, taking over an expired lease only at the
    /// exact generation carried by the refusal. Returns `false` while another
    /// owner still holds a live lease.
    async fn acquire_attempt_lease(
        &self,
        repo: &ProposalBuildAttemptRepository,
        build_attempt_id: &str,
    ) -> Result<bool> {
        let expires_at =
            (chrono::Utc::now() + chrono::Duration::seconds(ATTEMPT_LEASE_SECONDS)).to_rfc3339();
        // The first request claims a never-leased attempt; a refusal carries
        // the generation this process must present to take over an expired one.
        let mut expected_generation = 0;
        for _ in 0..2 {
            match repo
                .acquire_lease(&AcquireProposalBuildAttemptLeaseInput {
                    build_attempt_id: build_attempt_id.to_owned(),
                    owner_incarnation_id: self.owner_incarnation_id.clone(),
                    expected_generation,
                    expires_at: expires_at.clone(),
                })
                .await?
            {
                AcquireProposalBuildAttemptLeaseResult::Acquired(_)
                | AcquireProposalBuildAttemptLeaseResult::Replayed(_) => return Ok(true),
                // This process already holds the lease. A crash-retry of its
                // own stop must not be fenced against itself, and the live
                // lease is not takeable from anyone else meanwhile.
                AcquireProposalBuildAttemptLeaseResult::Stale {
                    current: Some(current),
                } if current.owner_incarnation_id == self.owner_incarnation_id => {
                    return Ok(true);
                }
                AcquireProposalBuildAttemptLeaseResult::Stale {
                    current: Some(current),
                } if current.generation != expected_generation => {
                    expected_generation = current.generation;
                }
                AcquireProposalBuildAttemptLeaseResult::Stale { .. } => return Ok(false),
            }
        }
        Ok(false)
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
    use djinn_core::events::EventBus;
    use djinn_core::models::{DirectDeliveryParkReason, ProposalBuildAttemptLifecycle};
    use djinn_db::{
        AcquireProposalBuildAttemptLeaseInput, AcquireProposalBuildAttemptLeaseResult, Database,
        ProposalBuildAttemptRepository, ProposalCreateInput, ProposalRepository,
        test_support::activate_direct_delivery_epoch_for_test,
    };
    use djinn_provider::github_api::GitHubApiClient;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path, query_param},
    };

    /// A freshly created attempt branch points at the exact base head it was
    /// created from, so one constant is the base, the branch head and the PR head.
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
    /// The base-branch observation `start` makes before it reserves anything.
    async fn mount_base(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/repos/o/r/git/ref/heads/main"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"object":{"sha":SHA}})),
            )
            .mount(server)
            .await;
    }
    async fn mount_start(server: &MockServer, branch: &str, number: u64) {
        mount_base(server).await;
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
        service_owned(db, server, "incarnation-1")
    }
    fn service_owned(
        db: Database,
        server: &MockServer,
        owner_incarnation_id: &str,
    ) -> ProposalAttemptLifecycle {
        ProposalAttemptLifecycle::new(
            db,
            GitHubApiClient::for_user_token_with_base_url("test".into(), server.uri()),
            "o".into(),
            "r".into(),
            owner_incarnation_id.into(),
        )
    }
    fn input(proposal: &djinn_core::models::Proposal, id: &str, short: &str) -> StartAttemptInput {
        StartAttemptInput {
            proposal_id: proposal.id.clone(),
            proposal_short_id: proposal.short_id.clone(),
            build_attempt_id: id.into(),
            build_attempt_short_id: short.into(),
            title: "attempt".into(),
            body: "body".into(),
        }
    }

    #[tokio::test]
    async fn start_reserves_the_attempt_from_the_exact_observed_base_head() {
        let (db, _proposals, proposal) = db_and_proposal().await;
        let branch = format!("proposal/{}/a1", proposal.short_id);
        let server = MockServer::start().await;
        mount_start(&server, &branch, 1).await;
        let attempt = match service(db, &server)
            .start(input(&proposal, "attempt-base", "a1"))
            .await
            .expect("start")
        {
            AttemptLifecycleOutcome::Ready(value) => value,
            other => panic!("unexpected: {other:?}"),
        };
        // The persisted base is the SHA read from `heads/main`, not anything
        // the caller supplied, and the ref create carries that same SHA.
        assert_eq!(attempt.base_sha, SHA);
        let created = server
            .received_requests()
            .await
            .expect("recorded requests")
            .into_iter()
            .find(|request| {
                request.method == wiremock::http::Method::POST
                    && request.url.path() == "/repos/o/r/git/refs"
            })
            .expect("the attempt branch must be created");
        let body: serde_json::Value = serde_json::from_slice(&created.body).expect("ref body");
        assert_eq!(body["ref"], format!("refs/heads/{branch}"));
        assert_eq!(body["sha"], SHA);
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

    /// A live lease held by a different process is the one thing that stops a
    /// second driver reaching the forge at all.
    #[tokio::test]
    async fn stop_parks_lease_lost_while_another_owner_holds_a_live_lease() {
        let (db, _proposals, proposal) = db_and_proposal().await;
        let branch = format!("proposal/{}/a1", proposal.short_id);
        let start_server = MockServer::start().await;
        mount_start(&start_server, &branch, 1).await;
        let attempt = match service(db.clone(), &start_server)
            .start(input(&proposal, "attempt-lease", "a1"))
            .await
            .expect("start")
        {
            AttemptLifecycleOutcome::Ready(value) => value,
            other => panic!("unexpected: {other:?}"),
        };
        let repo = ProposalBuildAttemptRepository::new(db.clone());
        assert!(matches!(
            repo.acquire_lease(&AcquireProposalBuildAttemptLeaseInput {
                build_attempt_id: attempt.id.clone(),
                owner_incarnation_id: "competing-incarnation".into(),
                expected_generation: 0,
                expires_at: (chrono::Utc::now() + chrono::Duration::seconds(600)).to_rfc3339(),
            })
            .await
            .expect("competing lease"),
            AcquireProposalBuildAttemptLeaseResult::Acquired(_)
        ));

        // Nothing is mounted on this server: reaching the forge at all fails.
        let blocked = MockServer::start().await;
        let outcome = service_owned(db.clone(), &blocked, "incarnation-2")
            .stop(attempt.clone(), "stopped")
            .await
            .expect("stop");
        assert!(
            matches!(
                outcome,
                AttemptLifecycleOutcome::Parked {
                    reason: DirectDeliveryParkReason::LeaseLost,
                    ..
                }
            ),
            "a live competing lease must park, not race: {outcome:?}"
        );
        assert!(
            blocked
                .received_requests()
                .await
                .expect("recorded requests")
                .is_empty(),
            "a parked stop must not reach the forge"
        );
        let parked = repo
            .get(&attempt.id)
            .await
            .expect("load attempt")
            .expect("attempt");
        assert_eq!(
            parked.park_reason,
            Some(DirectDeliveryParkReason::LeaseLost)
        );
        assert_ne!(parked.lifecycle, ProposalBuildAttemptLifecycle::Retired);
    }

    /// An expired lease is taken over at the exact generation the refusal
    /// carried, so a crashed driver never wedges the attempt permanently.
    #[tokio::test]
    async fn stop_takes_over_an_expired_lease_at_the_observed_generation() {
        let (db, _proposals, proposal) = db_and_proposal().await;
        let branch = format!("proposal/{}/a1", proposal.short_id);
        let start_server = MockServer::start().await;
        mount_start(&start_server, &branch, 1).await;
        let attempt = match service(db.clone(), &start_server)
            .start(input(&proposal, "attempt-expired", "a1"))
            .await
            .expect("start")
        {
            AttemptLifecycleOutcome::Ready(value) => value,
            other => panic!("unexpected: {other:?}"),
        };
        let repo = ProposalBuildAttemptRepository::new(db.clone());
        repo.acquire_lease(&AcquireProposalBuildAttemptLeaseInput {
            build_attempt_id: attempt.id.clone(),
            owner_incarnation_id: "crashed-incarnation".into(),
            expected_generation: 0,
            expires_at: (chrono::Utc::now() - chrono::Duration::seconds(600)).to_rfc3339(),
        })
        .await
        .expect("expired lease");

        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(pr(1, &branch, "open")))
            .mount(&start_server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues/1/comments"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({})))
            .mount(&start_server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/repos/o/r/pulls/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(pr(1, &branch, "closed")))
            .mount(&start_server)
            .await;
        assert!(matches!(
            service_owned(db, &start_server, "incarnation-2")
                .stop(attempt, "stopped")
                .await
                .expect("stop"),
            AttemptLifecycleOutcome::Retired(_)
        ));
    }

    /// A failed draft-PR POST still proves the attempt-scoped request boundary
    /// was crossed: the request reaches the forge before its response can be
    /// classified as a provider failure.
    #[tokio::test]
    async fn attempt_pr_request_reaches_the_forge_before_provider_failure() {
        let (db, _proposals, proposal) = db_and_proposal().await;
        let branch = format!("proposal/{}/failed", proposal.short_id);
        let server = MockServer::start().await;
        mount_base(&server).await;
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

        assert!(
            service(db, &server)
                .start(input(&proposal, "attempt-failure", "failed"))
                .await
                .is_err()
        );
        let attempt_pr_requests = server
            .received_requests()
            .await
            .expect("recorded requests")
            .into_iter()
            .filter(|request| {
                request.method == wiremock::http::Method::POST
                    && request.url.path() == "/repos/o/r/pulls"
            })
            .count();
        assert_eq!(
            attempt_pr_requests, 1,
            "the attempt draft-PR request must reach the forge before classification"
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
