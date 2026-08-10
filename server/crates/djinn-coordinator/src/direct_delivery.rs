//! Crash-convergent, dark direct append orchestration.
//!
//! This module intentionally has no task-PR API, uses only a non-force
//! expected-old update, and finalizes conflict generations before parking their
//! build attempt.

use std::{collections::HashSet, path::PathBuf};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use djinn_core::models::{
    DirectDeliveryParkReason, MappedHeadRetryDelivery, ReworkDelivery, TaskDeliveryIdentity,
    TaskIntegrated,
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
                    ParkReason::UnexpectedBranchHead => DirectDeliveryParkReason::UnexpectedBranchHead,
                    ParkReason::StaleHeadRetryBound => DirectDeliveryParkReason::MappedHeadRetryBound,
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
        // Select and validate the parent before recording immutable preparation
        // facts. A mapped append observed here is a valid candidate parent.
        let parent = match self.remote.observe(&attempt.branch_name).await? {
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
            match self
                .remote
                .update_expected_old(&attempt.branch_name, &parent, &candidate.candidate_sha)
                .await?
            {
                RemoteUpdate::Updated { sha } if sha == candidate.candidate_sha => {
                    let observed = self.remote.observe(&attempt.branch_name).await?;
                    if observed.as_deref() != Some(candidate.candidate_sha.as_str()) {
                        return self.park_unexpected(&attempt, &identity, observed).await;
                    }
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
                    observed_mapped_heads.insert(head.clone());
                    if observed_mapped_heads.len() > 3 {
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
                            if candidate.selected_parent_sha == head => candidate,
                        CandidateBuild::Clean(_) => {
                            return Err(anyhow!("candidate builder returned a different selected parent"));
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
        if self
            .ledger
            .integrate(TaskIntegrated::new(identity, &sha, &sha, &sha)?)
            .await?
            == LedgerResult::Stale
        {
            return Ok(DeliveryOutcome::UnexpectedHeadParked {
                observed_sha: Some(sha),
            });
        }
        Ok(DeliveryOutcome::Integrated { candidate_sha: sha })
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
    use std::sync::{Arc, Mutex};
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
        async fn update_expected_old(&self, _: &str, expected: &str, new: &str) -> Result<RemoteUpdate> {
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
        let engine = DirectDeliveryEngine::new(ledger(calls.clone()), remote, Builder { conflict: false });
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
    async fn duplicate_mapped_heads_do_not_consume_distinct_retry_budget() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (remote, updates) = remote(
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
        let mut state = ledger(calls.clone());
        state.mapped = true;
        let engine = DirectDeliveryEngine::new(state, remote, Builder { conflict: false });
        assert!(matches!(engine.deliver(source(1)).await.unwrap(), DeliveryOutcome::Integrated { .. }));
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "prepare:commit-base",
                "applying",
                "mapped-retry:1:2",
                "applying",
                "mapped-retry:2:3",
                "applying",
                "integrate:3"
            ]
        );
        assert!(updates.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn fourth_distinct_mapped_head_parks_at_retry_bound() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (remote, updates) = remote(
            vec![
                RemoteUpdate::Stale { observed_sha: Some("h4".into()) },
                RemoteUpdate::Stale { observed_sha: Some("h3".into()) },
                RemoteUpdate::Stale { observed_sha: Some("h2".into()) },
                RemoteUpdate::Stale { observed_sha: Some("h1".into()) },
            ],
            vec![Some("base")],
        );
        let mut state = ledger(calls.clone());
        state.mapped = true;
        let engine = DirectDeliveryEngine::new(state, remote, Builder { conflict: false });
        assert_eq!(
            engine.deliver(source(1)).await.unwrap(),
            DeliveryOutcome::RetryBoundParked { observed_heads: 4 }
        );
        assert_eq!(
            calls.lock().unwrap().last().unwrap(),
            "park:StaleHeadRetryBound"
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
}
