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
    TaskIntegrationResult, TaskIntegrationStaleness, TaskRepository,
};
use djinn_git::{
    DirectDeliveryBuild, DirectDeliveryInput, DirectDeliverySignature,
    build_direct_delivery_candidate,
};
use djinn_provider::github_api::{ExpectedOldShaRefUpdateResult, GitHubApiClient};
use djinn_workspace::MirrorManager;

/// The sole legacy discriminator, re-exported from `djinn-db` rather than
/// redeclared. The ledger-side SQL (`emit_unblocked_tasks`, the board-health
/// direct section, the `merged` classification) routes on this same label, so
/// one definition keeps the coordinator and the SQL from drifting apart.
pub use djinn_db::LEGACY_DELIVERY_LABEL;

/// Effect boundaries reached by the epoch-aware delivery routing path.
///
/// The recorder is always compiled, not `#[cfg(test)]`. The pod-worker
/// task-PR-open body lives in `djinn-agent`, which depends on this crate — so
/// `djinn-coordinator` can never depend on it back, and a `cfg(test)` recorder
/// would be a hard no-op for exactly the path that had no gate at all. Keeping
/// it compiled costs one uncontended mutex probe per boundary (PR opens and
/// poller ticks, not a hot loop) and is what makes an agent-originated task-PR
/// forge effect observable by the consumer cutover matrix's operation set.
///
/// Recording is still inert until a test installs a
/// [`boundary_operations_scope`]: with no scope, `observe_boundary_operation`
/// reads `None` and returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundaryOperation {
    CapabilityProbe,
    ResolveTaskActiveAttempt,
    NoProposalOwnerPark,
    DirectAppend,
    SimpleClose,
    SupervisorPrOpen,
    TaskPrLookup,
    TaskPrAdopt,
    TaskPrStatusPoll,
    TaskPrReviewPoll,
    TaskPrMergedPoll,
    TaskPrInlineCleanup,
    TaskPrStaleCleanup,
    TaskPrCreate,
    TaskPrMerge,
    TaskPrAutoMerge,
    TaskPrApproval,
    TaskPrSignoff,
    TaskPrCustomEnqueue,
    /// The attempt-scoped draft-PR request was issued before its provider
    /// response was awaited or classified.
    AttemptPrCreateOrAdoptRequest,
}

static BOUNDARY_OPERATIONS: std::sync::Mutex<
    Option<(std::thread::ThreadId, Vec<BoundaryOperation>)>,
> = std::sync::Mutex::new(None);

// The recorder follows real production calls, but observation is enabled only
// while the owning test thread holds this lock. The owner thread ID travels
// with the buffer so another concurrently running test cannot add effects to
// this scope. Tokio's default test runtime is current-thread, so effects from
// the test's production calls retain that owner identity across awaits.
static BOUNDARY_OPERATIONS_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Poison-tolerant access to the shared observation buffer.
///
/// The recorder now compiles into production builds, where `unwrap` on a
/// poisoned lock is denied — and rightly so: a panicking test must never be
/// able to abort a production PR-open. Recovering the inner value is correct
/// here because the buffer is a plain `Vec` with no invariant a panic could
/// have broken mid-write.
fn boundary_operations()
-> std::sync::MutexGuard<'static, Option<(std::thread::ThreadId, Vec<BoundaryOperation>)>> {
    BOUNDARY_OPERATIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub struct BoundaryOperationsScope {
    owner: std::thread::ThreadId,
    _guard: tokio::sync::MutexGuard<'static, ()>,
}

impl BoundaryOperationsScope {
    /// This scope's ordered production-effect stream so far.
    fn observed(&self) -> Vec<BoundaryOperation> {
        assert_eq!(
            std::thread::current().id(),
            self.owner,
            "boundary recorder used outside its owner thread"
        );
        let buffer = boundary_operations();
        let observed = buffer
            .as_ref()
            .filter(|(owner, _)| *owner == self.owner)
            .map(|(_, operations)| operations.clone());
        assert!(
            observed.is_some(),
            "boundary recorder used outside its owner scope"
        );
        observed.unwrap_or_default()
    }

    /// Marks a point in this scope's ordered production-effect stream.
    pub fn checkpoint(&self) -> usize {
        self.observed().len()
    }

    /// Returns effects observed after `checkpoint` without consuming them.
    pub fn operations_since(&self, checkpoint: usize) -> Vec<BoundaryOperation> {
        self.observed()[checkpoint..].to_vec()
    }
}

impl Drop for BoundaryOperationsScope {
    fn drop(&mut self) {
        // A panic or early return cannot leak observations into a later scope.
        *boundary_operations() = None;
    }
}

/// Install a process-wide observation scope for the calling test thread.
///
/// Exported beyond this crate so `djinn-agent`'s pod-worker PR-open regression
/// observes the same boundary stream the coordinator matrix does.
pub async fn boundary_operations_scope() -> BoundaryOperationsScope {
    let guard = BOUNDARY_OPERATIONS_TEST_LOCK.lock().await;
    let owner = std::thread::current().id();
    *boundary_operations() = Some((owner, Vec::new()));
    BoundaryOperationsScope {
        owner,
        _guard: guard,
    }
}

/// Record one production effect boundary.
///
/// Inert unless a [`boundary_operations_scope`] is installed on the calling
/// thread, so production behavior and the disabled epoch are preserved.
pub fn observe_boundary_operation(operation: &'static str) {
    let operation = match operation {
        "capability_probe" => BoundaryOperation::CapabilityProbe,
        "resolve_task_active_attempt" => BoundaryOperation::ResolveTaskActiveAttempt,
        "no_proposal_owner_park" => BoundaryOperation::NoProposalOwnerPark,
        "direct_append" => BoundaryOperation::DirectAppend,
        "simple_close" => BoundaryOperation::SimpleClose,
        "supervisor_pr_open" => BoundaryOperation::SupervisorPrOpen,
        "task_pr_lookup" => BoundaryOperation::TaskPrLookup,
        "task_pr_adopt" => BoundaryOperation::TaskPrAdopt,
        "task_pr_status_poll" => BoundaryOperation::TaskPrStatusPoll,
        "task_pr_review_poll" => BoundaryOperation::TaskPrReviewPoll,
        "task_pr_merged_poll" => BoundaryOperation::TaskPrMergedPoll,
        "task_pr_inline_cleanup" => BoundaryOperation::TaskPrInlineCleanup,
        "task_pr_stale_cleanup" => BoundaryOperation::TaskPrStaleCleanup,
        "task_pr_create" => BoundaryOperation::TaskPrCreate,
        "task_pr_merge" => BoundaryOperation::TaskPrMerge,
        "task_pr_auto_merge" => BoundaryOperation::TaskPrAutoMerge,
        "task_pr_approval" => BoundaryOperation::TaskPrApproval,
        "task_pr_signoff" => BoundaryOperation::TaskPrSignoff,
        "task_pr_custom_enqueue" => BoundaryOperation::TaskPrCustomEnqueue,
        "attempt_pr_create_or_adopt_request" => BoundaryOperation::AttemptPrCreateOrAdoptRequest,
        _ => return,
    };
    if let Some((scope_owner, operations)) = boundary_operations().as_mut()
        && *scope_owner == std::thread::current().id()
    {
        operations.push(operation);
    }
}

/// The only epoch-aware routing decision used by ready admission and completion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirectDeliveryAdmission {
    Legacy,
    Direct { attempt: ActiveAttempt },
    NoProposalOwner,
    ContractUnavailable(DirectDeliveryContract),
}

/// A persisted epoch contract which cannot safely select either delivery mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirectDeliveryContract {
    MissingSchema { missing_relations: Vec<String> },
    MissingEpoch,
    UnknownEpochState { state: String, generation: i64 },
}

/// The sole task-PR routing decision. Direct identities are ineligible for all
/// task-PR effects; explicit legacy labels keep the legacy route eligible.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskPrEligibility {
    LegacyAllowed,
    DirectDeliveryIneligible { attempt: ActiveAttempt },
    NoProposalOwner,
    ContractUnavailable(DirectDeliveryContract),
}

impl TaskPrEligibility {
    fn park_reason(&self) -> Option<&'static str> {
        match self {
            Self::NoProposalOwner => Some("no_proposal_owner"),
            Self::ContractUnavailable(DirectDeliveryContract::MissingSchema { .. }) => {
                Some("direct_delivery_contract_missing_schema")
            }
            Self::ContractUnavailable(DirectDeliveryContract::MissingEpoch) => {
                Some("direct_delivery_contract_missing_epoch")
            }
            Self::ContractUnavailable(DirectDeliveryContract::UnknownEpochState { .. }) => {
                Some("direct_delivery_contract_unknown_epoch")
            }
            Self::LegacyAllowed | Self::DirectDeliveryIneligible { .. } => None,
        }
    }
}

/// Liveness decision for a canonical direct-delivery attempt. This deliberately
/// reads the immutable ledger rather than any nullable task-PR field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DirectDeliveryLiveness {
    Legacy,
    Dispatch,
    /// A prepared/applying generation is owned by the direct engine and must be
    /// reconciled before a worker may be spawned or a task reopened.
    Reconcile,
    /// Applied, conflict, and superseded generations are immutable historical
    /// facts and must never re-enter task-PR liveness handling.
    Settled,
    /// The shared admission wrapper already persisted no_proposal_owner.
    Parked,
}

/// Persist the active-epoch ownership failure before any task-PR side effect.
pub async fn park_no_proposal_owner(repo: &TaskRepository, task_id: &str) -> Result<()> {
    park_direct_delivery_boundary(repo, task_id, "no_proposal_owner").await
}

async fn park_direct_delivery_boundary(
    repo: &TaskRepository,
    task_id: &str,
    reason: &'static str,
) -> Result<()> {
    repo.transition(
        task_id,
        TransitionAction::Escalate,
        "coordinator",
        "system",
        Some(reason),
        None,
    )
    .await?;
    observe_boundary_operation("no_proposal_owner_park");
    Ok(())
}

/// Persist a fail-closed task-PR result before a caller can reach any mirror or
/// forge effect. Direct identities retain their active attempt lifecycle.
pub async fn park_task_pr_ineligibility(
    repo: &TaskRepository,
    task_id: &str,
    eligibility: &TaskPrEligibility,
) -> Result<()> {
    if let Some(reason) = eligibility.park_reason() {
        park_direct_delivery_boundary(repo, task_id, reason).await?;
    }
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
            if has_explicit_legacy_delivery(&labels) {
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
        DirectDeliverySchemaCapability::MissingSchema { missing_relations } => {
            Ok(DirectDeliveryAdmission::ContractUnavailable(
                DirectDeliveryContract::MissingSchema { missing_relations },
            ))
        }
        DirectDeliverySchemaCapability::MissingEpoch => Ok(
            DirectDeliveryAdmission::ContractUnavailable(DirectDeliveryContract::MissingEpoch),
        ),
        DirectDeliverySchemaCapability::UnknownEpochState { state, generation } => {
            Ok(DirectDeliveryAdmission::ContractUnavailable(
                DirectDeliveryContract::UnknownEpochState { state, generation },
            ))
        }
    }
}

/// Production ready-task boundary. It consumes the shared admission result
/// before durably parking unresolved active ownership.
pub(crate) async fn admit_ready_direct_delivery(
    db: Database,
    tasks: &TaskRepository,
    task_id: &str,
) -> Result<DirectDeliveryAdmission> {
    let admission = admit_direct_delivery(db, task_id).await?;
    if let Some(eligibility) = fail_closed_task_pr_eligibility(&admission) {
        park_task_pr_ineligibility(tasks, task_id, &eligibility).await?;
    }
    Ok(admission)
}

/// Production liveness fence used before ready-task spawn and respawn handling.
/// It shares the epoch gate and canonical active-attempt resolver with
/// completion, then makes the ledger authoritative for direct task liveness.
pub(crate) async fn admit_direct_delivery_liveness(
    db: Database,
    tasks: &TaskRepository,
    task_id: &str,
) -> Result<DirectDeliveryLiveness> {
    match admit_ready_direct_delivery(db.clone(), tasks, task_id).await? {
        DirectDeliveryAdmission::Legacy => Ok(DirectDeliveryLiveness::Legacy),
        DirectDeliveryAdmission::NoProposalOwner
        | DirectDeliveryAdmission::ContractUnavailable(_) => Ok(DirectDeliveryLiveness::Parked),
        DirectDeliveryAdmission::Direct { attempt } => {
            let delivery = tasks
                .latest_delivery_for_attempt(&attempt.build_attempt_id, task_id)
                .await?;
            Ok(match delivery.map(|delivery| delivery.state) {
                None => DirectDeliveryLiveness::Dispatch,
                Some(
                    djinn_core::models::TaskDeliveryState::Prepared
                    | djinn_core::models::TaskDeliveryState::Applying,
                ) => DirectDeliveryLiveness::Reconcile,
                Some(
                    djinn_core::models::TaskDeliveryState::Applied
                    | djinn_core::models::TaskDeliveryState::Conflict
                    | djinn_core::models::TaskDeliveryState::Superseded,
                ) => DirectDeliveryLiveness::Settled,
            })
        }
    }
}

/// Production approved-task boundary. Completion cannot bypass the same
/// capability and canonical-ownership decision used by ready admission.
pub(crate) async fn admit_approved_direct_delivery(
    db: Database,
    tasks: &TaskRepository,
    task_id: &str,
) -> Result<DirectDeliveryAdmission> {
    let admission = admit_direct_delivery(db, task_id).await?;
    if let Some(eligibility) = fail_closed_task_pr_eligibility(&admission) {
        park_task_pr_ineligibility(tasks, task_id, &eligibility).await?;
    }
    Ok(admission)
}

/// Derive task-PR eligibility from the landed epoch admission and canonical
/// active-attempt resolver, never from a nullable task PR identity.
pub async fn task_pr_eligibility(db: Database, task_id: &str) -> Result<TaskPrEligibility> {
    Ok(match admit_direct_delivery(db, task_id).await? {
        DirectDeliveryAdmission::Legacy => TaskPrEligibility::LegacyAllowed,
        DirectDeliveryAdmission::Direct { attempt } => {
            TaskPrEligibility::DirectDeliveryIneligible { attempt }
        }
        DirectDeliveryAdmission::NoProposalOwner => TaskPrEligibility::NoProposalOwner,
        DirectDeliveryAdmission::ContractUnavailable(contract) => {
            TaskPrEligibility::ContractUnavailable(contract)
        }
    })
}

fn fail_closed_task_pr_eligibility(
    admission: &DirectDeliveryAdmission,
) -> Option<TaskPrEligibility> {
    match admission {
        DirectDeliveryAdmission::NoProposalOwner => Some(TaskPrEligibility::NoProposalOwner),
        DirectDeliveryAdmission::ContractUnavailable(contract) => {
            Some(TaskPrEligibility::ContractUnavailable(contract.clone()))
        }
        DirectDeliveryAdmission::Legacy | DirectDeliveryAdmission::Direct { .. } => None,
    }
}

fn has_explicit_legacy_delivery(labels: &[String]) -> bool {
    labels.iter().any(|label| label == LEGACY_DELIVERY_LABEL)
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
    /// Transient: the ledger declined for a condition another attempt can clear.
    Stale,
    /// Permanent: the ledger declined for a condition no retry can clear.
    ///
    /// Kept a separate variant rather than a flag on `Stale` so that a caller
    /// which only knows how to wait for staleness to pass cannot silently treat
    /// this as something to wait for.
    PermanentlyStale(PermanentStaleness),
}

impl LedgerResult {
    /// Whether the ledger declined the transition, transiently or permanently.
    ///
    /// Transition writers (prepare, rework, begin-apply, finalize) already treat
    /// any decline as terminal, so they ask this rather than comparing against
    /// one variant and silently ignoring the other.
    pub const fn is_stale(self) -> bool {
        matches!(self, Self::Stale | Self::PermanentlyStale(_))
    }
}

/// A ledger decline that no number of retries can turn into an integration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermanentStaleness {
    /// The task is not `approved`. Every recovery seam that reconciles an
    /// `Applying` generation selects `in_progress`, `in_task_review`, or
    /// `in_lead_intervention`, so this is the production-reachable case.
    TaskNotApproved,
    /// No ledger row exists at this exact delivery identity.
    MissingGeneration,
    /// The generation is no longer `applying`.
    GenerationNotApplying,
    /// The persisted candidate and the observed applied candidate disagree.
    CandidateIdentityMismatch,
}

impl PermanentStaleness {
    fn from_ledger(staleness: TaskIntegrationStaleness) -> Option<Self> {
        match staleness {
            TaskIntegrationStaleness::UnfinalizedAttemptHead => None,
            TaskIntegrationStaleness::TaskNotApproved => Some(Self::TaskNotApproved),
            TaskIntegrationStaleness::MissingGeneration => Some(Self::MissingGeneration),
            TaskIntegrationStaleness::GenerationNotApplying => Some(Self::GenerationNotApplying),
            TaskIntegrationStaleness::CandidateIdentityMismatch => {
                Some(Self::CandidateIdentityMismatch)
            }
        }
    }
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
            // The ledger already knows which decline can converge; carry that
            // fact rather than re-deriving it from nullable row snapshots.
            TaskIntegrationResult::Stale { staleness, .. } => {
                match PermanentStaleness::from_ledger(staleness) {
                    Some(permanent) => LedgerResult::PermanentlyStale(permanent),
                    None => LedgerResult::Stale,
                }
            }
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
    Integrated {
        candidate_sha: String,
    },
    ConflictParked {
        reason: String,
    },
    UnexpectedHeadParked {
        observed_sha: Option<String>,
    },
    RetryBoundParked {
        observed_heads: usize,
    },
    /// The remote already carries this generation's candidate, but the ledger
    /// will never integrate it. Terminal: nothing here converges by waiting.
    Unintegrable {
        candidate_sha: String,
        reason: PermanentStaleness,
    },
    /// The remote already carries this generation's candidate and the ledger
    /// decline is genuinely transient, but its selected parent did not finalize
    /// within [`INTEGRATION_RECONCILE_BUDGET`].
    ///
    /// Terminal **for this call only**: the exact-candidate reconciliation at
    /// the top of `deliver` re-enters this same integration on the next pass,
    /// so the caller's own loop owns the next attempt rather than this one
    /// holding a coordinator loop open indefinitely.
    IntegrationDeferred {
        candidate_sha: String,
    },
    Disabled,
}

/// How long `integrate` waits for a genuinely transient decline to clear.
///
/// A selected parent's transaction is a database transaction, not a scheduler
/// turn, so the budget is wall-clock rather than a poll count. It is bounded
/// because a coordinator recovery pass that never returns is a worse failure
/// than one that defers: exhausting it is reported, not swallowed.
pub const INTEGRATION_RECONCILE_BUDGET: Duration = Duration::from_secs(10);

/// How many times the same already-mapped head may be replayed before the
/// attempt is parked at the stale-head retry bound.
///
/// Distinct from the three-distinct-heads budget: that one bounds how many
/// immutable generations a topology race may mint, this one bounds re-CASing
/// the candidate already prepared for one head.
pub const MAPPED_HEAD_REPLAY_BUDGET: usize = 3;

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
                    .is_stale()
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
                if finalized.is_stale()
                    || (applying.is_stale() && finalized != LedgerResult::Replayed)
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
            .is_stale()
            || self
                .ledger
                .begin_apply(&identity, &delivery_source.transition_id)
                .await?
                .is_stale()
        {
            return Ok(DeliveryOutcome::RetryBoundParked { observed_heads: 0 });
        }
        let mut observed_mapped_heads = HashSet::new();
        let mut replayed_mapped_heads = 0usize;
        loop {
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
                    //
                    // Bounded, because this arm mints no generation and does no
                    // ledger work: a remote that keeps reporting a head whose
                    // own CAS it then rejects would otherwise spin here without
                    // even yielding, and this loop is reachable from the
                    // coordinator's recovery seams.
                    if !observed_mapped_heads.insert(head.clone()) {
                        debug_assert_eq!(candidate.selected_parent_sha, head);
                        if replayed_mapped_heads >= MAPPED_HEAD_REPLAY_BUDGET {
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
                        replayed_mapped_heads += 1;
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
                                .is_stale()
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
                            if finalized.is_stale()
                                || (applying.is_stale() && finalized != LedgerResult::Replayed)
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
                        .is_stale()
                        || self
                            .ledger
                            .begin_apply(&next_identity, &next_source.transition_id)
                            .await?
                            .is_stale()
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
        let deadline = tokio::time::Instant::now() + INTEGRATION_RECONCILE_BUDGET;
        loop {
            match self
                .ledger
                .integrate(TaskIntegrated::new(identity.clone(), &sha, &sha, &sha)?)
                .await?
            {
                LedgerResult::Applied | LedgerResult::Replayed => {
                    return Ok(DeliveryOutcome::Integrated { candidate_sha: sha });
                }
                // No wait clears this one. The recovery seams reconcile
                // `Applying` generations on tasks that are `in_progress`,
                // `in_task_review`, or `in_lead_intervention` — none of which
                // `task_integrated` can close from — so retrying here is what
                // used to hang the coordinator's recovery pass outright.
                LedgerResult::PermanentlyStale(reason) => {
                    return Ok(DeliveryOutcome::Unintegrable {
                        candidate_sha: sha,
                        reason,
                    });
                }
                // `TaskIntegrated` performs the transactional durable-head
                // check. A transient decline means its selected parent has not
                // finalized yet; wait before asking that transaction to
                // reconcile again rather than misclassifying a transient parent
                // transaction as an unexpected remote head.
                LedgerResult::Stale => {
                    if tokio::time::Instant::now() >= deadline {
                        return Ok(DeliveryOutcome::IntegrationDeferred { candidate_sha: sha });
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
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
        /// Results `integrate` hands back in order. The last entry repeats, so
        /// a script ending in a decline models a condition that never clears.
        integrate_results: Arc<Mutex<std::collections::VecDeque<LedgerResult>>>,
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
            let mut scripted = self
                .integrate_results
                .lock()
                .map_err(|_| anyhow!("poison"))?;
            Ok(match scripted.len() {
                0 => LedgerResult::Applied,
                1 => scripted[0],
                _ => scripted.pop_front().unwrap_or(LedgerResult::Applied),
            })
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
            integrate_results: Arc::new(Mutex::new(std::collections::VecDeque::new())),
        }
    }

    /// Count the integration attempts the engine actually made.
    fn integrate_calls(calls: &Arc<Mutex<Vec<String>>>) -> usize {
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.starts_with("integrate:"))
            .count()
    }

    /// One clean delivery reaching integration, with a scripted ledger.
    fn integration_engine(
        calls: Arc<Mutex<Vec<String>>>,
        scripted: Vec<LedgerResult>,
    ) -> DirectDeliveryEngine<Ledger, Remote, Builder> {
        let (remote, _) = remote(
            vec![RemoteUpdate::Updated {
                sha: "commit-base".into(),
            }],
            vec![Some("base")],
        );
        let mut state = ledger(calls);
        state.integrate_results = Arc::new(Mutex::new(scripted.into()));
        DirectDeliveryEngine::new(state, remote, Builder { conflict: false })
    }

    // ─── i5fn: transient and permanent staleness are not the same wait ─────

    /// Transient staleness still retries, and still converges.
    #[tokio::test(start_paused = true)]
    async fn transient_integration_staleness_retries_until_it_converges() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let engine = integration_engine(
            calls.clone(),
            vec![
                LedgerResult::Stale,
                LedgerResult::Stale,
                LedgerResult::Stale,
                LedgerResult::Applied,
            ],
        );
        assert_eq!(
            engine.deliver(source(1)).await.unwrap(),
            DeliveryOutcome::Integrated {
                candidate_sha: "commit-base".into()
            }
        );
        assert_eq!(
            integrate_calls(&calls),
            4,
            "every transient decline must be retried, not swallowed"
        );
    }

    /// A permanently stale generation is terminal on its first answer: it is
    /// neither retried nor reported as an integration.
    #[tokio::test(start_paused = true)]
    async fn permanent_integration_staleness_is_terminal_without_a_single_retry() {
        for reason in [
            PermanentStaleness::TaskNotApproved,
            PermanentStaleness::MissingGeneration,
            PermanentStaleness::GenerationNotApplying,
            PermanentStaleness::CandidateIdentityMismatch,
        ] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let engine =
                integration_engine(calls.clone(), vec![LedgerResult::PermanentlyStale(reason)]);
            assert_eq!(
                engine.deliver(source(1)).await.unwrap(),
                DeliveryOutcome::Unintegrable {
                    candidate_sha: "commit-base".into(),
                    reason
                }
            );
            assert_eq!(
                integrate_calls(&calls),
                1,
                "{reason:?}: a condition no retry can clear must not be retried"
            );
        }
    }

    /// The remaining wait is bounded. A transient decline that never clears is
    /// deferred to the caller's own loop rather than held open forever.
    ///
    /// Time is paused, so the assertion is about the engine's bound and not
    /// about how fast this machine is.
    #[tokio::test(start_paused = true)]
    async fn a_transient_decline_that_never_clears_is_deferred_not_spun_on() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let engine = integration_engine(calls.clone(), vec![LedgerResult::Stale]);
        let started = tokio::time::Instant::now();
        assert_eq!(
            engine.deliver(source(1)).await.unwrap(),
            DeliveryOutcome::IntegrationDeferred {
                candidate_sha: "commit-base".into()
            }
        );
        assert!(
            started.elapsed() >= INTEGRATION_RECONCILE_BUDGET,
            "the engine must actually spend its budget before deferring"
        );
        assert!(
            integrate_calls(&calls) > 1,
            "a transient decline is retried inside the budget"
        );
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
    async fn reconciliation_collaborator_records_the_real_direct_engine_effect() {
        use crate::dispatch::wave_dispatch::run_direct_completion;

        let boundary_operations = boundary_operations_scope().await;
        let boundary_checkpoint = boundary_operations.checkpoint();
        let outcome = run_direct_completion(|| async { "reconciled" }).await;

        assert_eq!(outcome, "reconciled");
        assert_eq!(
            boundary_operations.operations_since(boundary_checkpoint),
            [BoundaryOperation::DirectAppend]
        );
    }

    #[tokio::test]
    async fn boundary_recorder_ignores_operations_from_an_unowned_test_thread() {
        let boundary_operations = boundary_operations_scope().await;
        let boundary_checkpoint = boundary_operations.checkpoint();

        std::thread::spawn(|| observe_boundary_operation("direct_append"))
            .join()
            .unwrap();
        observe_boundary_operation("simple_close");

        assert_eq!(
            boundary_operations.operations_since(boundary_checkpoint),
            [BoundaryOperation::SimpleClose],
            "an unscoped concurrent test thread must not write this scope's buffer"
        );
    }

    #[tokio::test]
    async fn explicit_legacy_completion_preserves_existing_persisted_pr_identity() {
        use crate::dispatch::wave_dispatch::{
            route_approved_completion, run_legacy_completion_preserving_pr_identity,
        };
        use djinn_core::events::EventBus;
        use djinn_db::{EpicRepository, TaskRepository};

        let boundary_operations = boundary_operations_scope().await;
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
        repo.update_labels(&task.id, &format!(r#"["{LEGACY_DELIVERY_LABEL}"]"#))
            .await
            .unwrap();
        // Completion receives the same persisted/reloaded shape as production.
        let task = repo.get(&task.id).await.unwrap().unwrap();
        djinn_db::test_support::activate_direct_delivery_epoch_for_test(&db).await;

        let boundary_checkpoint = boundary_operations.checkpoint();
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
            boundary_operations.operations_since(boundary_checkpoint),
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
    /// A remote that keeps reporting the same mapped head its own CAS rejects
    /// used to spin this arm forever without yielding. It is bounded now.
    ///
    /// The remote here is exhaustible on purpose: an unbounded engine would
    /// drain it and then fail on "missing update" rather than park, so the
    /// assertion cannot be satisfied by an engine that never stops.
    #[tokio::test]
    async fn endlessly_replayed_mapped_head_parks_at_the_replay_bound() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (mut remote, updates) = remote(
            std::iter::repeat_n(
                RemoteUpdate::Stale {
                    observed_sha: Some("mapped".into()),
                },
                MAPPED_HEAD_REPLAY_BUDGET + 3,
            )
            .collect(),
            vec![Some("base")],
        );
        remote.calls = Some(calls.clone());
        let mut state = ledger(calls.clone());
        state.mapped = true;
        let engine = DirectDeliveryEngine::new(state, remote, Builder { conflict: false });
        assert_eq!(
            engine.deliver(source(1)).await.unwrap(),
            DeliveryOutcome::RetryBoundParked { observed_heads: 1 }
        );
        let calls = calls.lock().unwrap();
        assert_eq!(calls.last().unwrap(), "park:StaleHeadRetryBound");
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.as_str() == "update:mapped:commit-mapped")
                .count(),
            MAPPED_HEAD_REPLAY_BUDGET + 1,
            "the replayed head is retried a bounded number of times and no more"
        );
        assert!(
            !updates.lock().unwrap().is_empty(),
            "parking must happen before the scripted remote is exhausted"
        );
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
    /// Repository-backed cross-product for both production admission boundaries.
    #[tokio::test]
    async fn production_ready_and_completion_admission_matrix_is_fail_closed() {
        use crate::dispatch::wave_dispatch::{
            route_approved_completion, run_direct_completion,
            run_legacy_completion_preserving_pr_identity,
        };
        use djinn_core::events::EventBus;
        use djinn_core::models::ProposalBuildAttemptLifecycle;
        use djinn_db::{
            ActivateProposalBuildAttemptInput, EpicRepository, ProposalBuildAttemptRepository,
            ReserveProposalBuildAttemptInput, TaskRepository,
        };

        async fn snapshot(
            db: &Database,
            tasks: &TaskRepository,
            task_id: &str,
        ) -> (
            String,
            Option<String>,
            Option<String>,
            i64,
            i64,
            Option<i64>,
            Option<i64>,
        ) {
            let task = tasks.get(task_id).await.unwrap().unwrap();
            let activities = djinn_db::test_support::activity_row_count_for_test(db, task_id).await;
            let attempts = djinn_db::test_support::task_attempt_count_for_test(db, task_id).await;
            let direct_delivery_counts =
                djinn_db::test_support::direct_delivery_matrix_counts_for_test(db).await;
            (
                task.status,
                task.pr_url,
                task.merge_commit_sha,
                activities,
                attempts,
                direct_delivery_counts.build_attempts,
                direct_delivery_counts.deliveries,
            )
        }

        #[derive(Clone, Copy)]
        enum State {
            Disabled,
            ExplicitLegacy,
            Direct,
            Unresolved,
            MissingSchema,
            MissingEpoch,
            UnknownEpoch,
        }
        let boundary_operations = boundary_operations_scope().await;
        for state in [
            State::Disabled,
            State::ExplicitLegacy,
            State::Direct,
            State::Unresolved,
            State::MissingSchema,
            State::MissingEpoch,
            State::UnknownEpoch,
        ] {
            for completion in [false, true] {
                let db = Database::open_in_memory().unwrap();
                let events = EventBus::noop();
                let epic = EpicRepository::new(db.clone(), events.clone())
                    .create("matrix", "", "", "", "", None)
                    .await
                    .unwrap();
                let tasks = TaskRepository::new(db.clone(), events);
                let task = tasks
                    .create(
                        &epic.id,
                        "matrix task",
                        "",
                        "",
                        "task",
                        0,
                        "worker",
                        Some(if completion { "approved" } else { "open" }),
                    )
                    .await
                    .unwrap();
                if matches!(state, State::ExplicitLegacy) {
                    tasks
                        .set_pr_url(&task.id, "https://example.test/pr/unchanged")
                        .await
                        .unwrap();
                    tasks
                        .update_labels(&task.id, &format!(r#"["{LEGACY_DELIVERY_LABEL}"]"#))
                        .await
                        .unwrap();
                }
                if matches!(
                    state,
                    State::Direct | State::Unresolved | State::ExplicitLegacy
                ) {
                    djinn_db::test_support::activate_direct_delivery_epoch_for_test(&db).await;
                }
                if matches!(state, State::Direct) {
                    djinn_db::test_support::seed_direct_delivery_proposal_owner_for_test(
                        &db, &epic.id, "p", "p",
                    )
                    .await;
                    let attempts = ProposalBuildAttemptRepository::new(db.clone());
                    attempts
                        .reserve(&ReserveProposalBuildAttemptInput {
                            proposal_id: "p".into(),
                            proposal_short_id: "p".into(),
                            build_attempt_id: "a".into(),
                            build_attempt_short_id: "a".into(),
                            observed_base_sha: "base".into(),
                        })
                        .await
                        .unwrap();
                    attempts
                        .activate(&ActivateProposalBuildAttemptInput {
                            build_attempt_id: "a".into(),
                            expected_lifecycle: ProposalBuildAttemptLifecycle::Reserved,
                            expected_branch_head_sha: None,
                            branch_head_sha: "base".into(),
                        })
                        .await
                        .unwrap();
                }
                match state {
                    State::MissingSchema => {
                        djinn_db::test_support::drop_table_cascade_for_test(&db, "task_deliveries")
                            .await
                    }
                    State::MissingEpoch => {
                        djinn_db::test_support::remove_direct_delivery_epoch_for_test(&db).await;
                    }
                    State::UnknownEpoch => {
                        djinn_db::test_support::seed_unknown_direct_delivery_epoch_for_test(&db)
                            .await;
                    }
                    _ => {}
                }
                let before = snapshot(&db, &tasks, &task.id).await;
                let boundary_checkpoint = boundary_operations.checkpoint();
                let admission = if completion {
                    admit_approved_direct_delivery(db.clone(), &tasks, &task.id).await
                } else {
                    admit_ready_direct_delivery(db.clone(), &tasks, &task.id).await
                };
                match (&admission, state) {
                    (
                        Ok(DirectDeliveryAdmission::Legacy),
                        State::Disabled | State::ExplicitLegacy,
                    ) => {}
                    (Ok(DirectDeliveryAdmission::Direct { .. }), State::Direct) => {}
                    (Ok(DirectDeliveryAdmission::NoProposalOwner), State::Unresolved) => {}
                    (
                        Ok(DirectDeliveryAdmission::ContractUnavailable(_)),
                        State::MissingSchema | State::MissingEpoch | State::UnknownEpoch,
                    ) => {}
                    _ => panic!("matrix state selected the wrong admission route"),
                }
                let external_pr_seen = std::sync::Arc::new(std::sync::Mutex::new(None));
                if let Ok(admission) = admission
                    && completion
                    && matches!(
                        admission,
                        DirectDeliveryAdmission::Legacy | DirectDeliveryAdmission::Direct { .. }
                    )
                {
                    let completion_task = tasks.get(&task.id).await.unwrap().unwrap();
                    let legacy_pr = completion_task
                        .pr_url
                        .clone()
                        .unwrap_or_else(|| "https://example.test/pr/legacy".to_owned());
                    let db_for_legacy = db.clone();
                    let external_pr_for_legacy = external_pr_seen.clone();
                    let task_for_legacy = completion_task.clone();
                    route_approved_completion(
                        admission,
                        || async { run_direct_completion(|| async {}).await },
                        || async move {
                            run_legacy_completion_preserving_pr_identity(
                                &db_for_legacy,
                                &task_for_legacy,
                                || async move {
                                    *external_pr_for_legacy.lock().unwrap() =
                                        Some(legacy_pr.clone());
                                    djinn_runtime::TaskRunOutcome::PrOpened {
                                        url: legacy_pr,
                                        sha: "legacy-head".to_owned(),
                                    }
                                },
                            )
                            .await
                            .unwrap();
                        },
                    )
                    .await;
                }
                let after = snapshot(&db, &tasks, &task.id).await;
                if matches!(
                    state,
                    State::Unresolved
                        | State::MissingSchema
                        | State::MissingEpoch
                        | State::UnknownEpoch
                ) {
                    assert_eq!(after.0, "needs_lead_intervention");
                    assert_eq!(
                        after.1, before.1,
                        "fail-closed parking must not alter PR identity"
                    );
                    assert_eq!(
                        after.2, before.2,
                        "fail-closed parking must not integrate the task"
                    );
                    assert_eq!(
                        after.4, before.4,
                        "fail-closed parking must not alter task attempts"
                    );
                    assert_eq!(
                        after.5, before.5,
                        "fail-closed parking must not alter build attempts"
                    );
                    assert_eq!(
                        after.6, before.6,
                        "fail-closed parking must not alter delivery ledger"
                    );
                }
                if matches!(state, State::ExplicitLegacy) {
                    assert_eq!(
                        after.1.as_deref(),
                        Some("https://example.test/pr/unchanged")
                    );
                    if completion {
                        assert_eq!(
                            external_pr_seen.lock().unwrap().as_deref(),
                            Some("https://example.test/pr/unchanged"),
                            "persisted and externally observed explicit legacy PR identities must match"
                        );
                    }
                }
                let expected_ops = match (state, completion) {
                    (State::Disabled | State::ExplicitLegacy, false) => {
                        vec![BoundaryOperation::CapabilityProbe]
                    }
                    (State::Disabled | State::ExplicitLegacy, true) => vec![
                        BoundaryOperation::CapabilityProbe,
                        BoundaryOperation::SupervisorPrOpen,
                    ],
                    (State::Direct, false) => vec![
                        BoundaryOperation::CapabilityProbe,
                        BoundaryOperation::ResolveTaskActiveAttempt,
                    ],
                    (State::Direct, true) => vec![
                        BoundaryOperation::CapabilityProbe,
                        BoundaryOperation::ResolveTaskActiveAttempt,
                        BoundaryOperation::DirectAppend,
                    ],
                    (State::Unresolved, _) => vec![
                        BoundaryOperation::CapabilityProbe,
                        BoundaryOperation::ResolveTaskActiveAttempt,
                        BoundaryOperation::NoProposalOwnerPark,
                    ],
                    (State::MissingSchema | State::MissingEpoch | State::UnknownEpoch, _) => vec![
                        BoundaryOperation::CapabilityProbe,
                        BoundaryOperation::NoProposalOwnerPark,
                    ],
                };
                assert_eq!(
                    boundary_operations.operations_since(boundary_checkpoint),
                    expected_ops,
                    "every matrix cell must assert the complete ordered production effect vector"
                );
            }
        }
    }
}

#[cfg(test)]
mod ready_dispatch_repository_liveness_tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct FixtureRemote(Arc<Mutex<(String, usize)>>);
    #[async_trait]
    impl AttemptRef for FixtureRemote {
        async fn observe(&self, _: &str) -> Result<Option<String>> {
            Ok(Some(self.0.lock().unwrap().0.clone()))
        }
        async fn update_expected_old(&self, _: &str, old: &str, new: &str) -> Result<RemoteUpdate> {
            let mut state = self.0.lock().unwrap();
            state.1 += 1;
            if state.0 == old {
                state.0 = new.into();
                Ok(RemoteUpdate::Updated { sha: new.into() })
            } else {
                Ok(RemoteUpdate::Stale {
                    observed_sha: Some(state.0.clone()),
                })
            }
        }
    }
    struct FixtureBuilder;
    #[async_trait]
    impl CandidateBuilder for FixtureBuilder {
        async fn build(
            &self,
            _: &TaskDeliveryIdentity,
            _: &DeliverySource,
            parent: &str,
        ) -> Result<CandidateBuild> {
            Ok(CandidateBuild::Clean(Candidate {
                candidate_sha: "fixture-candidate".into(),
                patch_digest: "fixture-patch".into(),
                selected_parent_sha: parent.into(),
            }))
        }
    }

    /// Repository-backed ready admission reaches the exact collaborator called
    /// from `dispatch_ready_tasks` before it can select a role or spawn a slot.
    /// The fixture leaves `pr_url` null: direct liveness comes from canonical
    /// ownership and the immutable delivery generation, never nullable PR data.
    #[tokio::test]
    async fn ready_dispatch_collaborator_reconciles_and_replays_repository_delivery() {
        use djinn_core::events::EventBus;
        use djinn_db::test_support::seed_direct_delivery_liveness_fixture_for_test;
        use djinn_db::{Database, EpicRepository, ProposalBuildAttemptRepository, TaskRepository};
        let boundary_operations = boundary_operations_scope().await;
        let db = Database::open_in_memory().unwrap();
        let epic = EpicRepository::new(db.clone(), EventBus::noop())
            .create("ready", "", "", "", "", None)
            .await
            .unwrap();
        let updates = Arc::new(Mutex::new(Vec::new()));
        let observed_updates = updates.clone();
        let observing_events = EventBus::new(move |event| {
            if event.entity_type == "task" && event.action == "updated" {
                observed_updates.lock().unwrap().push(event.payload);
            }
        });
        let tasks = TaskRepository::new(db.clone(), observing_events.clone());
        let task = tasks
            .create(&epic.id, "ready", "", "", "task", 0, "", Some("approved"))
            .await
            .unwrap();
        let dependent = tasks
            .create(&epic.id, "dependent", "", "", "task", 0, "", Some("open"))
            .await
            .unwrap();
        tasks.add_blocker(&dependent.id, &task.id).await.unwrap();
        let fixture = seed_direct_delivery_liveness_fixture_for_test(
            &db,
            &epic.id,
            &task.id,
            Some("applying"),
        )
        .await;
        assert!(task.pr_url.is_none());
        let remote = Arc::new(Mutex::new(("fixture-base".to_owned(), 0)));
        let engine = Arc::new(DirectDeliveryEngine::new(
            RepositoryDeliveryLedger::new(
                db.clone(),
                ProposalBuildAttemptRepository::new(db.clone()),
                // The ledger owns TaskIntegrated and dependent release. Share this
                // fixture's observer with that production ownership boundary.
                TaskRepository::new(db.clone(), observing_events),
            ),
            FixtureRemote(remote.clone()),
            FixtureBuilder,
        ));
        let source = DeliverySource {
            task_id: task.id.clone(),
            delivery_generation: 1,
            transition_id: "fixture-prepare".into(),
            source_sha: "fixture-source".into(),
            normalized_patch: "fixture-patch".into(),
        };
        let boundary_checkpoint = boundary_operations.checkpoint();
        updates.lock().unwrap().clear();
        let reconciliations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reconciliations_for_engine = reconciliations.clone();
        let continuations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let continuations_for_apply = continuations.clone();
        let decision = crate::dispatch::task_dispatch::continue_ready_dispatch(
            db.clone(),
            &tasks,
            &task.id,
            || {
                let engine = engine.clone();
                async move {
                    reconciliations_for_engine.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    crate::dispatch::wave_dispatch::run_direct_completion(|| engine.deliver(source))
                        .await
                }
            },
            || async move {
                continuations_for_apply.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(
            decision,
            crate::dispatch::task_dispatch::ReadyDispatchContinuation::Reconciled
        );
        let closed = tasks.get(&task.id).await.unwrap().unwrap();
        assert_eq!(
            (closed.status.as_str(), closed.merge_commit_sha.as_deref()),
            ("closed", Some("fixture-candidate"))
        );
        let counts = djinn_db::test_support::direct_delivery_matrix_counts_for_test(&db).await;
        assert_eq!(
            (counts.build_attempts, counts.deliveries),
            (Some(1), Some(1))
        );
        assert_eq!(remote.lock().unwrap().1, 1);
        assert_eq!(
            continuations.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "Applying reconciliation must not enter the legacy spawn/task-PR continuation"
        );
        assert_eq!(reconciliations.load(std::sync::atomic::Ordering::SeqCst), 1);
        {
            let integrated_and_released = updates.lock().unwrap();
            assert_eq!(
                integrated_and_released.len(),
                2,
                "TaskIntegrated must update the source and release its dependent once"
            );
            assert!(
                integrated_and_released
                    .iter()
                    .any(|payload| payload["task"]["id"] == task.id)
            );
            assert!(
                integrated_and_released
                    .iter()
                    .any(|payload| payload["task"]["id"] == dependent.id)
            );
        }
        assert_eq!(
            boundary_operations.operations_since(boundary_checkpoint),
            vec![
                BoundaryOperation::CapabilityProbe,
                BoundaryOperation::ResolveTaskActiveAttempt,
                BoundaryOperation::DirectAppend,
            ]
        );
        let replay = crate::dispatch::task_dispatch::continue_ready_dispatch(
            db.clone(),
            &tasks,
            &task.id,
            || {
                let reconciliations = reconciliations.clone();
                async move {
                    reconciliations.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    panic!("Applied must not re-enter engine")
                }
            },
            || {
                let continuations = continuations.clone();
                async move {
                    continuations.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(
            replay,
            crate::dispatch::task_dispatch::ReadyDispatchContinuation::Settled
        );
        assert_eq!(tasks.get(&task.id).await.unwrap().unwrap().status, "closed");
        assert_eq!(
            djinn_db::test_support::direct_delivery_matrix_counts_for_test(&db).await,
            counts
        );
        assert_eq!(remote.lock().unwrap().1, 1);
        assert_eq!(
            reconciliations.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "Applied replay cannot re-enter reconciliation/spawn"
        );
        assert_eq!(
            updates.lock().unwrap().len(),
            2,
            "Applied replay cannot repeat integration or dependent release"
        );
        assert_eq!(
            continuations.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "Applied replay must not enter the legacy spawn/task-PR continuation"
        );
        assert_eq!(fixture.delivery_generation, Some(1));
    }

    #[tokio::test]
    async fn ready_dispatch_conflict_generation_never_spawns_or_reconciles() {
        use djinn_core::events::EventBus;
        use djinn_db::test_support::seed_direct_delivery_liveness_fixture_for_test;
        use djinn_db::{Database, EpicRepository, TaskRepository};

        let db = Database::open_in_memory().unwrap();
        let epic = EpicRepository::new(db.clone(), EventBus::noop())
            .create("conflict", "", "", "", "", None)
            .await
            .unwrap();
        let tasks = TaskRepository::new(db.clone(), EventBus::noop());
        let task = tasks
            .create(
                &epic.id,
                "conflict",
                "",
                "",
                "task",
                0,
                "",
                Some("approved"),
            )
            .await
            .unwrap();
        seed_direct_delivery_liveness_fixture_for_test(&db, &epic.id, &task.id, Some("conflict"))
            .await;
        let counts_before =
            djinn_db::test_support::direct_delivery_matrix_counts_for_test(&db).await;
        let continuations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let conflict_continuations = continuations.clone();

        let decision = crate::dispatch::task_dispatch::continue_ready_dispatch(
            db.clone(),
            &tasks,
            &task.id,
            || async { panic!("immutable Conflict must not spawn or reconcile") },
            || async move {
                conflict_continuations.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(
            decision,
            crate::dispatch::task_dispatch::ReadyDispatchContinuation::Settled
        );
        assert_eq!(
            tasks.get(&task.id).await.unwrap().unwrap().status,
            "approved"
        );
        assert_eq!(
            continuations.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "Conflict must not enter the legacy spawn/task-PR continuation"
        );
        assert_eq!(
            djinn_db::test_support::direct_delivery_matrix_counts_for_test(&db).await,
            counts_before,
            "Conflict must not mutate immutable delivery state"
        );
    }

    /// Independent, purpose-specific effect counters for one ready-dispatch
    /// call. Nothing here is an aggregate: each field is observed at its own
    /// production boundary, so a replay that skipped one effect but repeated
    /// another cannot hide inside a single total.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct ReadyDispatchEffectCounts {
        spawn_continuations: usize,
        reconciliations: usize,
        task_pr_operations: usize,
        direct_appends: usize,
        remote_ref_pushes: usize,
        integrations: usize,
        dependent_releases: usize,
    }

    fn task_pr_operation(operation: &BoundaryOperation) -> bool {
        matches!(
            operation,
            BoundaryOperation::SupervisorPrOpen
                | BoundaryOperation::TaskPrLookup
                | BoundaryOperation::TaskPrAdopt
                | BoundaryOperation::TaskPrStatusPoll
                | BoundaryOperation::TaskPrReviewPoll
                | BoundaryOperation::TaskPrMergedPoll
                | BoundaryOperation::TaskPrInlineCleanup
                | BoundaryOperation::TaskPrStaleCleanup
                | BoundaryOperation::TaskPrCreate
                | BoundaryOperation::TaskPrMerge
                | BoundaryOperation::TaskPrAutoMerge
                | BoundaryOperation::TaskPrApproval
                | BoundaryOperation::TaskPrSignoff
                | BoundaryOperation::TaskPrCustomEnqueue
                | BoundaryOperation::AttemptPrCreateOrAdoptRequest
        )
    }

    /// A repository fixture that is already settled when ready dispatch first
    /// sees it.
    ///
    /// This is deliberately not the Applying scenario: there, the very first
    /// `continue_ready_dispatch` call performs the reconciliation that produces
    /// Applied, so "no second effect" is only ever observed on call two. Here
    /// the engine runs to `TaskIntegrated` *before* the frame is entered, so
    /// both invocations start from exact Applied plus closed and every effect
    /// count below must stay at zero from the first call onward.
    #[tokio::test]
    async fn exact_applied_closed_ready_dispatch_replays_without_any_production_effect() {
        use djinn_core::events::EventBus;
        use djinn_db::test_support::{
            direct_delivery_candidate_cardinality_for_test, direct_delivery_generations_for_test,
            direct_delivery_matrix_counts_for_test, seed_direct_delivery_liveness_fixture_for_test,
        };
        use djinn_db::{Database, EpicRepository, ProposalBuildAttemptRepository, TaskRepository};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let boundary_operations = boundary_operations_scope().await;
        let db = Database::open_in_memory().unwrap();
        let epic = EpicRepository::new(db.clone(), EventBus::noop())
            .create("applied-replay", "", "", "", "", None)
            .await
            .unwrap();

        let source_updates = Arc::new(Mutex::new(0usize));
        let dependent_updates = Arc::new(Mutex::new(0usize));
        let observed_source = source_updates.clone();
        let observed_dependent = dependent_updates.clone();
        // Keep the two counters independent: an integration that released
        // nothing and a release that integrated nothing are different failures,
        // and one combined "task updated" total cannot tell them apart.
        let source_task_id = Arc::new(Mutex::new(String::new()));
        let dependent_task_id = Arc::new(Mutex::new(String::new()));
        let source_id_for_events = source_task_id.clone();
        let dependent_id_for_events = dependent_task_id.clone();
        let observing_events = EventBus::new(move |event| {
            if event.entity_type != "task" || event.action != "updated" {
                return;
            }
            let id = event.payload["task"]["id"].as_str().unwrap_or_default();
            if id == source_id_for_events.lock().unwrap().as_str() {
                *observed_source.lock().unwrap() += 1;
            } else if id == dependent_id_for_events.lock().unwrap().as_str() {
                *observed_dependent.lock().unwrap() += 1;
            }
        });

        let tasks = TaskRepository::new(db.clone(), observing_events.clone());
        let task = tasks
            .create(&epic.id, "applied", "", "", "task", 0, "", Some("approved"))
            .await
            .unwrap();
        let dependent = tasks
            .create(&epic.id, "dependent", "", "", "task", 0, "", Some("open"))
            .await
            .unwrap();
        tasks.add_blocker(&dependent.id, &task.id).await.unwrap();
        *source_task_id.lock().unwrap() = task.id.clone();
        *dependent_task_id.lock().unwrap() = dependent.id.clone();

        let fixture = seed_direct_delivery_liveness_fixture_for_test(
            &db,
            &epic.id,
            &task.id,
            Some("applying"),
        )
        .await;
        assert_eq!(fixture.delivery_generation, Some(1));
        assert!(task.pr_url.is_none());

        let remote = Arc::new(Mutex::new(("fixture-base".to_owned(), 0usize)));
        let engine = DirectDeliveryEngine::new(
            RepositoryDeliveryLedger::new(
                db.clone(),
                ProposalBuildAttemptRepository::new(db.clone()),
                TaskRepository::new(db.clone(), observing_events),
            ),
            FixtureRemote(remote.clone()),
            FixtureBuilder,
        );

        // Reach exact Applied + closed through the real engine and the real
        // `TaskIntegrated` repository transition, OUTSIDE the ready-dispatch
        // frame. Everything after this point is replay.
        let settle = crate::dispatch::wave_dispatch::run_direct_completion(|| {
            engine.deliver(DeliverySource {
                task_id: task.id.clone(),
                delivery_generation: 1,
                transition_id: "fixture-prepare".into(),
                source_sha: "fixture-source".into(),
                normalized_patch: "fixture-patch".into(),
            })
        })
        .await
        .expect("engine must settle the fixture generation");
        assert!(
            matches!(settle, DeliveryOutcome::Integrated { .. }),
            "fixture must reach integration before the first ready-dispatch call, got {settle:?}"
        );

        let settled_task = tasks.get(&task.id).await.unwrap().unwrap();
        assert_eq!(
            (
                settled_task.status.as_str(),
                settled_task.merge_commit_sha.as_deref()
            ),
            ("closed", Some("fixture-candidate")),
            "initial state must be exact Applied plus closed, reached via TaskIntegrated"
        );
        let generations_before = direct_delivery_generations_for_test(&db, &task.id).await;
        assert_eq!(generations_before.len(), 1);
        assert_eq!(generations_before[0].state, "applied");
        assert_eq!(generations_before[0].delivery_generation, 1);
        assert_eq!(generations_before[0].candidate_sha, "fixture-candidate");
        assert!(generations_before[0].applied);
        let cardinality_before =
            direct_delivery_candidate_cardinality_for_test(&db, &task.id).await;
        assert_eq!(
            (
                cardinality_before.generations,
                cardinality_before.distinct_candidates,
                cardinality_before.distinct_build_attempts
            ),
            (1, 1, 1)
        );
        let matrix_before = direct_delivery_matrix_counts_for_test(&db).await;
        assert_eq!(
            *dependent_updates.lock().unwrap(),
            1,
            "the pre-frame integration must have released the dependent exactly once"
        );

        // Baseline every independent counter at zero for the replay window.
        let spawn_continuations = Arc::new(AtomicUsize::new(0));
        let reconciliations = Arc::new(AtomicUsize::new(0));
        *source_updates.lock().unwrap() = 0;
        *dependent_updates.lock().unwrap() = 0;
        let pushes_before = remote.lock().unwrap().1;

        for call in 1..=2 {
            let checkpoint = boundary_operations.checkpoint();
            let spawn_continuations_for_call = spawn_continuations.clone();
            let reconciliations_for_call = reconciliations.clone();
            let decision = crate::dispatch::task_dispatch::continue_ready_dispatch(
                db.clone(),
                &tasks,
                &task.id,
                || async move {
                    reconciliations_for_call.fetch_add(1, Ordering::SeqCst);
                    panic!("exact Applied must never re-enter the delivery engine")
                },
                || async move {
                    spawn_continuations_for_call.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .await
            .unwrap();
            assert_eq!(
                decision,
                crate::dispatch::task_dispatch::ReadyDispatchContinuation::Settled,
                "call {call}: exact Applied plus closed must settle"
            );

            let operations = boundary_operations.operations_since(checkpoint);
            let counts = ReadyDispatchEffectCounts {
                spawn_continuations: spawn_continuations.load(Ordering::SeqCst),
                reconciliations: reconciliations.load(Ordering::SeqCst),
                task_pr_operations: operations.iter().filter(|op| task_pr_operation(op)).count(),
                direct_appends: operations
                    .iter()
                    .filter(|op| matches!(op, BoundaryOperation::DirectAppend))
                    .count(),
                remote_ref_pushes: remote.lock().unwrap().1 - pushes_before,
                integrations: *source_updates.lock().unwrap(),
                dependent_releases: *dependent_updates.lock().unwrap(),
            };
            assert_eq!(
                counts,
                ReadyDispatchEffectCounts {
                    spawn_continuations: 0,
                    reconciliations: 0,
                    task_pr_operations: 0,
                    direct_appends: 0,
                    remote_ref_pushes: 0,
                    integrations: 0,
                    dependent_releases: 0,
                },
                "call {call}: an already-settled generation must produce no production effect"
            );
            assert_eq!(
                operations,
                vec![
                    BoundaryOperation::CapabilityProbe,
                    BoundaryOperation::ResolveTaskActiveAttempt,
                ],
                "call {call}: settlement must be decided by the canonical probe and \
                 attempt resolution alone"
            );

            assert_eq!(
                tasks.get(&task.id).await.unwrap().unwrap().status,
                "closed",
                "call {call}: closed status must survive replay"
            );
            assert_eq!(
                direct_delivery_generations_for_test(&db, &task.id).await,
                generations_before,
                "call {call}: the immutable delivery generation must be unchanged"
            );
            assert_eq!(
                direct_delivery_candidate_cardinality_for_test(&db, &task.id).await,
                cardinality_before,
                "call {call}: candidate cardinality must not grow"
            );
            assert_eq!(
                direct_delivery_matrix_counts_for_test(&db).await,
                matrix_before,
                "call {call}: attempt/ledger cardinality must not grow"
            );
        }
    }

    /// Every persisted routing case this slice covers, named by the persisted
    /// state that selects it rather than by the outcome it is expected to
    /// produce — the outcome is what the assertions have to earn.
    #[derive(Clone, Copy, Debug)]
    enum ReadyRoutingCase {
        /// Attempt-owning proposal exists but no epic carries it.
        UnresolvedOwnership,
        /// The delivery ledger relation is absent entirely.
        MissingSchema,
        /// No epoch row at all.
        MissingEpoch,
        /// An epoch row whose state string the typed contract does not define.
        UnknownEpoch,
        /// An active epoch and a resolvable attempt, but the persisted delivery
        /// generation carries a state the typed contract does not define.
        UnknownPersistedDeliveryState,
        /// An active epoch and a resolvable attempt with no ledger row at all —
        /// the canonical active-direct case.
        ActiveDirectNoLedgerRow,
        /// Epoch present and supported, but switched off.
        SupportedDisabled,
        /// Epoch active, task explicitly labelled back onto legacy delivery.
        SupportedActiveExplicitLegacy,
    }

    /// What the shared frame did, and whether anything moved underneath it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct ReadyRoutingObservation {
        continuation: Option<String>,
        errored: bool,
        legacy_continuations: usize,
        reconciliations: usize,
        task_pr_operations: usize,
        direct_appends: usize,
        remote_ref_pushes: usize,
        task_updates: usize,
    }

    /// Repository state a fail-closed decision must leave exactly as it found
    /// it. `status` is deliberately excluded: parking *is* the fail-closed
    /// action, so it is asserted separately rather than smuggled in here.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct ReadyRoutingPersistedState {
        pr_url: Option<String>,
        merge_commit_sha: Option<String>,
        task_attempts: i64,
        matrix: djinn_db::test_support::DirectDeliveryMatrixCountsForTest,
        generations: Option<Vec<djinn_db::test_support::DirectDeliveryGenerationSnapshotForTest>>,
    }

    /// Route the malformed/fail-closed and positive retained-legacy matrix
    /// through the *same* top-level frame production `dispatch_ready_tasks`
    /// calls.
    ///
    /// The pre-existing admission matrix asserts on `admit_ready_direct_delivery`
    /// directly. That proves the classifier, not the frame: a regression that
    /// classified correctly and then continued into spawn anyway would leave it
    /// green. Here `continue_ready_dispatch` is the only boundary invoked, and
    /// the legacy continuation is a real closure whose invocation is counted, so
    /// "failed closed" means the spawn path was never entered — not merely that
    /// an enum said so.
    ///
    /// Routing is established entirely by typed repository state — epoch row,
    /// resolved attempt, persisted delivery generation, task labels. `pr_url`
    /// stays null in every case below precisely so nothing can be inferred from
    /// it.
    #[tokio::test]
    async fn ready_dispatch_frame_fails_closed_and_retains_legacy_by_persisted_state() {
        use djinn_core::events::EventBus;
        use djinn_db::test_support::{
            direct_delivery_generations_if_readable_for_test,
            direct_delivery_matrix_counts_for_test, drop_table_cascade_for_test,
            remove_direct_delivery_epoch_for_test, remove_task_delivery_rows_for_test,
            seed_direct_delivery_liveness_fixture_for_test, seed_direct_delivery_proposal_for_test,
            seed_unknown_direct_delivery_epoch_for_test, seed_unknown_task_delivery_state_for_test,
            task_attempt_count_for_test,
        };
        use djinn_db::{Database, EpicRepository, TaskRepository};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let boundary_operations = boundary_operations_scope().await;

        for case in [
            ReadyRoutingCase::UnresolvedOwnership,
            ReadyRoutingCase::MissingSchema,
            ReadyRoutingCase::MissingEpoch,
            ReadyRoutingCase::UnknownEpoch,
            ReadyRoutingCase::UnknownPersistedDeliveryState,
            ReadyRoutingCase::ActiveDirectNoLedgerRow,
            ReadyRoutingCase::SupportedDisabled,
            ReadyRoutingCase::SupportedActiveExplicitLegacy,
        ] {
            let db = Database::open_in_memory().unwrap();
            let task_updates = Arc::new(Mutex::new(0usize));
            let observed_updates = task_updates.clone();
            let observing_events = EventBus::new(move |event| {
                if event.entity_type == "task" && event.action == "updated" {
                    *observed_updates.lock().unwrap() += 1;
                }
            });
            let epic = EpicRepository::new(db.clone(), EventBus::noop())
                .create("routing", "", "", "", "", None)
                .await
                .unwrap();
            let tasks = TaskRepository::new(db.clone(), observing_events);
            let task = tasks
                .create(&epic.id, "routing", "", "", "task", 0, "", Some("approved"))
                .await
                .unwrap();
            assert!(
                task.pr_url.is_none(),
                "{case:?}: routing must never have nullable PR data to infer from"
            );

            match case {
                ReadyRoutingCase::UnresolvedOwnership => {
                    // A proposal exists and is active, but no epic carries it,
                    // so `resolve_task_active_attempt` cannot reach an owner.
                    djinn_db::test_support::activate_direct_delivery_epoch_for_test(&db).await;
                    seed_direct_delivery_proposal_for_test(&db, &task.id, &task.id[..8]).await;
                }
                ReadyRoutingCase::MissingSchema => {
                    seed_direct_delivery_liveness_fixture_for_test(
                        &db,
                        &epic.id,
                        &task.id,
                        Some("applying"),
                    )
                    .await;
                    drop_table_cascade_for_test(&db, "task_deliveries").await;
                }
                ReadyRoutingCase::MissingEpoch => {
                    seed_direct_delivery_liveness_fixture_for_test(
                        &db,
                        &epic.id,
                        &task.id,
                        Some("applying"),
                    )
                    .await;
                    remove_direct_delivery_epoch_for_test(&db).await;
                }
                ReadyRoutingCase::UnknownEpoch => {
                    seed_direct_delivery_liveness_fixture_for_test(
                        &db,
                        &epic.id,
                        &task.id,
                        Some("applying"),
                    )
                    .await;
                    seed_unknown_direct_delivery_epoch_for_test(&db).await;
                }
                ReadyRoutingCase::UnknownPersistedDeliveryState => {
                    seed_direct_delivery_liveness_fixture_for_test(
                        &db,
                        &epic.id,
                        &task.id,
                        Some("applying"),
                    )
                    .await;
                    seed_unknown_task_delivery_state_for_test(&db, &task.id, "quiesced").await;
                }
                ReadyRoutingCase::ActiveDirectNoLedgerRow => {
                    seed_direct_delivery_liveness_fixture_for_test(
                        &db,
                        &epic.id,
                        &task.id,
                        Some("applying"),
                    )
                    .await;
                    let removed = remove_task_delivery_rows_for_test(&db, &task.id).await;
                    assert_eq!(
                        removed, 1,
                        "{case:?}: the no-ledger-row case must actually have removed a row"
                    );
                }
                ReadyRoutingCase::SupportedDisabled => {
                    seed_direct_delivery_liveness_fixture_for_test(
                        &db,
                        &epic.id,
                        &task.id,
                        Some("applying"),
                    )
                    .await;
                    djinn_db::test_support::disable_direct_delivery_epoch_for_test(&db).await;
                }
                ReadyRoutingCase::SupportedActiveExplicitLegacy => {
                    seed_direct_delivery_liveness_fixture_for_test(
                        &db,
                        &epic.id,
                        &task.id,
                        Some("applying"),
                    )
                    .await;
                    tasks
                        .update_labels(&task.id, &format!(r#"["{LEGACY_DELIVERY_LABEL}"]"#))
                        .await
                        .unwrap();
                }
            }

            async fn persisted(
                db: &Database,
                tasks: &TaskRepository,
                task_id: &str,
            ) -> (String, ReadyRoutingPersistedState) {
                let task = tasks.get(task_id).await.unwrap().unwrap();
                (
                    task.status,
                    ReadyRoutingPersistedState {
                        pr_url: task.pr_url,
                        merge_commit_sha: task.merge_commit_sha,
                        task_attempts: task_attempt_count_for_test(db, task_id).await,
                        matrix: direct_delivery_matrix_counts_for_test(db).await,
                        generations: direct_delivery_generations_if_readable_for_test(db, task_id)
                            .await,
                    },
                )
            }

            let (status_before, state_before) = persisted(&db, &tasks, &task.id).await;
            *task_updates.lock().unwrap() = 0;
            let checkpoint = boundary_operations.checkpoint();
            let legacy_continuations = Arc::new(AtomicUsize::new(0));
            let reconciliations = Arc::new(AtomicUsize::new(0));
            let remote_pushes = Arc::new(AtomicUsize::new(0));
            let legacy_for_call = legacy_continuations.clone();
            let reconcile_for_call = reconciliations.clone();
            let pushes_for_call = remote_pushes.clone();

            let decision = crate::dispatch::task_dispatch::continue_ready_dispatch(
                db.clone(),
                &tasks,
                &task.id,
                || async move {
                    reconcile_for_call.fetch_add(1, Ordering::SeqCst);
                    // A reconcile that is reached at all in this matrix is the
                    // failure; make it push before returning so a wrongly
                    // admitted case cannot look inert.
                    pushes_for_call.fetch_add(1, Ordering::SeqCst);
                    Ok(DeliveryOutcome::Integrated {
                        candidate_sha: "unexpected-reconcile".into(),
                    })
                },
                || async move {
                    legacy_for_call.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .await;

            let operations = boundary_operations.operations_since(checkpoint);
            let observation = ReadyRoutingObservation {
                continuation: decision
                    .as_ref()
                    .ok()
                    .map(|decision| format!("{decision:?}")),
                errored: decision.is_err(),
                legacy_continuations: legacy_continuations.load(Ordering::SeqCst),
                reconciliations: reconciliations.load(Ordering::SeqCst),
                task_pr_operations: operations.iter().filter(|op| task_pr_operation(op)).count(),
                direct_appends: operations
                    .iter()
                    .filter(|op| matches!(op, BoundaryOperation::DirectAppend))
                    .count(),
                remote_ref_pushes: remote_pushes.load(Ordering::SeqCst),
                task_updates: *task_updates.lock().unwrap(),
            };
            let (status_after, state_after) = persisted(&db, &tasks, &task.id).await;

            match case {
                // ---- fail closed -------------------------------------------
                ReadyRoutingCase::UnresolvedOwnership
                | ReadyRoutingCase::MissingSchema
                | ReadyRoutingCase::MissingEpoch
                | ReadyRoutingCase::UnknownEpoch => {
                    assert_eq!(
                        observation,
                        ReadyRoutingObservation {
                            continuation: Some("Parked".to_owned()),
                            errored: false,
                            legacy_continuations: 0,
                            reconciliations: 0,
                            task_pr_operations: 0,
                            direct_appends: 0,
                            remote_ref_pushes: 0,
                            // The single update is the fail-closed park itself.
                            task_updates: 1,
                        },
                        "{case:?}: must park before any spawn, task-PR, append, or push effect"
                    );
                    assert_eq!(
                        status_after, "needs_lead_intervention",
                        "{case:?}: failing closed must escalate rather than dispatch"
                    );
                    assert_ne!(
                        status_before, "needs_lead_intervention",
                        "{case:?}: the fixture must not start already parked"
                    );
                    assert_eq!(
                        state_after, state_before,
                        "{case:?}: parking must not touch PR identity, integration, task \
                         attempts, or the delivery ledger"
                    );
                }
                // ---- fail closed, as an error ------------------------------
                ReadyRoutingCase::UnknownPersistedDeliveryState => {
                    assert_eq!(
                        observation,
                        ReadyRoutingObservation {
                            continuation: None,
                            errored: true,
                            legacy_continuations: 0,
                            reconciliations: 0,
                            task_pr_operations: 0,
                            direct_appends: 0,
                            remote_ref_pushes: 0,
                            task_updates: 0,
                        },
                        "{case:?}: an undefined persisted contract state must abort the pass \
                         without spawning, reconciling, or mutating anything"
                    );
                    assert_eq!(
                        status_after, status_before,
                        "{case:?}: an unreadable ledger row must not move the task at all"
                    );
                    assert_eq!(state_after, state_before);
                }
                // ---- positive retained legacy / canonical direct dispatch ---
                ReadyRoutingCase::ActiveDirectNoLedgerRow
                | ReadyRoutingCase::SupportedDisabled
                | ReadyRoutingCase::SupportedActiveExplicitLegacy => {
                    assert_eq!(
                        observation,
                        ReadyRoutingObservation {
                            continuation: Some("LegacyDispatch(())".to_owned()),
                            errored: false,
                            legacy_continuations: 1,
                            reconciliations: 0,
                            task_pr_operations: 0,
                            direct_appends: 0,
                            remote_ref_pushes: 0,
                            task_updates: 0,
                        },
                        "{case:?}: must positively reach the retained legacy/dispatch \
                         continuation exactly once"
                    );
                    assert_eq!(
                        status_after, status_before,
                        "{case:?}: entering the continuation is not itself a task mutation"
                    );
                    assert_eq!(state_after, state_before);
                }
            }

            // Routing evidence: which typed reads the frame actually performed.
            // SupportedDisabled short-circuits at the epoch, so it never
            // resolves an attempt; every other supported case must.
            let probes = operations
                .iter()
                .filter(|op| matches!(op, BoundaryOperation::CapabilityProbe))
                .count();
            assert_eq!(probes, 1, "{case:?}: the epoch must be probed exactly once");
            let resolutions = operations
                .iter()
                .filter(|op| matches!(op, BoundaryOperation::ResolveTaskActiveAttempt))
                .count();
            let expected_resolutions = match case {
                ReadyRoutingCase::MissingSchema
                | ReadyRoutingCase::MissingEpoch
                | ReadyRoutingCase::UnknownEpoch
                | ReadyRoutingCase::SupportedDisabled
                | ReadyRoutingCase::SupportedActiveExplicitLegacy => 0,
                ReadyRoutingCase::UnresolvedOwnership
                | ReadyRoutingCase::UnknownPersistedDeliveryState
                | ReadyRoutingCase::ActiveDirectNoLedgerRow => 1,
            };
            assert_eq!(
                resolutions, expected_resolutions,
                "{case:?}: ownership resolution must be driven by persisted epoch/label state"
            );
        }
    }
}
