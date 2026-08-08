//! Typed read projections for durable feedback-refinement handoff state.
//!
//! These reads keep lifecycle assertions and operational diagnostics behind
//! the repository boundary rather than requiring control-plane consumers to
//! know the persistence schema.

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::repositories::proposal::ProposalRepository;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct FeedbackRefinementActiveBoundary {
    pub run_id: String,
    pub intent_id: String,
    pub generation: i32,
    pub source_captures: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct PendingFeedbackRefinementState {
    pub pending_members: i64,
    pub pending_owners: i64,
    pub source_captures: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct FeedbackRefinementLifecycleState {
    pub runs: i64,
    pub running: i64,
    pub intents: i64,
    pub objections: i64,
    pub injections: i64,
    pub immutable_generations: i64,
    pub immutable_sources: i64,
    pub pending: i64,
    pub pending_owners: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct FeedbackRefinementAdmittedState {
    pub admitted: i64,
    pub admitted_owners: i64,
    pub successor_generation: Option<i32>,
}

impl ProposalRepository {
    /// Observe the live run selected by an initial feedback boundary together
    /// with the boundary's immutable source-capture cardinality.
    pub async fn load_feedback_refinement_active_boundary(
        &self,
        proposal_id: &str,
        source_feedback_id: &str,
    ) -> Result<Option<FeedbackRefinementActiveBoundary>> {
        self.db().ensure_initialized().await?;
        Ok(sqlx::query_as(
            "SELECT r.id AS run_id, i.id AS intent_id, r.generation, \
                    (SELECT count(*) FROM proposal_feedback_refinement_sources \
                     WHERE source_feedback_id=$2) AS source_captures \
             FROM refinement_runs r \
             JOIN refinement_dispatch_intents i ON i.run_id=r.id \
             WHERE r.proposal_id=$1 AND r.state='running' AND i.state='pending'",
        )
        .bind(proposal_id)
        .bind(source_feedback_id)
        .fetch_optional(self.db().pool())
        .await?)
    }

    /// Observe a proposal's pending handoff cohort and one feedback boundary's
    /// source-capture count at the same persisted lifecycle point.
    pub async fn load_pending_feedback_refinement_state(
        &self,
        proposal_id: &str,
        source_feedback_id: &str,
    ) -> Result<PendingFeedbackRefinementState> {
        self.db().ensure_initialized().await?;
        Ok(sqlx::query_as(
            "SELECT \
               (SELECT count(*) FROM pending_feedback_refinement_handoffs \
                WHERE proposal_id=$1 AND state='pending') AS pending_members, \
               (SELECT count(*) FROM pending_feedback_refinement_handoffs \
                WHERE proposal_id=$1 AND state='pending' AND cohort_owner) AS pending_owners, \
               (SELECT count(*) FROM proposal_feedback_refinement_sources \
                WHERE source_feedback_id=$2) AS source_captures",
        )
        .bind(proposal_id)
        .bind(source_feedback_id)
        .fetch_one(self.db().pool())
        .await?)
    }

    /// Observe cardinalities that prove a feedback handoff drained exactly
    /// once, including injection rows and their immutable root generations.
    pub async fn load_feedback_refinement_lifecycle_state(
        &self,
        proposal_id: &str,
        first_source_feedback_id: &str,
        second_source_feedback_id: &str,
    ) -> Result<FeedbackRefinementLifecycleState> {
        self.db().ensure_initialized().await?;
        Ok(sqlx::query_as(
            "SELECT \
               (SELECT count(*) FROM refinement_runs WHERE proposal_id=$1) AS runs, \
               (SELECT count(*) FROM refinement_runs WHERE proposal_id=$1 AND state='running') AS running, \
               (SELECT count(*) FROM refinement_dispatch_intents i JOIN refinement_runs r ON r.id=i.run_id WHERE r.proposal_id=$1) AS intents, \
               (SELECT count(*) FROM proposal_debate_trail WHERE proposal_id=$1 AND kind='human_feedback') AS objections, \
               (SELECT count(*) FROM proposal_feedback_refinement_injections WHERE proposal_id=$1) AS injections, \
               (SELECT count(DISTINCT (root_feedback_id, generation)) FROM proposal_feedback_refinement_injections WHERE proposal_id=$1) AS immutable_generations, \
               (SELECT count(*) FROM proposal_feedback_refinement_sources WHERE source_feedback_id IN ($2,$3)) AS immutable_sources, \
               (SELECT count(*) FROM pending_feedback_refinement_handoffs WHERE proposal_id=$1 AND state='pending') AS pending, \
               (SELECT count(*) FROM pending_feedback_refinement_handoffs WHERE proposal_id=$1 AND state='pending' AND cohort_owner) AS pending_owners",
        )
        .bind(proposal_id)
        .bind(first_source_feedback_id)
        .bind(second_source_feedback_id)
        .fetch_one(self.db().pool())
        .await?)
    }

    /// Observe admitted handoff ownership and the successor generation that
    /// captured a particular feedback boundary.
    pub async fn load_feedback_refinement_admitted_state(
        &self,
        proposal_id: &str,
        source_feedback_id: &str,
    ) -> Result<FeedbackRefinementAdmittedState> {
        self.db().ensure_initialized().await?;
        Ok(sqlx::query_as(
            "SELECT \
               (SELECT count(*) FROM pending_feedback_refinement_handoffs WHERE proposal_id=$1 AND state='admitted') AS admitted, \
               (SELECT count(*) FROM pending_feedback_refinement_handoffs WHERE proposal_id=$1 AND state='admitted' AND cohort_owner) AS admitted_owners, \
               (SELECT r.generation FROM proposal_feedback_refinement_sources s \
                 JOIN pending_feedback_refinement_handoffs h ON h.boundary_feedback_id=s.source_feedback_id \
                 JOIN refinement_runs r ON r.id=h.successor_run_id \
                 WHERE s.source_feedback_id=$2) AS successor_generation",
        )
        .bind(proposal_id)
        .bind(source_feedback_id)
        .fetch_one(self.db().pool())
        .await?)
    }
}
