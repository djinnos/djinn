//! Crash-convergent, dark direct append orchestration.
//!
//! This module intentionally has no task-PR API, uses only a non-force
//! expected-old update, and finalizes conflict generations before parking their
//! build attempt.

use std::{collections::HashSet, path::PathBuf, time::Duration};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use djinn_core::models::{
    DirectDeliveryParkReason, MappedHeadRetryDelivery, ReworkDelivery, TaskDelivery,
    TaskDeliveryIdentity, TaskIntegrated, TransitionAction,
};
use djinn_db::{
    Database, DeliveryFinalizeInput, DeliveryMappedHeadRetryInput, DeliveryPrepareInput,
    DeliveryReworkInput, DeliveryTransitionResult, DirectDeliveryCapabilityRepository,
    DirectDeliverySchemaCapability, ProposalBuildAttemptRepository, ResolveTaskActiveAttemptResult,
    TaskIntegrationResult, TaskRepository,
};
use djinn_git::{
    DirectDeliveryBuild, DirectDeliveryInput, DirectDeliverySignature,
    build_direct_delivery_candidate,
};
use djinn_provider::github_api::{ExpectedOldShaRefUpdateResult, GitHubApiClient};
use djinn_workspace::MirrorManager;

pub const LEGACY_DELIVERY_LABEL: &str = "direct-delivery-legacy";

/// Effect boundaries reached by the epoch-aware delivery routing path.
/// The recorder is test-only and calls to it sit beside real production effects.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BoundaryOperation {
    CapabilityProbe,
    ResolveTaskActiveAttempt,
    NoProposalOwnerPark,
    DirectAppend,
    SimpleClose,
    SupervisorPrOpen,
    TaskPrCreate,
    TaskPrMerge,
    TaskPrAutoMerge,
    TaskPrApproval,
    TaskPrSignoff,
    TaskPrCustomEnqueue,
}

#[cfg(test)]
static BOUNDARY_OPERATIONS: std::sync::Mutex<Vec<BoundaryOperation>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) fn clear_boundary_operations() {
    BOUNDARY_OPERATIONS.lock().unwrap().clear();
}

#[cfg(test)]
pub(crate) fn take_boundary_operations() -> Vec<BoundaryOperation> {
    std::mem::take(&mut *BOUNDARY_OPERATIONS.lock().unwrap())
}

/// A no-op outside tests, preserving production behavior and the disabled epoch.
pub(crate) fn observe_boundary_operation(operation: &'static str) {
    #[cfg(test)]
    {
        let operation = match operation {
            "capability_probe" => BoundaryOperation::CapabilityProbe,
            "resolve_task_active_attempt" => BoundaryOperation::ResolveTaskActiveAttempt,
            "no_proposal_owner_park" => BoundaryOperation::NoProposalOwnerPark,
            "direct_append" => BoundaryOperation::DirectAppend,
            "simple_close" => BoundaryOperation::SimpleClose,
            "supervisor_pr_open" => BoundaryOperation::SupervisorPrOpen,
            "task_pr_create" => BoundaryOperation::TaskPrCreate,
            "task_pr_merge" => BoundaryOperation::TaskPrMerge,
            "task_pr_auto_merge" => BoundaryOperation::TaskPrAutoMerge,
            "task_pr_approval" => BoundaryOperation::TaskPrApproval,
            "task_pr_signoff" => BoundaryOperation::TaskPrSignoff,
            "task_pr_custom_enqueue" => BoundaryOperation::TaskPrCustomEnqueue,
            _ => return,
        };
        BOUNDARY_OPERATIONS.lock().unwrap().push(operation);
    }
    #[cfg(not(test))]
    let _ = operation;
}

/// The only epoch-aware routing decision used by ready admission and completion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirectDeliveryAdmission {
    Legacy,
    Direct { attempt: ActiveAttempt },
    NoProposalOwner,
}

/// Persist the active-epoch ownership failure before any task-PR side effect.
pub async fn park_no_proposal_owner(repo: &TaskRepository, task_id: &str) -> Result<()> {
    repo.transition(
        task_id,
        TransitionAction::Escalate,
        "coordinator",
        "system",
        Some("no_proposal_owner"),
        None,
    )
    .await?;
    observe_boundary_operation("no_proposal_owner_park");
    Ok(())
}

/// Read-only, fail-closed epoch and owner selection. Ownership comes exclusively
/// from `resolve_task_active_attempt`, never coordinator task fields.
pub async fn admit_direct_delivery(db: Database, task_id: &str) -> Result<DirectDeliveryAdmission> {
    let capability = DirectDeliveryCapabilityRepository::new(db.clone())
        .probe()
        .await?;
    observe_boundary_operation("capability_probe");
    match capability {
        DirectDeliverySchemaCapability::SupportedDisabled { .. } => {
            Ok(DirectDeliveryAdmission::Legacy)
        }
        DirectDeliverySchemaCapability::SupportedActive { .. } => {
            let task = TaskRepository::new(db.clone(), djinn_core::events::EventBus::noop())
                .get(task_id)
                .await?
                .ok_or_else(|| anyhow!("task {task_id} disappeared during delivery admission"))?;
            let labels: Vec<String> = serde_json::from_str(&task.labels)
                .map_err(|error| anyhow!("task {task_id} has invalid labels: {error}"))?;
            if has_explicit_legacy_delivery(task.pr_url.as_deref(), &labels) {
                return Ok(DirectDeliveryAdmission::Legacy);
            }
            let resolved = ProposalBuildAttemptRepository::new(db)
                .resolve_task_active_attempt(task_id)
                .await?;
            observe_boundary_operation("resolve_task_active_attempt");
            match resolved {
                ResolveTaskActiveAttemptResult::Resolved(resolved) => {
                    let attempt = resolved.attempt;
                    Ok(DirectDeliveryAdmission::Direct {
                        attempt: ActiveAttempt {
                            build_attempt_id: attempt.id,
                            branch_name: attempt.branch_name,
                            branch_head_sha: attempt
                                .branch_head_sha
                                .ok_or_else(|| anyhow!("active attempt has no branch head"))?,
                        },
                    })
                }
                ResolveTaskActiveAttemptResult::NoProposalOwner { .. }
                | ResolveTaskActiveAttemptResult::NoActiveAttempt { .. }
                | ResolveTaskActiveAttemptResult::AmbiguousProposalOwner { .. } => {
                    Ok(DirectDeliveryAdmission::NoProposalOwner)
                }
            }
        }
        DirectDeliverySchemaCapability::MissingSchema { missing_relations } => Err(anyhow!(
            "direct_delivery_v1 schema unavailable: {}",
            missing_relations.join(", ")
        )),
        DirectDeliverySchemaCapability::MissingEpoch => {
            Err(anyhow!("direct_delivery_v1 epoch is unavailable"))
        }
        DirectDeliverySchemaCapability::UnknownEpochState { state, generation } => Err(anyhow!(
            "direct_delivery_v1 has unknown state {state} at generation {generation}"
        )),
    }
}

/// Legacy identities are an explicit routing boundary: admission may inspect a
/// task PR, but direct delivery must never replace or otherwise mutate it.
fn has_explicit_legacy_delivery(pr_url: Option<&str>, labels: &[String]) -> bool {
    pr_url.is_some() || labels.iter().any(|label| label == LEGACY_DELIVERY_LABEL)
}

/// Direct completion adapter; it exposes no legacy task-PR operation.
#[allow(clippy::too_many_arguments)]
pub async fn deliver_task_branch(
    db: Database,
    event_bus: djinn_core::events::EventBus,
    mirror: &MirrorManager,
    task_id: &str,
    project_id: &str,
    task_branch: &str,
    base_branch: &str,
    owner: String,
    repo: String,
    github: GitHubApiClient,
) -> Result<DeliveryOutcome> {
    let workspace = mirror.clone_ephemeral(project_id, task_branch).await?;
    let repository = workspace.path_buf();
    let source_sha =
        djinn_git::run_git_command(repository.clone(), vec!["rev-parse".into(), "HEAD".into()])
            .await?
            .stdout
            .trim()
            .to_owned();
    let normalized_patch = djinn_git::run_git_command(
        repository.clone(),
        vec![
            "diff".into(),
            "--binary".into(),
            format!("origin/{base_branch}..HEAD"),
        ],
    )
    .await?
    .stdout;
    let signature = DirectDeliverySignature {
        name: "Djinn Direct Delivery".into(),
        email: "direct-delivery@djinn.local".into(),
        when: "0 +0000".into(),
    };
    let tasks = TaskRepository::new(db.clone(), event_bus.clone());
    // The adapter owns no proposal-routing rule. Resolve the same canonical
    // active attempt used by admission before selecting an immutable replay.
    let attempts = ProposalBuildAttemptRepository::new(db.clone());
    let active_attempt = match attempts.resolve_task_active_attempt(task_id).await? {
        ResolveTaskActiveAttemptResult::Resolved(resolved) => resolved.attempt,
        other => {
            return Err(anyhow!(
                "no canonical active attempt for {task_id}: {other:?}"
            ));
        }
    };
    let source = resume_delivery_source(
        task_id,
        source_sha,
        normalized_patch,
        tasks
            .latest_delivery_for_attempt(&active_attempt.id, task_id)
            .await?
            .as_ref(),
    );
    let ledger = RepositoryDeliveryLedger::new(db.clone(), attempts, tasks);
    DirectDeliveryEngine::new(
        ledger,
        GitHubAttemptRef::new(github, owner, repo),
        GitCandidateBuilder::new(repository, signature.clone(), signature),
    )
    .deliver(source)
    .await
}

/// Keep replay tied to the durable generation rather than the current source
/// checkout. A mapped-head successor may be Applying after a provider response
/// was lost; only its original identity and transition can reconcile it.
fn resume_delivery_source(
    task_id: &str,
    source_sha: String,
    normalized_patch: String,
    latest: Option<&TaskDelivery>,
) -> DeliverySource {
    match latest {
        Some(delivery) => DeliverySource {
            task_id: task_id.into(),
            delivery_generation: delivery.identity.delivery_generation,
            transition_id: delivery.prepare_transition_id.clone(),
            source_sha: delivery.source_sha.clone(),
            // Exact-candidate reconciliation occurs before candidate rebuilding.
            // The current patch remains available for a definitive retry.
            normalized_patch,
        },
        None => DeliverySource {
            task_id: task_id.into(),
            delivery_generation: 1,
            transition_id: format!("direct-delivery:{task_id}:1"),
            source_sha,
            normalized_patch,
        },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LedgerResult {
    Applied,
    Replayed,
    Stale,
}

/// The sole production remote seam. It cannot create, merge, or approve task PRs.
pub struct GitHubAttemptRef {
    github: GitHubApiClient,
    owner: String,
    repo: String,
}
impl GitHubAttemptRef {
    pub fn new(github: GitHubApiClient, owner: String, repo: String) -> Self {
        Self {
            github,
            owner,
            repo,
        }
    }
}
#[async_trait]
impl AttemptRef for GitHubAttemptRef {
    async fn observe(&self, branch: &str) -> Result<Option<String>> {
        Ok(self.github.get_ref(&self.owner, &self.repo, branch).await?)
    }
    async fn update_expected_old(
        &self,
        branch: &str,
        expected: &str,
        new: &str,
    ) -> Result<RemoteUpdate> {
        match self
            .github
            .update_ref_expected_old_sha(&self.owner, &self.repo, branch, expected, new)
            .await
        {
            ExpectedOldShaRefUpdateResult::Updated { sha } => Ok(RemoteUpdate::Updated { sha }),
            ExpectedOldShaRefUpdateResult::StaleObservedHead { observed_sha } => {
                Ok(RemoteUpdate::Stale { observed_sha })
            }
            ExpectedOldShaRefUpdateResult::ProviderFailure(error) => Err(anyhow!(error)),
        }
    }
}

/// Deterministic object-only candidate construction adapter.
pub struct GitCandidateBuilder {
    repository: PathBuf,
    author: DirectDeliverySignature,
    committer: DirectDeliverySignature,
}
impl GitCandidateBuilder {
    pub fn new(
        repository: PathBuf,
        author: DirectDeliverySignature,
        committer: DirectDeliverySignature,
    ) -> Self {
        Self {
            repository,
            author,
            committer,
        }
    }
}
#[async_trait]
impl CandidateBuilder for GitCandidateBuilder {
    async fn build(
        &self,
        identity: &TaskDeliveryIdentity,
        source: &DeliverySource,
        parent: &str,
    ) -> Result<CandidateBuild> {
        let input = DirectDeliveryInput {
            identity: identity.clone(),
            selected_parent_sha: parent.into(),
            source_sha: source.source_sha.clone(),
            normalized_patch: source.normalized_patch.clone(),
            author: self.author.clone(),
            committer: self.committer.clone(),
            message: format!(
                "Direct delivery {}\n\nSource: {}",
                source.task_id, source.source_sha
            ),
        };
        match build_direct_delivery_candidate(&self.repository, &input).await? {
            DirectDeliveryBuild::Clean(candidate) => Ok(CandidateBuild::Clean(Candidate {
                candidate_sha: candidate.candidate_sha,
                patch_digest: candidate.normalized_patch_digest,
                selected_parent_sha: candidate.first_parent_sha,
            })),
            DirectDeliveryBuild::Conflict {
                normalized_patch_digest,
                reason,
            } => Ok(CandidateBuild::Conflict {
                patch_digest: normalized_patch_digest,
                reason,
            }),
            DirectDeliveryBuild::InvalidSource { reason } => Err(anyhow!(reason)),
        }
    }
}

/// Public-db-only production ledger adapter.
pub struct RepositoryDeliveryLedger {
    capability: DirectDeliveryCapabilityRepository,
    attempts: ProposalBuildAttemptRepository,
    tasks: TaskRepository,
}
impl RepositoryDeliveryLedger {
    pub fn new(
        db: Database,
        attempts: ProposalBuildAttemptRepository,
        tasks: TaskRepository,
    ) -> Self {
        Self {
            capability: DirectDeliveryCapabilityRepository::new(db),
            attempts,
            tasks,
        }
    }
}
fn transition_result(result: DeliveryTransitionResult) -> LedgerResult {
    match result {
        DeliveryTransitionResult::Applied(_) => LedgerResult::Applied,
        DeliveryTransitionResult::Replayed(_) => LedgerResult::Replayed,
        DeliveryTransitionResult::Stale { .. } => LedgerResult::Stale,
    }
}
#[async_trait]
impl DeliveryLedger for RepositoryDeliveryLedger {
    async fn direct_delivery_enabled(&self) -> Result<bool> {
        Ok(matches!(
            self.capability.probe().await?,
            DirectDeliverySchemaCapability::SupportedActive { .. }
        ))
    }
    async fn retry_from_mapped_head(
        &self,
        retry: MappedHeadRetryDelivery,
        source: &DeliverySource,
        candidate: &Candidate,
    ) -> Result<LedgerResult> {
        Ok(transition_result(
            self.tasks
                .retry_delivery_from_mapped_head(&DeliveryMappedHeadRetryInput {
                    retry,
                    source_sha: source.source_sha.clone(),
                    patch_digest: candidate.patch_digest.clone(),
                    selected_parent_sha: candidate.selected_parent_sha.clone(),
                    candidate_sha: candidate.candidate_sha.clone(),
                })
                .await?,
        ))
    }
    async fn resolve_active_attempt(&self, task_id: &str) -> Result<ActiveAttempt> {
        match self.attempts.resolve_task_active_attempt(task_id).await? {
            ResolveTaskActiveAttemptResult::Resolved(resolved) => Ok(ActiveAttempt {
                build_attempt_id: resolved.attempt.id,
                branch_name: resolved.attempt.branch_name,
                branch_head_sha: resolved
                    .attempt
                    .branch_head_sha
                    .ok_or_else(|| anyhow!("active attempt has no branch head"))?,
            }),
            other => Err(anyhow!(
                "no canonical active attempt for {task_id}: {other:?}"
            )),
        }
    }
    async fn prepared_candidate(
        &self,
        identity: &TaskDeliveryIdentity,
    ) -> Result<Option<Candidate>> {
        Ok(self
            .tasks
            .get_delivery(identity)
            .await?
            .map(|delivery| Candidate {
                candidate_sha: delivery.candidate_sha,
                patch_digest: delivery.patch_digest,
                selected_parent_sha: delivery.selected_parent_sha,
            }))
    }
    async fn prepare(
        &self,
        identity: &TaskDeliveryIdentity,
        source: &DeliverySource,
        candidate: &Candidate,
    ) -> Result<LedgerResult> {
        Ok(transition_result(
            self.tasks
                .prepare_delivery(&DeliveryPrepareInput {
                    identity: identity.clone(),
                    transition_id: source.transition_id.clone(),
                    source_sha: source.source_sha.clone(),
                    patch_digest: candidate.patch_digest.clone(),
                    selected_parent_sha: candidate.selected_parent_sha.clone(),
                    candidate_sha: candidate.candidate_sha.clone(),
                })
                .await?,
        ))
    }
    async fn begin_apply(
        &self,
        identity: &TaskDeliveryIdentity,
        transition: &str,
    ) -> Result<LedgerResult> {
        Ok(transition_result(
            self.tasks
                .begin_delivery_apply(&DeliveryFinalizeInput {
                    identity: identity.clone(),
                    transition_id: transition.into(),
                    conflict_reason: None,
                })
                .await?,
        ))
    }
    async fn finalize_conflict(
        &self,
        identity: &TaskDeliveryIdentity,
        transition: &str,
        reason: &str,
    ) -> Result<LedgerResult> {
        Ok(transition_result(
            self.tasks
                .finalize_delivery_conflict(&DeliveryFinalizeInput {
                    identity: identity.clone(),
                    transition_id: transition.into(),
                    conflict_reason: Some(reason.into()),
                })
                .await?,
        ))
    }
    async fn integrate(&self, integrated: TaskIntegrated) -> Result<LedgerResult> {
        Ok(match self.tasks.task_integrated(&integrated).await? {
            TaskIntegrationResult::Integrated(_) => LedgerResult::Applied,
            TaskIntegrationResult::Replayed(_) => LedgerResult::Replayed,
            TaskIntegrationResult::Stale { .. } => LedgerResult::Stale,
        })
    }
    async fn is_mapped_first_parent(&self, attempt: &ActiveAttempt, sha: &str) -> Result<bool> {
        Ok(attempt.branch_head_sha == sha
            || self
                .tasks
                .is_delivery_candidate_for_attempt(&attempt.build_attempt_id, sha)
                .await?)
    }
    async fn rework(
        &self,
        rework: ReworkDelivery,
        source: &DeliverySource,
        candidate: &Candidate,
    ) -> Result<LedgerResult> {
        Ok(transition_result(
            self.tasks
                .rework_delivery(&DeliveryReworkInput {
                    rework,
                    source_sha: source.source_sha.clone(),
                    patch_digest: candidate.patch_digest.clone(),
                    selected_parent_sha: candidate.selected_parent_sha.clone(),
                    candidate_sha: candidate.candidate_sha.clone(),
                })
                .await?,
        ))
    }
    async fn park(
        &self,
        attempt_id: &str,
        _: &TaskDeliveryIdentity,
        reason: ParkReason,
        _: &str,
    ) -> Result<()> {
        self.attempts
            .park(
                attempt_id,
                match reason {
                    ParkReason::TaskAppendConflict => DirectDeliveryParkReason::DeliveryConflict,
                    ParkReason::UnexpectedBranchHead => {
                        DirectDeliveryParkReason::UnexpectedBranchHead
                    }
                    ParkReason::StaleHeadRetryBound => {
                        DirectDeliveryParkReason::MappedHeadRetryBound
                    }
                },
            )
            .await?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveAttempt {
    pub build_attempt_id: String,
    pub branch_name: String,
    pub branch_head_sha: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliverySource {
    pub task_id: String,
    pub delivery_generation: i64,
    pub transition_id: String,
    pub source_sha: String,
    pub normalized_patch: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub candidate_sha: String,
    pub patch_digest: String,
    pub selected_parent_sha: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CandidateBuild {
    Clean(Candidate),
    Conflict {
        patch_digest: String,
        reason: String,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteUpdate {
    Updated { sha: String },
    Stale { observed_sha: Option<String> },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParkReason {
    TaskAppendConflict,
    UnexpectedBranchHead,
    StaleHeadRetryBound,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeliveryOutcome {
    Integrated { candidate_sha: String },
    ConflictParked { reason: String },
    UnexpectedHeadParked { observed_sha: Option<String> },
    RetryBoundParked { observed_heads: usize },
    Disabled,
}

#[async_trait]
pub trait DeliveryLedger: Send + Sync {
    async fn direct_delivery_enabled(&self) -> Result<bool>;
    async fn resolve_active_attempt(&self, task_id: &str) -> Result<ActiveAttempt>;
    /// Immutable candidate already recorded for this exact generation.
    async fn prepared_candidate(&self, _: &TaskDeliveryIdentity) -> Result<Option<Candidate>> {
        Ok(None)
    }
    async fn prepare(
        &self,
        identity: &TaskDeliveryIdentity,
        source: &DeliverySource,
        candidate: &Candidate,
    ) -> Result<LedgerResult>;
    async fn retry_from_mapped_head(
        &self,
        retry: MappedHeadRetryDelivery,
        source: &DeliverySource,
        candidate: &Candidate,
    ) -> Result<LedgerResult>;
    async fn begin_apply(
        &self,
        identity: &TaskDeliveryIdentity,
        transition_id: &str,
    ) -> Result<LedgerResult>;
    /// Durable terminal fact; must succeed before conflict parking.
    async fn finalize_conflict(
        &self,
        identity: &TaskDeliveryIdentity,
        transition_id: &str,
        reason: &str,
    ) -> Result<LedgerResult>;
    async fn integrate(&self, integrated: TaskIntegrated) -> Result<LedgerResult>;
    async fn is_mapped_first_parent(&self, attempt: &ActiveAttempt, sha: &str) -> Result<bool>;
    async fn rework(
        &self,
        rework: ReworkDelivery,
        source: &DeliverySource,
        candidate: &Candidate,
    ) -> Result<LedgerResult>;
    async fn park(
        &self,
        attempt_id: &str,
        identity: &TaskDeliveryIdentity,
        reason: ParkReason,
        detail: &str,
    ) -> Result<()>;
}
#[async_trait]
pub trait AttemptRef: Send + Sync {
    async fn observe(&self, branch: &str) -> Result<Option<String>>;
    async fn update_expected_old(
        &self,
        branch: &str,
        expected_old: &str,
        new: &str,
    ) -> Result<RemoteUpdate>;
}
#[async_trait]
pub trait CandidateBuilder: Send + Sync {
    async fn build(
        &self,
        identity: &TaskDeliveryIdentity,
        source: &DeliverySource,
        parent: &str,
    ) -> Result<CandidateBuild>;
}

/// Direct-delivery engine. No task PR creation, merge, approval, signoff, or enqueue APIs are reachable from this type.
pub struct DirectDeliveryEngine<L, R, B> {
    ledger: L,
    remote: R,
    builder: B,
}
impl<L: DeliveryLedger, R: AttemptRef, B: CandidateBuilder> DirectDeliveryEngine<L, R, B> {
    pub fn new(ledger: L, remote: R, builder: B) -> Self {
        Self {
            ledger,
            remote,
            builder,
        }
    }

    async fn prepare_generation(
        &self,
        identity: &TaskDeliveryIdentity,
        source: &DeliverySource,
        candidate: &Candidate,
    ) -> Result<LedgerResult> {
        if identity.delivery_generation == 1 {
            return self.ledger.prepare(identity, source, candidate).await;
        }
        self.ledger
            .rework(
                ReworkDelivery::new(
                    &source.transition_id,
                    &identity.build_attempt_id,
                    &identity.task_id,
                    identity.delivery_generation - 1,
                    identity.delivery_generation,
                )?,
                source,
                candidate,
            )
            .await
    }

    pub async fn deliver(&self, source: DeliverySource) -> Result<DeliveryOutcome> {
        if !self.ledger.direct_delivery_enabled().await? {
            return Ok(DeliveryOutcome::Disabled);
        }
        let attempt = self.ledger.resolve_active_attempt(&source.task_id).await?;
        let identity = TaskDeliveryIdentity::new(
            &attempt.build_attempt_id,
            &source.task_id,
            source.delivery_generation,
        )?;
        // A crash after remote success can leave the ref at this generation's
        // exact durable candidate. Reconcile it before selecting a new parent;
        // otherwise rebuilding would produce a second commit on top of it.
        let observed = self.remote.observe(&attempt.branch_name).await?;
        if let (Some(head), Some(candidate)) =
            (&observed, self.ledger.prepared_candidate(&identity).await?)
            && *head == candidate.candidate_sha
        {
            return self.integrate(identity, candidate.candidate_sha).await;
        }
        // Select and validate the parent before recording immutable preparation
        // facts. A mapped append observed here is a valid candidate parent.
        let parent = match observed {
            Some(head) if self.ledger.is_mapped_first_parent(&attempt, &head).await? => head,
            observed => return self.park_unexpected(&attempt, &identity, observed).await,
        };
        let built = self.builder.build(&identity, &source, &parent).await?;
        let candidate = match built {
            CandidateBuild::Clean(candidate) if candidate.selected_parent_sha == parent => {
                candidate
            }
            CandidateBuild::Clean(_) => {
                return Err(anyhow!(
                    "candidate builder returned a different selected parent"
                ));
            }
            CandidateBuild::Conflict {
                patch_digest,
                reason,
            } => {
                // `candidate_sha` is a durable conflict sentinel, never a ref target.
                let conflict = Candidate {
                    candidate_sha: format!("conflict:{patch_digest}"),
                    patch_digest,
                    selected_parent_sha: parent,
                };
                if self
                    .prepare_generation(&identity, &source, &conflict)
                    .await?
                    == LedgerResult::Stale
                {
                    return Ok(DeliveryOutcome::RetryBoundParked { observed_heads: 0 });
                }
                // A crash after terminal finalization makes the applying transition stale. Exact conflict finalization replays and must still complete parking.
                let applying = self
                    .ledger
                    .begin_apply(&identity, &source.transition_id)
                    .await?;
                let finalized = self
                    .ledger
                    .finalize_conflict(
                        &identity,
                        &format!("{}:conflict", source.transition_id),
                        &reason,
                    )
                    .await?;
                if finalized == LedgerResult::Stale
                    || (applying == LedgerResult::Stale && finalized != LedgerResult::Replayed)
                {
                    return Ok(DeliveryOutcome::RetryBoundParked { observed_heads: 0 });
                }
                self.ledger
                    .park(
                        &attempt.build_attempt_id,
                        &identity,
                        ParkReason::TaskAppendConflict,
                        &reason,
                    )
                    .await?;
                return Ok(DeliveryOutcome::ConflictParked { reason });
            }
        };
        // Preparation/rework and applying must be auditable before any ref
        // mutation. Every mapped-head loss supersedes this immutable generation
        // and prepares its successor before attempting another CAS.
        let mut identity = identity;
        let mut delivery_source = source;
        let mut candidate = candidate;
        let mut parent = parent;
        if self
            .prepare_generation(&identity, &delivery_source, &candidate)
            .await?
            == LedgerResult::Stale
            || self
                .ledger
                .begin_apply(&identity, &delivery_source.transition_id)
                .await?
                == LedgerResult::Stale
        {
            return Ok(DeliveryOutcome::RetryBoundParked { observed_heads: 0 });
        }
        let mut observed_mapped_heads = HashSet::new();
        loop {
            // This is the real ref mutation; route selection and failed
            // resolution must not be reported as an append.
            observe_boundary_operation("direct_append");
            match self
                .remote
                .update_expected_old(&attempt.branch_name, &parent, &candidate.candidate_sha)
                .await?
            {
                RemoteUpdate::Updated { sha } if sha == candidate.candidate_sha => {
                    return self.integrate(identity, candidate.candidate_sha).await;
                }
                RemoteUpdate::Updated { sha } => {
                    return Err(anyhow!("remote CAS acknowledged unexpected SHA {sha}"));
                }
                RemoteUpdate::Stale {
                    observed_sha: Some(head),
                } if head == candidate.candidate_sha => {
                    return self.integrate(identity, candidate.candidate_sha).await;
                }
                RemoteUpdate::Stale {
                    observed_sha: Some(head),
                } => {
                    if !self.ledger.is_mapped_first_parent(&attempt, &head).await? {
                        return self.park_unexpected(&attempt, &identity, Some(head)).await;
                    }
                    // A replayed stale response for the same mapped head is not
                    // another topology change. Retry its prepared successor,
                    // rather than minting another immutable generation.
                    if !observed_mapped_heads.insert(head.clone()) {
                        debug_assert_eq!(candidate.selected_parent_sha, head);
                        parent = head;
                        continue;
                    }
                    // The retry budget is a bound on distinct mapped heads; do
                    // not prepare a successor after observing the third one.
                    if observed_mapped_heads.len() >= 3 {
                        self.ledger
                            .park(
                                &attempt.build_attempt_id,
                                &identity,
                                ParkReason::StaleHeadRetryBound,
                                &head,
                            )
                            .await?;
                        return Ok(DeliveryOutcome::RetryBoundParked {
                            observed_heads: observed_mapped_heads.len(),
                        });
                    }
                    let next_generation = identity.delivery_generation + 1;
                    let next_identity = TaskDeliveryIdentity::new(
                        &attempt.build_attempt_id,
                        &delivery_source.task_id,
                        next_generation,
                    )?;
                    let next_source = DeliverySource {
                        delivery_generation: next_generation,
                        transition_id: format!(
                            "{}:mapped-head:{next_generation}",
                            delivery_source.transition_id
                        ),
                        ..delivery_source.clone()
                    };
                    let next_candidate = match self
                        .builder
                        .build(&next_identity, &next_source, &head)
                        .await?
                    {
                        CandidateBuild::Clean(candidate)
                            if candidate.selected_parent_sha == head =>
                        {
                            candidate
                        }
                        CandidateBuild::Clean(_) => {
                            return Err(anyhow!(
                                "candidate builder returned a different selected parent"
                            ));
                        }
                        CandidateBuild::Conflict {
                            patch_digest,
                            reason,
                        } => {
                            // A topology retry is a distinct immutable generation:
                            // persist its conflict sentinel and terminal fact before
                            // parking, leaving the prior generation superseded.
                            let conflict = Candidate {
                                candidate_sha: format!("conflict:{patch_digest}"),
                                patch_digest,
                                selected_parent_sha: head.clone(),
                            };
                            if self
                                .ledger
                                .retry_from_mapped_head(
                                    MappedHeadRetryDelivery::new(
                                        &next_source.transition_id,
                                        &attempt.build_attempt_id,
                                        &next_source.task_id,
                                        identity.delivery_generation,
                                        next_generation,
                                    )?,
                                    &next_source,
                                    &conflict,
                                )
                                .await?
                                == LedgerResult::Stale
                            {
                                return Ok(DeliveryOutcome::RetryBoundParked {
                                    observed_heads: observed_mapped_heads.len(),
                                });
                            }
                            let applying = self
                                .ledger
                                .begin_apply(&next_identity, &next_source.transition_id)
                                .await?;
                            let finalized = self
                                .ledger
                                .finalize_conflict(
                                    &next_identity,
                                    &format!("{}:conflict", next_source.transition_id),
                                    &reason,
                                )
                                .await?;
                            if finalized == LedgerResult::Stale
                                || (applying == LedgerResult::Stale
                                    && finalized != LedgerResult::Replayed)
                            {
                                return Ok(DeliveryOutcome::RetryBoundParked {
                                    observed_heads: observed_mapped_heads.len(),
                                });
                            }
                            self.ledger
                                .park(
                                    &attempt.build_attempt_id,
                                    &next_identity,
                                    ParkReason::TaskAppendConflict,
                                    &reason,
                                )
                                .await?;
                            return Ok(DeliveryOutcome::ConflictParked { reason });
                        }
                    };
                    if self
                        .ledger
                        .retry_from_mapped_head(
                            MappedHeadRetryDelivery::new(
                                &next_source.transition_id,
                                &attempt.build_attempt_id,
                                &next_source.task_id,
                                identity.delivery_generation,
                                next_generation,
                            )?,
                            &next_source,
                            &next_candidate,
                        )
                        .await?
                        == LedgerResult::Stale
                        || self
                            .ledger
                            .begin_apply(&next_identity, &next_source.transition_id)
                            .await?
                            == LedgerResult::Stale
                    {
                        return Ok(DeliveryOutcome::RetryBoundParked {
                            observed_heads: observed_mapped_heads.len(),
                        });
                    }
                    identity = next_identity;
                    delivery_source = next_source;
                    candidate = next_candidate;
                    parent = head;
                }
                RemoteUpdate::Stale { observed_sha: None } => {
                    return self.park_unexpected(&attempt, &identity, None).await;
                }
            }
        }
    }
    async fn integrate(
        &self,
        identity: TaskDeliveryIdentity,
        sha: String,
    ) -> Result<DeliveryOutcome> {
        // A concurrent append can advance the remote through this candidate
        // before its selected parent has finalized the durable attempt head.
        // Reconcile the exact system-only fact until its durable predecessor
        // commits. This is deliberately not a scheduler-yield budget: a real
        // database transaction can remain in progress longer than an arbitrary
        // number of executor turns. No replay here can mutate the remote ref
        // or change this generation's identity.
        loop {
            if self
                .ledger
                .integrate(TaskIntegrated::new(identity.clone(), &sha, &sha, &sha)?)
                .await?
                != LedgerResult::Stale
            {
                return Ok(DeliveryOutcome::Integrated { candidate_sha: sha });
            }
            // `TaskIntegrated` performs the transactional durable-head check.
            // A stale result means its selected parent has not finalized yet;
            // wait before asking that transaction to reconcile again rather
            // than misclassifying a transient parent transaction as an
            // unexpected remote head.
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
    async fn park_unexpected(
        &self,
        attempt: &ActiveAttempt,
        identity: &TaskDeliveryIdentity,
        observed: Option<String>,
    ) -> Result<DeliveryOutcome> {
        self.ledger
            .park(
                &attempt.build_attempt_id,
                identity,
                ParkReason::UnexpectedBranchHead,
                observed.as_deref().unwrap_or("attempt ref is absent"),
            )
            .await?;
        Ok(DeliveryOutcome::UnexpectedHeadParked {
            observed_sha: observed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    #[derive(Clone)]
    struct Ledger {
        calls: Arc<Mutex<Vec<String>>>,
        mapped: bool,
        replay_terminal_conflict: bool,
        rework_result: LedgerResult,
    }
    #[async_trait]
    impl DeliveryLedger for Ledger {
        async fn direct_delivery_enabled(&self) -> Result<bool> {
            Ok(true)
        }
        async fn resolve_active_attempt(&self, _: &str) -> Result<ActiveAttempt> {
            Ok(ActiveAttempt {
                build_attempt_id: "attempt".into(),
                branch_name: "proposal/p/a".into(),
                branch_head_sha: "base".into(),
            })
        }
        async fn prepare(
            &self,
            _: &TaskDeliveryIdentity,
            _: &DeliverySource,
            c: &Candidate,
        ) -> Result<LedgerResult> {
            self.calls
                .lock()
                .map_err(|_| anyhow!("poison"))?
                .push(format!("prepare:{}", c.candidate_sha));
            Ok(if self.replay_terminal_conflict {
                LedgerResult::Replayed
            } else {
                LedgerResult::Applied
            })
        }
        async fn begin_apply(&self, _: &TaskDeliveryIdentity, _: &str) -> Result<LedgerResult> {
            self.calls
                .lock()
                .map_err(|_| anyhow!("poison"))?
                .push("applying".into());
            Ok(if self.replay_terminal_conflict {
                LedgerResult::Stale
            } else {
                LedgerResult::Applied
            })
        }
        async fn finalize_conflict(
            &self,
            _: &TaskDeliveryIdentity,
            _: &str,
            r: &str,
        ) -> Result<LedgerResult> {
            self.calls
                .lock()
                .map_err(|_| anyhow!("poison"))?
                .push(format!("conflict:{r}"));
            Ok(if self.replay_terminal_conflict {
                LedgerResult::Replayed
            } else {
                LedgerResult::Applied
            })
        }
        async fn integrate(&self, i: TaskIntegrated) -> Result<LedgerResult> {
            self.calls
                .lock()
                .map_err(|_| anyhow!("poison"))?
                .push(format!("integrate:{}", i.identity.delivery_generation));
            Ok(LedgerResult::Applied)
        }
        async fn is_mapped_first_parent(&self, _: &ActiveAttempt, sha: &str) -> Result<bool> {
            Ok(sha == "base" || self.mapped)
        }
        async fn rework(
            &self,
            r: ReworkDelivery,
            _: &DeliverySource,
            _: &Candidate,
        ) -> Result<LedgerResult> {
            self.calls
                .lock()
                .map_err(|_| anyhow!("poison"))?
                .push(format!(
                    "rework:{}:{}",
                    r.expected_generation, r.delivery_generation
                ));
            Ok(self.rework_result)
        }
        async fn retry_from_mapped_head(
            &self,
            retry: MappedHeadRetryDelivery,
            _: &DeliverySource,
            _: &Candidate,
        ) -> Result<LedgerResult> {
            self.calls
                .lock()
                .map_err(|_| anyhow!("poison"))?
                .push(format!(
                    "mapped-retry:{}:{}",
                    retry.expected_generation, retry.delivery_generation
                ));
            Ok(LedgerResult::Applied)
        }
        async fn park(
            &self,
            _: &str,
            _: &TaskDeliveryIdentity,
            r: ParkReason,
            _: &str,
        ) -> Result<()> {
            self.calls
                .lock()
                .map_err(|_| anyhow!("poison"))?
                .push(format!("park:{r:?}"));
            Ok(())
        }
    }
    struct Builder {
        conflict: bool,
    }
    #[async_trait]
    impl CandidateBuilder for Builder {
        async fn build(
            &self,
            _: &TaskDeliveryIdentity,
            _: &DeliverySource,
            p: &str,
        ) -> Result<CandidateBuild> {
            if self.conflict {
                Ok(CandidateBuild::Conflict {
                    patch_digest: "digest".into(),
                    reason: "content conflict".into(),
                })
            } else {
                Ok(CandidateBuild::Clean(Candidate {
                    candidate_sha: format!("commit-{p}"),
                    patch_digest: "digest".into(),
                    selected_parent_sha: p.into(),
                }))
            }
        }
    }
    struct MappedConflictBuilder;
    #[async_trait]
    impl CandidateBuilder for MappedConflictBuilder {
        async fn build(
            &self,
            _: &TaskDeliveryIdentity,
            _: &DeliverySource,
            parent: &str,
        ) -> Result<CandidateBuild> {
            if parent == "mapped" {
                return Ok(CandidateBuild::Conflict {
                    patch_digest: "digest".into(),
                    reason: "mapped conflict".into(),
                });
            }
            Ok(CandidateBuild::Clean(Candidate {
                candidate_sha: format!("commit-{parent}"),
                patch_digest: "digest".into(),
                selected_parent_sha: parent.into(),
            }))
        }
    }
    struct Remote {
        update: Arc<Mutex<Vec<RemoteUpdate>>>,
        observed: Mutex<Vec<Option<String>>>,
        calls: Option<Arc<Mutex<Vec<String>>>>,
    }
    #[async_trait]
    impl AttemptRef for Remote {
        async fn observe(&self, _: &str) -> Result<Option<String>> {
            self.observed
                .lock()
                .map_err(|_| anyhow!("poison"))?
                .pop()
                .ok_or_else(|| anyhow!("missing observation"))
        }
        async fn update_expected_old(
            &self,
            _: &str,
            expected: &str,
            new: &str,
        ) -> Result<RemoteUpdate> {
            if let Some(calls) = &self.calls {
                calls
                    .lock()
                    .map_err(|_| anyhow!("poison"))?
                    .push(format!("update:{expected}:{new}"));
            }
            self.update
                .lock()
                .map_err(|_| anyhow!("poison"))?
                .pop()
                .ok_or_else(|| anyhow!("missing update"))
        }
    }
    fn source(generation: i64) -> DeliverySource {
        DeliverySource {
            task_id: "task".into(),
            delivery_generation: generation,
            transition_id: format!("transition-{generation}"),
            source_sha: format!("source-{generation}"),
            normalized_patch: "patch".into(),
        }
    }
    fn ledger(calls: Arc<Mutex<Vec<String>>>) -> Ledger {
        Ledger {
            calls,
            mapped: false,
            replay_terminal_conflict: false,
            rework_result: LedgerResult::Applied,
        }
    }
    fn remote(
        updates: Vec<RemoteUpdate>,
        observations: Vec<Option<&str>>,
    ) -> (Remote, Arc<Mutex<Vec<RemoteUpdate>>>) {
        let updates = Arc::new(Mutex::new(updates));
        (
            Remote {
                update: updates.clone(),
                calls: None,
                observed: Mutex::new(
                    observations
                        .into_iter()
                        .map(|sha| sha.map(str::to_owned))
                        .collect(),
                ),
            },
            updates,
        )
    }
    #[test]
    fn mapped_head_successor_resumes_its_immutable_identity() {
        let successor = TaskDelivery {
            identity: TaskDeliveryIdentity::new("attempt", "task", 2).unwrap(),
            state: djinn_core::models::TaskDeliveryState::Applying,
            candidate_sha: "candidate-task-g2-on-mapped".into(),
            source_sha: "original-source".into(),
            patch_digest: "original-digest".into(),
            selected_parent_sha: "mapped".into(),
            prepare_transition_id: "transition-1:mapped-head:2".into(),
            base_sha: "mapped".into(),
            applied_at: None,
            conflict_reason: None,
            supersede_transition_id: None,
            created_at: "now".into(),
        };
        let resumed = resume_delivery_source(
            "task",
            "newer-checkout-source".into(),
            "newer-checkout-patch".into(),
            Some(&successor),
        );
        assert_eq!(resumed.delivery_generation, 2);
        assert_eq!(resumed.transition_id, "transition-1:mapped-head:2");
        assert_eq!(resumed.source_sha, "original-source");
        // The immutable candidate remains in the ledger and is recovered by
        // `prepared_candidate` before this patch is ever rebuilt.
        assert_eq!(successor.candidate_sha, "candidate-task-g2-on-mapped");
    }
    #[tokio::test]
    async fn explicit_legacy_completion_preserves_existing_persisted_pr_identity() {
        use crate::dispatch::wave_dispatch::{
            route_approved_completion, run_legacy_completion_preserving_pr_identity,
        };
        use djinn_core::events::EventBus;
        use djinn_db::{EpicRepository, TaskRepository};

        let db = Database::open_in_memory().unwrap();
        let events = EventBus::noop();
        let epic = EpicRepository::new(db.clone(), events.clone())
            .create("Legacy delivery", "", "", "", "", None)
            .await
            .unwrap();
        let repo = TaskRepository::new(db.clone(), events);
        let task = repo
            .create(
                &epic.id,
                "Approved legacy task",
                "",
                "",
                "task",
                0,
                "worker",
                Some("approved"),
            )
            .await
            .unwrap();
        let existing_pr = "https://github.example/owner/repo/pull/42";
        repo.set_pr_url(&task.id, existing_pr).await.unwrap();
        // Completion receives the same persisted/reloaded shape as production.
        let task = repo.get(&task.id).await.unwrap().unwrap();
        djinn_db::test_support::activate_direct_delivery_epoch_for_test(&db).await;

        clear_boundary_operations();
        let admission = admit_direct_delivery(db.clone(), &task.id).await.unwrap();
        assert_eq!(admission, DirectDeliveryAdmission::Legacy);

        let external_pr_seen = Arc::new(Mutex::new(None));
        let direct_append_calls = Arc::new(Mutex::new(0usize));
        let external_pr_for_completion = external_pr_seen.clone();
        let task_for_completion = task.clone();
        let direct_append_for_completion = direct_append_calls.clone();
        // The exact value returned by the real admission service is consumed by
        // the production completion seam. The direct collaborator is a panic-free
        // counter only so mutually exclusive closures can share observation state.
        let outcome = route_approved_completion(
            admission,
            || async move {
                *direct_append_for_completion.lock().unwrap() += 1;
                djinn_runtime::TaskRunOutcome::Failed {
                    stage: "unexpected-direct-append".to_owned(),
                    provider_failure: None,
                    reason: "explicit legacy admission selected direct append".to_owned(),
                    error_class: None,
                    hint: None,
                    body_excerpt: None,
                }
            },
            || async move {
                run_legacy_completion_preserving_pr_identity(
                    &db,
                    &task_for_completion,
                    || async move {
                        *external_pr_for_completion.lock().unwrap() = Some(existing_pr.to_owned());
                        djinn_runtime::TaskRunOutcome::PrOpened {
                            url: existing_pr.to_owned(),
                            sha: "legacy-head".to_owned(),
                        }
                    },
                )
                .await
                .unwrap()
            },
        )
        .await;

        assert_eq!(
            take_boundary_operations(),
            [
                BoundaryOperation::CapabilityProbe,
                BoundaryOperation::SupervisorPrOpen
            ],
            "only the real legacy completion collaborator may run for an explicit legacy identity"
        );
        assert_eq!(*direct_append_calls.lock().unwrap(), 0);
        assert!(matches!(
            outcome,
            djinn_runtime::TaskRunOutcome::PrOpened { .. }
        ));
        assert_eq!(
            external_pr_seen.lock().unwrap().as_deref(),
            Some(existing_pr)
        );
        assert_eq!(
            repo.get(&task.id).await.unwrap().unwrap().pr_url.as_deref(),
            Some(existing_pr),
            "legacy completion must leave the task's persisted PR identity unchanged"
        );
    }
    #[tokio::test]
    async fn fresh_legacy_completion_may_persist_its_first_pr_identity() {
        use crate::dispatch::wave_dispatch::run_legacy_completion_preserving_pr_identity;
        use djinn_core::events::EventBus;
        use djinn_db::{EpicRepository, TaskRepository};

        let db = Database::open_in_memory().unwrap();
        let events = EventBus::noop();
        let epic = EpicRepository::new(db.clone(), events.clone())
            .create("Fresh legacy delivery", "", "", "", "", None)
            .await
            .unwrap();
        let repo = TaskRepository::new(db.clone(), events);
        let task = repo
            .create(
                &epic.id,
                "Approved fresh legacy task",
                "",
                "",
                "task",
                0,
                "worker",
                Some("approved"),
            )
            .await
            .unwrap();
        assert!(task.pr_url.is_none());
        let first_pr = "https://github.example/owner/repo/pull/43";

        let db_for_completion = db.clone();
        let task_id = task.id.clone();
        let outcome = run_legacy_completion_preserving_pr_identity(&db, &task, || async move {
            TaskRepository::new(db_for_completion, EventBus::noop())
                .set_pr_url(&task_id, first_pr)
                .await
                .unwrap();
            djinn_runtime::TaskRunOutcome::PrOpened {
                url: first_pr.to_owned(),
                sha: "legacy-head".to_owned(),
            }
        })
        .await
        .unwrap();

        assert!(matches!(
            outcome,
            djinn_runtime::TaskRunOutcome::PrOpened { .. }
        ));
        assert_eq!(
            repo.get(&task.id).await.unwrap().unwrap().pr_url.as_deref(),
            Some(first_pr),
            "fresh legacy completion must be allowed to persist its first PR identity"
        );
    }
    #[tokio::test]
    async fn conflict_is_finalized_before_parking() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (remote, _) = remote(vec![], vec![Some("base")]);
        let engine =
            DirectDeliveryEngine::new(ledger(calls.clone()), remote, Builder { conflict: true });
        assert!(matches!(
            engine.deliver(source(1)).await.unwrap(),
            DeliveryOutcome::ConflictParked { .. }
        ));
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "prepare:conflict:digest",
                "applying",
                "conflict:content conflict",
                "park:TaskAppendConflict"
            ]
        );
    }
    #[tokio::test]
    async fn finalized_conflict_replay_still_parks_attempt() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (remote, _) = remote(vec![], vec![Some("base")]);
        let mut state = ledger(calls.clone());
        state.replay_terminal_conflict = true;
        let engine = DirectDeliveryEngine::new(state, remote, Builder { conflict: true });
        assert!(matches!(
            engine.deliver(source(1)).await.unwrap(),
            DeliveryOutcome::ConflictParked { .. }
        ));
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "prepare:conflict:digest",
                "applying",
                "conflict:content conflict",
                "park:TaskAppendConflict"
            ]
        );
    }
    #[tokio::test]
    async fn mapped_observed_head_rebuilds_then_integrates() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (remote, _) = remote(
            vec![RemoteUpdate::Updated {
                sha: "commit-mapped".into(),
            }],
            vec![Some("commit-mapped"), Some("mapped")],
        );
        let mut state = ledger(calls.clone());
        state.mapped = true;
        let engine = DirectDeliveryEngine::new(state, remote, Builder { conflict: false });
        assert!(
            matches!(engine.deliver(source(1)).await.unwrap(), DeliveryOutcome::Integrated { candidate_sha } if candidate_sha == "commit-mapped")
        );
        assert_eq!(
            *calls.lock().unwrap(),
            ["prepare:commit-mapped", "applying", "integrate:1"]
        );
    }
    #[tokio::test]
    async fn corrected_generation_reworks_then_integrates() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (remote, _) = remote(
            vec![RemoteUpdate::Updated {
                sha: "commit-base".into(),
            }],
            vec![Some("commit-base"), Some("base")],
        );
        let engine =
            DirectDeliveryEngine::new(ledger(calls.clone()), remote, Builder { conflict: false });
        assert!(matches!(
            engine.deliver(source(2)).await.unwrap(),
            DeliveryOutcome::Integrated { .. }
        ));
        assert_eq!(
            *calls.lock().unwrap(),
            ["rework:1:2", "applying", "integrate:2"]
        );
    }
    #[tokio::test]
    async fn stale_rework_never_updates_the_remote_ref() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (remote, updates) = remote(
            vec![RemoteUpdate::Updated {
                sha: "commit-base".into(),
            }],
            vec![Some("base")],
        );
        let mut state = ledger(calls.clone());
        state.rework_result = LedgerResult::Stale;
        let engine = DirectDeliveryEngine::new(state, remote, Builder { conflict: false });
        assert!(matches!(
            engine.deliver(source(2)).await.unwrap(),
            DeliveryOutcome::RetryBoundParked { .. }
        ));
        assert_eq!(*calls.lock().unwrap(), ["rework:1:2"]);
        assert_eq!(updates.lock().unwrap().len(), 1);
    }
    #[tokio::test]
    async fn post_prepare_mapped_stale_retries_new_generation_then_integrates() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        // `Remote` pops, so the stale first update precedes the successful retry.
        let (mut remote, updates) = remote(
            vec![
                RemoteUpdate::Updated {
                    sha: "commit-mapped".into(),
                },
                RemoteUpdate::Stale {
                    observed_sha: Some("mapped".into()),
                },
            ],
            vec![Some("commit-mapped"), Some("base")],
        );
        remote.calls = Some(calls.clone());
        let mut state = ledger(calls.clone());
        state.mapped = true;
        let engine = DirectDeliveryEngine::new(state, remote, Builder { conflict: false });
        assert!(matches!(
            engine.deliver(source(1)).await.unwrap(),
            DeliveryOutcome::Integrated { candidate_sha } if candidate_sha == "commit-mapped"
        ));
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "prepare:commit-base",
                "applying",
                "update:base:commit-base",
                "mapped-retry:1:2",
                "applying",
                "update:mapped:commit-mapped",
                "integrate:2"
            ]
        );
        assert!(updates.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn exact_candidate_stale_integrates_without_another_ref_update() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (remote, updates) = remote(
            vec![RemoteUpdate::Stale {
                observed_sha: Some("commit-base".into()),
            }],
            vec![Some("base")],
        );
        let engine =
            DirectDeliveryEngine::new(ledger(calls.clone()), remote, Builder { conflict: false });
        assert!(matches!(
            engine.deliver(source(1)).await.unwrap(),
            DeliveryOutcome::Integrated { candidate_sha } if candidate_sha == "commit-base"
        ));
        assert_eq!(
            *calls.lock().unwrap(),
            ["prepare:commit-base", "applying", "integrate:1"]
        );
        assert!(updates.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn duplicate_mapped_heads_retry_the_prepared_candidate_without_a_new_generation() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (mut remote, updates) = remote(
            vec![
                RemoteUpdate::Updated {
                    sha: "commit-mapped".into(),
                },
                RemoteUpdate::Stale {
                    observed_sha: Some("mapped".into()),
                },
                RemoteUpdate::Stale {
                    observed_sha: Some("mapped".into()),
                },
            ],
            vec![Some("commit-mapped"), Some("base")],
        );
        remote.calls = Some(calls.clone());
        let mut state = ledger(calls.clone());
        state.mapped = true;
        let engine = DirectDeliveryEngine::new(state, remote, Builder { conflict: false });
        assert!(matches!(
            engine.deliver(source(1)).await.unwrap(),
            DeliveryOutcome::Integrated { candidate_sha } if candidate_sha == "commit-mapped"
        ));
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "prepare:commit-base",
                "applying",
                "update:base:commit-base",
                "mapped-retry:1:2",
                "applying",
                "update:mapped:commit-mapped",
                "update:mapped:commit-mapped",
                "integrate:2"
            ]
        );
        assert!(updates.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn third_distinct_mapped_head_parks_at_retry_bound() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (mut remote, updates) = remote(
            vec![
                RemoteUpdate::Stale {
                    observed_sha: Some("h3".into()),
                },
                RemoteUpdate::Stale {
                    observed_sha: Some("h2".into()),
                },
                RemoteUpdate::Stale {
                    observed_sha: Some("h1".into()),
                },
            ],
            vec![Some("base")],
        );
        remote.calls = Some(calls.clone());
        let mut state = ledger(calls.clone());
        state.mapped = true;
        let engine = DirectDeliveryEngine::new(state, remote, Builder { conflict: false });
        assert_eq!(
            engine.deliver(source(1)).await.unwrap(),
            DeliveryOutcome::RetryBoundParked { observed_heads: 3 }
        );
        let calls = calls.lock().unwrap();
        assert_eq!(calls.last().unwrap(), "park:StaleHeadRetryBound");
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.starts_with("update:"))
                .count(),
            3
        );
        assert!(updates.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn mapped_retry_conflict_is_finalized_before_parking() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (remote, updates) = remote(
            vec![RemoteUpdate::Stale {
                observed_sha: Some("mapped".into()),
            }],
            vec![Some("base")],
        );
        let mut state = ledger(calls.clone());
        state.mapped = true;
        let engine = DirectDeliveryEngine::new(state, remote, MappedConflictBuilder);
        assert!(matches!(
            engine.deliver(source(1)).await.unwrap(),
            DeliveryOutcome::ConflictParked { reason } if reason == "mapped conflict"
        ));
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "prepare:commit-base",
                "applying",
                "mapped-retry:1:2",
                "applying",
                "conflict:mapped conflict",
                "park:TaskAppendConflict"
            ]
        );
        assert!(updates.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn arbitrary_unmapped_head_parks_as_unexpected() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (remote, updates) = remote(vec![], vec![Some("foreign")]);
        let engine =
            DirectDeliveryEngine::new(ledger(calls.clone()), remote, Builder { conflict: false });
        assert!(matches!(
            engine.deliver(source(1)).await.unwrap(),
            DeliveryOutcome::UnexpectedHeadParked { .. }
        ));
        assert_eq!(
            calls.lock().unwrap().last().unwrap(),
            "park:UnexpectedBranchHead"
        );
        assert!(updates.lock().unwrap().is_empty());
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ConcurrentGenerationState {
        Prepared,
        Applying,
        Superseded,
        Applied,
    }
    /// Deterministic process-loss points. Shared state remains durable while
    /// the caller receives an error and must replay its immutable generation.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum CrashWindow {
        BeforeRemoteMutation,
        AfterRemoteBeforeSqlAcknowledgment,
        AfterSqlFinalization,
    }
    #[derive(Clone)]
    struct ConcurrentGeneration {
        identity: TaskDeliveryIdentity,
        source: DeliverySource,
        candidate: Candidate,
        state: ConcurrentGenerationState,
        superseded_by: Option<i64>,
        supersede_transition: Option<String>,
        applying_transition: Option<String>,
    }
    #[derive(Default)]
    struct ConcurrentState {
        head: String,
        durable_head: String,
        attempt_branch_head_sha: String,
        parents: std::collections::HashMap<String, String>,
        /// Every expected-old CAS invocation, including a pre-mutation crash.
        updates: Vec<(String, String)>,
        /// Candidate commits actually published to the shared remote graph.
        published_commits: Vec<String>,
        generations: std::collections::HashMap<(String, i64), ConcurrentGeneration>,
        integrated: std::collections::HashMap<String, String>,
        task_status: std::collections::HashMap<String, String>,
        task_merge_commit_sha: std::collections::HashMap<String, String>,
        closure_calls: std::collections::HashMap<String, usize>,
        dependent_release_calls: std::collections::HashMap<String, usize>,
        integration_attempts: usize,
        crash_window: Option<CrashWindow>,
        crash_injected: bool,
        /// Simulates another mapped append winning generation one's CAS.
        inject_mapped_head_on_base_cas: bool,
        /// Simulates a provider response lost after a successful successor CAS.
        provider_error_after_success_on_successor: bool,
    }
    #[derive(Default)]
    struct ConcurrentIntegrationGate {
        enabled: bool,
        block_predecessor_once: AtomicBool,
        successor_stale_attempts: AtomicUsize,
        release_predecessor: tokio::sync::Notify,
    }
    #[derive(Clone)]
    struct ConcurrentLedger {
        state: Arc<Mutex<ConcurrentState>>,
        gate: Arc<ConcurrentIntegrationGate>,
    }
    #[async_trait]
    impl DeliveryLedger for ConcurrentLedger {
        async fn direct_delivery_enabled(&self) -> Result<bool> {
            Ok(true)
        }
        async fn resolve_active_attempt(&self, _: &str) -> Result<ActiveAttempt> {
            Ok(ActiveAttempt {
                build_attempt_id: "attempt".into(),
                branch_name: "proposal/p/a".into(),
                branch_head_sha: "base".into(),
            })
        }
        async fn prepared_candidate(
            &self,
            identity: &TaskDeliveryIdentity,
        ) -> Result<Option<Candidate>> {
            Ok(self
                .state
                .lock()
                .map_err(|_| anyhow!("poison"))?
                .generations
                .get(&(identity.task_id.clone(), identity.delivery_generation))
                .map(|generation| generation.candidate.clone()))
        }
        async fn prepare(
            &self,
            identity: &TaskDeliveryIdentity,
            source: &DeliverySource,
            candidate: &Candidate,
        ) -> Result<LedgerResult> {
            let mut s = self.state.lock().map_err(|_| anyhow!("poison"))?;
            let key = (identity.task_id.clone(), identity.delivery_generation);
            if let Some(existing) = s.generations.get(&key) {
                return Ok(
                    if existing.candidate == *candidate && existing.source == *source {
                        LedgerResult::Replayed
                    } else {
                        LedgerResult::Stale
                    },
                );
            }
            s.generations.insert(
                key,
                ConcurrentGeneration {
                    identity: identity.clone(),
                    source: source.clone(),
                    candidate: candidate.clone(),
                    state: ConcurrentGenerationState::Prepared,
                    superseded_by: None,
                    supersede_transition: None,
                    applying_transition: None,
                },
            );
            Ok(LedgerResult::Applied)
        }
        async fn retry_from_mapped_head(
            &self,
            retry: MappedHeadRetryDelivery,
            source: &DeliverySource,
            candidate: &Candidate,
        ) -> Result<LedgerResult> {
            let mut s = self.state.lock().map_err(|_| anyhow!("poison"))?;
            let old_key = (retry.task_id.clone(), retry.expected_generation);
            let new_key = (retry.task_id.clone(), retry.delivery_generation);
            if source.task_id != retry.task_id
                || source.delivery_generation != retry.delivery_generation
                || source.transition_id != retry.transition_id
                || candidate.selected_parent_sha == "base"
                || !s.parents.contains_key(&candidate.selected_parent_sha)
            {
                return Ok(LedgerResult::Stale);
            }
            if s.generations.contains_key(&new_key) {
                return Ok(LedgerResult::Stale);
            }
            let Some(old) = s.generations.get_mut(&old_key) else {
                return Ok(LedgerResult::Stale);
            };
            if old.identity.build_attempt_id != retry.build_attempt_id
                || old.identity.task_id != retry.task_id
                || old.identity.delivery_generation != retry.expected_generation
                || old.state != ConcurrentGenerationState::Applying
                || old.source.source_sha != source.source_sha
                || old.source.normalized_patch != source.normalized_patch
                || old.candidate.patch_digest != candidate.patch_digest
            {
                return Ok(LedgerResult::Stale);
            }
            old.state = ConcurrentGenerationState::Superseded;
            old.superseded_by = Some(retry.delivery_generation);
            old.supersede_transition = Some(retry.transition_id.clone());
            s.generations.insert(
                new_key,
                ConcurrentGeneration {
                    identity: TaskDeliveryIdentity::new(
                        &retry.build_attempt_id,
                        &retry.task_id,
                        retry.delivery_generation,
                    )?,
                    source: source.clone(),
                    candidate: candidate.clone(),
                    state: ConcurrentGenerationState::Prepared,
                    superseded_by: None,
                    supersede_transition: None,
                    applying_transition: None,
                },
            );
            Ok(LedgerResult::Applied)
        }
        async fn begin_apply(
            &self,
            identity: &TaskDeliveryIdentity,
            transition: &str,
        ) -> Result<LedgerResult> {
            let mut s = self.state.lock().map_err(|_| anyhow!("poison"))?;
            let Some(generation) = s
                .generations
                .get_mut(&(identity.task_id.clone(), identity.delivery_generation))
            else {
                return Ok(LedgerResult::Stale);
            };
            if generation.identity != *identity || generation.source.transition_id != transition {
                return Ok(LedgerResult::Stale);
            }
            match generation.state {
                ConcurrentGenerationState::Prepared => {
                    generation.state = ConcurrentGenerationState::Applying;
                    generation.applying_transition = Some(transition.into());
                    Ok(LedgerResult::Applied)
                }
                ConcurrentGenerationState::Applying => Ok(LedgerResult::Replayed),
                _ => Ok(LedgerResult::Stale),
            }
        }
        async fn finalize_conflict(
            &self,
            _: &TaskDeliveryIdentity,
            _: &str,
            _: &str,
        ) -> Result<LedgerResult> {
            Ok(LedgerResult::Applied)
        }
        async fn integrate(&self, i: TaskIntegrated) -> Result<LedgerResult> {
            let key = (i.identity.task_id.clone(), i.identity.delivery_generation);
            // Hold the predecessor transaction until the successor has observed
            // four durable-head misses. This makes the post-CAS ordering
            // deterministic and proves the engine is not relying on three
            // scheduler yields.
            let block_predecessor = self.gate.enabled && {
                let s = self.state.lock().map_err(|_| anyhow!("poison"))?;
                s.generations.get(&key).is_some_and(|generation| {
                    generation.candidate.selected_parent_sha == "base"
                        && generation.candidate.candidate_sha == i.candidate_sha
                })
            };
            if block_predecessor
                && !self
                    .gate
                    .block_predecessor_once
                    .swap(true, Ordering::SeqCst)
            {
                self.gate.release_predecessor.notified().await;
            }

            let mut s = self.state.lock().map_err(|_| anyhow!("poison"))?;
            s.integration_attempts += 1;
            if let Some(sha) = s.integrated.get(&i.identity.task_id) {
                return Ok(if sha == &i.candidate_sha {
                    LedgerResult::Replayed
                } else {
                    LedgerResult::Stale
                });
            }
            let Some(generation) = s.generations.get(&key).cloned() else {
                return Ok(LedgerResult::Stale);
            };
            if generation.identity != i.identity
                || generation.state != ConcurrentGenerationState::Applying
                || generation.candidate.candidate_sha != i.candidate_sha
                || i.candidate_sha != i.observed_applied_candidate_sha
                || i.candidate_sha != i.merge_commit_sha
            {
                return Ok(LedgerResult::Stale);
            }
            if s.durable_head != generation.candidate.selected_parent_sha {
                if generation.candidate.selected_parent_sha != "base"
                    && self
                        .gate
                        .successor_stale_attempts
                        .fetch_add(1, Ordering::SeqCst)
                        + 1
                        == 4
                {
                    self.gate.release_predecessor.notify_one();
                }
                return Ok(LedgerResult::Stale);
            }
            s.durable_head = i.candidate_sha.clone();
            s.attempt_branch_head_sha = i.candidate_sha.clone();
            s.generations.get_mut(&key).unwrap().state = ConcurrentGenerationState::Applied;
            s.integrated
                .insert(i.identity.task_id.clone(), i.candidate_sha.clone());
            s.task_status
                .insert(i.identity.task_id.clone(), "closed".into());
            s.task_merge_commit_sha
                .insert(i.identity.task_id.clone(), i.merge_commit_sha.clone());
            *s.closure_calls
                .entry(i.identity.task_id.clone())
                .or_default() += 1;
            *s.dependent_release_calls
                .entry(i.identity.task_id)
                .or_default() += 1;
            if s.crash_window == Some(CrashWindow::AfterSqlFinalization) && !s.crash_injected {
                s.crash_injected = true;
                return Err(anyhow!("injected crash after SQL finalization"));
            }
            Ok(LedgerResult::Applied)
        }
        async fn is_mapped_first_parent(&self, _: &ActiveAttempt, sha: &str) -> Result<bool> {
            let s = self.state.lock().map_err(|_| anyhow!("poison"))?;
            Ok(sha == "base" || s.parents.contains_key(sha))
        }
        async fn rework(
            &self,
            r: ReworkDelivery,
            source: &DeliverySource,
            candidate: &Candidate,
        ) -> Result<LedgerResult> {
            self.prepare(
                &TaskDeliveryIdentity::new(&r.build_attempt_id, &r.task_id, r.delivery_generation)?,
                source,
                candidate,
            )
            .await
        }
        async fn park(
            &self,
            _: &str,
            _: &TaskDeliveryIdentity,
            _: ParkReason,
            _: &str,
        ) -> Result<()> {
            Ok(())
        }
    }
    #[derive(Clone)]
    struct ConcurrentRemote {
        state: Arc<Mutex<ConcurrentState>>,
        first_cas: Option<Arc<tokio::sync::Notify>>,
        second_cas: Option<Arc<tokio::sync::Notify>>,
    }
    #[async_trait]
    impl AttemptRef for ConcurrentRemote {
        async fn observe(&self, _: &str) -> Result<Option<String>> {
            Ok(Some(
                self.state
                    .lock()
                    .map_err(|_| anyhow!("poison"))?
                    .head
                    .clone(),
            ))
        }
        async fn update_expected_old(&self, _: &str, old: &str, new: &str) -> Result<RemoteUpdate> {
            let first = old == "base";
            let crash_after_remote;
            let provider_error_after_remote;
            {
                let mut s = self.state.lock().map_err(|_| anyhow!("poison"))?;
                s.updates.push((old.into(), new.into()));
                if s.crash_window == Some(CrashWindow::BeforeRemoteMutation) && !s.crash_injected {
                    s.crash_injected = true;
                    return Err(anyhow!("injected crash before remote mutation"));
                }
                if first && s.inject_mapped_head_on_base_cas {
                    s.inject_mapped_head_on_base_cas = false;
                    s.parents.insert("mapped".into(), "base".into());
                    s.head = "mapped".into();
                    // The competing mapped append is already durably
                    // reconciled before this task builds its successor.
                    s.durable_head = "mapped".into();
                    s.attempt_branch_head_sha = "mapped".into();
                    return Ok(RemoteUpdate::Stale {
                        observed_sha: Some("mapped".into()),
                    });
                }
                if s.head != old {
                    return Ok(RemoteUpdate::Stale {
                        observed_sha: Some(s.head.clone()),
                    });
                }
                s.parents.insert(new.into(), old.into());
                s.head = new.into();
                s.published_commits.push(new.into());
                crash_after_remote = s.crash_window
                    == Some(CrashWindow::AfterRemoteBeforeSqlAcknowledgment)
                    && !s.crash_injected;
                if crash_after_remote {
                    s.crash_injected = true;
                }
                provider_error_after_remote = !first && s.provider_error_after_success_on_successor;
                if provider_error_after_remote {
                    s.provider_error_after_success_on_successor = false;
                }
            }
            if crash_after_remote {
                return Err(anyhow!(
                    "injected crash after remote mutation before SQL acknowledgment"
                ));
            }
            if provider_error_after_remote {
                return Err(anyhow!(
                    "injected provider error after successful remote mutation"
                ));
            }
            if first {
                if let (Some(first_cas), Some(second_cas)) = (&self.first_cas, &self.second_cas) {
                    first_cas.notify_one();
                    second_cas.notified().await;
                }
            } else if let Some(second_cas) = &self.second_cas {
                second_cas.notify_one();
            }
            Ok(RemoteUpdate::Updated { sha: new.into() })
        }
    }
    #[derive(Clone)]
    struct ConcurrentBuilder(Arc<tokio::sync::Barrier>);
    #[async_trait]
    impl CandidateBuilder for ConcurrentBuilder {
        async fn build(
            &self,
            i: &TaskDeliveryIdentity,
            _: &DeliverySource,
            parent: &str,
        ) -> Result<CandidateBuild> {
            if i.delivery_generation == 1 {
                self.0.wait().await;
            }
            Ok(CandidateBuild::Clean(Candidate {
                candidate_sha: format!(
                    "candidate-{}-g{}-on-{parent}",
                    i.task_id, i.delivery_generation
                ),
                patch_digest: "digest".into(),
                selected_parent_sha: parent.into(),
            }))
        }
    }
    struct CrashBuilder;
    #[async_trait]
    impl CandidateBuilder for CrashBuilder {
        async fn build(
            &self,
            identity: &TaskDeliveryIdentity,
            _: &DeliverySource,
            parent: &str,
        ) -> Result<CandidateBuild> {
            Ok(CandidateBuild::Clean(Candidate {
                candidate_sha: format!(
                    "candidate-{}-g{}-on-{parent}",
                    identity.task_id, identity.delivery_generation
                ),
                patch_digest: "crash-matrix-digest".into(),
                selected_parent_sha: parent.into(),
            }))
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_clean_completions_share_cas_graph_and_rebuild_loser() {
        let state = Arc::new(Mutex::new(ConcurrentState {
            head: "base".into(),
            durable_head: "base".into(),
            ..Default::default()
        }));
        let gate = Arc::new(ConcurrentIntegrationGate {
            enabled: true,
            ..Default::default()
        });
        let ledger = ConcurrentLedger {
            state: state.clone(),
            gate: gate.clone(),
        };
        let remote = ConcurrentRemote {
            state: state.clone(),
            first_cas: Some(Arc::new(tokio::sync::Notify::new())),
            second_cas: Some(Arc::new(tokio::sync::Notify::new())),
        };
        let builder = ConcurrentBuilder(Arc::new(tokio::sync::Barrier::new(2)));
        let a = DirectDeliveryEngine::new(ledger.clone(), remote.clone(), builder.clone());
        let b = DirectDeliveryEngine::new(ledger, remote, builder);
        let (a, b) = tokio::join!(
            a.deliver(DeliverySource {
                task_id: "a".into(),
                delivery_generation: 1,
                transition_id: "a".into(),
                source_sha: "a".into(),
                normalized_patch: "a".into()
            }),
            b.deliver(DeliverySource {
                task_id: "b".into(),
                delivery_generation: 1,
                transition_id: "b".into(),
                source_sha: "b".into(),
                normalized_patch: "b".into()
            })
        );
        assert!(matches!(a.unwrap(), DeliveryOutcome::Integrated { .. }));
        assert!(matches!(b.unwrap(), DeliveryOutcome::Integrated { .. }));
        let s = state.lock().unwrap();
        assert_eq!(s.integrated.len(), 2);
        assert_eq!(s.updates.len(), 3);
        assert!(s.updates.iter().all(|(old, new)| old != new));
        assert_eq!(s.parents.len(), 2);
        assert_eq!(s.durable_head, s.head);
        assert_eq!(s.generations.len(), 3);
        // More than two attempts proves the successor tried to integrate before
        // the winner had advanced the durable parent head and then replayed.
        assert!(s.integration_attempts >= 6);
        assert_eq!(gate.successor_stale_attempts.load(Ordering::SeqCst), 4);
        assert!(s.integrated.values().all(|sha| s.parents.contains_key(sha)));
        assert!(s.closure_calls.values().all(|calls| *calls == 1));
        let superseded = s
            .generations
            .values()
            .find(|g| g.state == ConcurrentGenerationState::Superseded)
            .unwrap();
        let retry = s
            .generations
            .get(&(
                superseded.identity.task_id.clone(),
                superseded.superseded_by.unwrap(),
            ))
            .unwrap();
        assert_eq!(retry.state, ConcurrentGenerationState::Applied);
        assert_eq!(
            superseded.superseded_by,
            Some(retry.identity.delivery_generation)
        );
        assert_eq!(
            superseded.supersede_transition,
            Some(retry.source.transition_id.clone())
        );
        assert_eq!(
            retry.applying_transition,
            Some(retry.source.transition_id.clone())
        );
        let winner = s
            .generations
            .values()
            .find(|generation| {
                generation.state == ConcurrentGenerationState::Applied
                    && generation.identity.task_id != retry.identity.task_id
            })
            .unwrap();
        assert_eq!(
            retry.candidate.selected_parent_sha,
            winner.candidate.candidate_sha
        );
        assert_eq!(
            s.parents[&retry.candidate.candidate_sha],
            retry.candidate.selected_parent_sha
        );
    }

    #[tokio::test]
    async fn crash_window_replays_converge_the_shared_remote_and_durable_ledger() {
        for window in [
            CrashWindow::BeforeRemoteMutation,
            CrashWindow::AfterRemoteBeforeSqlAcknowledgment,
            CrashWindow::AfterSqlFinalization,
        ] {
            let state = Arc::new(Mutex::new(ConcurrentState {
                head: "base".into(),
                durable_head: "base".into(),
                attempt_branch_head_sha: "base".into(),
                crash_window: Some(window),
                ..Default::default()
            }));
            let ledger = ConcurrentLedger {
                state: state.clone(),
                gate: Arc::new(ConcurrentIntegrationGate::default()),
            };
            let remote = ConcurrentRemote {
                state: state.clone(),
                first_cas: None,
                second_cas: None,
            };
            let engine = DirectDeliveryEngine::new(ledger, remote, CrashBuilder);
            let immutable_source = source(1);
            assert!(
                engine.deliver(immutable_source.clone()).await.is_err(),
                "{window:?}"
            );
            if window == CrashWindow::BeforeRemoteMutation {
                let s = state.lock().unwrap();
                let prepared = s.generations.get(&("task".into(), 1)).unwrap();
                assert_eq!(s.head, "base");
                assert!(s.published_commits.is_empty());
                assert_eq!(prepared.state, ConcurrentGenerationState::Applying);
                assert_eq!(
                    prepared.candidate.candidate_sha,
                    "candidate-task-g1-on-base"
                );
            }
            let outcome = engine.deliver(immutable_source).await.unwrap();
            let candidate = "candidate-task-g1-on-base";
            assert_eq!(
                outcome,
                DeliveryOutcome::Integrated {
                    candidate_sha: candidate.into()
                }
            );

            let s = state.lock().unwrap();
            assert!(s.crash_injected, "{window:?}");
            assert_eq!(s.head, candidate, "{window:?}");
            assert_eq!(s.durable_head, candidate, "{window:?}");
            assert_eq!(s.attempt_branch_head_sha, candidate, "{window:?}");
            assert_eq!(s.parents.len(), 1, "{window:?}");
            assert_eq!(s.parents[candidate], "base", "{window:?}");
            assert_eq!(s.published_commits, [candidate], "{window:?}");
            assert_eq!(
                s.updates.len(),
                if window == CrashWindow::BeforeRemoteMutation {
                    2
                } else {
                    1
                },
                "{window:?}"
            );
            assert!(
                s.updates
                    .iter()
                    .all(|(old, new)| old == "base" && new == candidate),
                "{window:?}"
            );
            let generation = s.generations.get(&("task".into(), 1)).unwrap();
            assert_eq!(
                generation.state,
                ConcurrentGenerationState::Applied,
                "{window:?}"
            );
            assert_eq!(generation.candidate.candidate_sha, candidate, "{window:?}");
            assert_eq!(
                generation.candidate.selected_parent_sha, "base",
                "{window:?}"
            );
            assert_eq!(s.integrated["task"], candidate, "{window:?}");
            assert_eq!(s.task_status["task"], "closed", "{window:?}");
            assert_eq!(s.task_merge_commit_sha["task"], candidate, "{window:?}");
            assert_eq!(s.closure_calls["task"], 1, "{window:?}");
            assert_eq!(s.dependent_release_calls["task"], 1, "{window:?}");
        }
    }

    #[tokio::test]
    async fn mapped_head_generation_two_provider_error_replays_without_duplicate_push() {
        // Execute both delivery passes. The first expected-old update loses to
        // a mapped head, durably supersedes generation one, and then publishes
        // generation two while its provider response is lost.
        let state = Arc::new(Mutex::new(ConcurrentState {
            head: "base".into(),
            durable_head: "base".into(),
            attempt_branch_head_sha: "base".into(),
            inject_mapped_head_on_base_cas: true,
            provider_error_after_success_on_successor: true,
            ..Default::default()
        }));
        let engine = DirectDeliveryEngine::new(
            ConcurrentLedger {
                state: state.clone(),
                gate: Arc::new(ConcurrentIntegrationGate::default()),
            },
            ConcurrentRemote {
                state: state.clone(),
                first_cas: None,
                second_cas: None,
            },
            CrashBuilder,
        );

        assert!(engine.deliver(source(1)).await.is_err());
        let retained_successor = {
            let state = state.lock().unwrap();
            let first = &state.generations[&("task".into(), 1)];
            let successor = &state.generations[&("task".into(), 2)];
            assert_eq!(first.state, ConcurrentGenerationState::Superseded);
            assert_eq!(first.superseded_by, Some(2));
            assert_eq!(
                first.supersede_transition.as_deref(),
                Some("transition-1:mapped-head:2")
            );
            assert_eq!(first.applying_transition.as_deref(), Some("transition-1"));
            assert_eq!(successor.state, ConcurrentGenerationState::Applying);
            assert_eq!(
                successor.applying_transition.as_deref(),
                Some("transition-1:mapped-head:2")
            );
            assert_eq!(successor.candidate.selected_parent_sha, "mapped");
            assert_eq!(state.head, successor.candidate.candidate_sha);
            assert_eq!(state.updates.len(), 2);
            assert_eq!(
                state.published_commits,
                std::slice::from_ref(&successor.candidate.candidate_sha)
            );
            TaskDelivery {
                identity: successor.identity.clone(),
                state: djinn_core::models::TaskDeliveryState::Applying,
                candidate_sha: successor.candidate.candidate_sha.clone(),
                source_sha: successor.source.source_sha.clone(),
                patch_digest: successor.candidate.patch_digest.clone(),
                selected_parent_sha: successor.candidate.selected_parent_sha.clone(),
                prepare_transition_id: successor.source.transition_id.clone(),
                base_sha: successor.candidate.selected_parent_sha.clone(),
                applied_at: None,
                conflict_reason: None,
                supersede_transition_id: None,
                created_at: "now".into(),
            }
        };
        // This is the production adapter's durable lookup: its current
        // checkout is deliberately ignored in favor of generation two's
        // immutable transition/source identity.
        let replay_source = resume_delivery_source(
            "task",
            "newer-checkout-source".into(),
            "newer-checkout-patch".into(),
            Some(&retained_successor),
        );
        assert_eq!(replay_source.delivery_generation, 2);
        assert_eq!(replay_source.transition_id, "transition-1:mapped-head:2");
        assert_eq!(
            engine.deliver(replay_source).await.unwrap(),
            DeliveryOutcome::Integrated {
                candidate_sha: retained_successor.candidate_sha.clone()
            }
        );
        let state = state.lock().unwrap();
        assert_eq!(state.updates.len(), 2, "exact replay must not push again");
        assert_eq!(state.head, retained_successor.candidate_sha);
        assert_eq!(state.durable_head, retained_successor.candidate_sha);
        assert_eq!(
            state.generations[&("task".into(), 2)].state,
            ConcurrentGenerationState::Applied
        );
        assert_eq!(state.task_status["task"], "closed");
        assert_eq!(
            state.task_merge_commit_sha["task"],
            retained_successor.candidate_sha
        );
        assert_eq!(state.closure_calls["task"], 1);
    }
}
