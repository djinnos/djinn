//! Coordinator adapter joining Kubernetes graph warming to the one v1 lease FIFO.
//!
//! This is intentionally an adapter, not another admission repository or cap.
//! Both task invocation and graph warming call the same `BuildLeaseService`.

use std::sync::Arc;

use async_trait::async_trait;
use djinn_k8s::{GraphWarmLease, GraphWarmLeaseError, GraphWarmLeaseGrant};
use djinn_supervisor::services::{
    GraphWarmLeaseIdentity, LeaseBindRequest, LeaseDeadlines, LeaseFencingToken, LeaseGrantRequest,
    LeaseIdentity, LeaseQueueRequest, LeaseResult, LeaseState, LeaseStatusRequest,
};

use crate::build_lease::BuildLeaseService;

/// Kubernetes-facing view of the coordinator-owned durable lease service.
/// Keep one instance of this adapter beside the task consumers that already
/// share `service`; constructing it never constructs a cap or repository.
pub struct BuildLeaseGraphWarmAdapter {
    service: Arc<BuildLeaseService>,
}

impl BuildLeaseGraphWarmAdapter {
    #[must_use]
    pub fn new(service: Arc<BuildLeaseService>) -> Self {
        Self { service }
    }
}

fn unavailable(result: LeaseResult) -> GraphWarmLeaseError {
    match result {
        LeaseResult::Queued(_) => GraphWarmLeaseError::Queued,
        LeaseResult::LeaseWaitTimeout { .. } => GraphWarmLeaseError::Timeout,
        LeaseResult::LeaseUnavailable => GraphWarmLeaseError::Unavailable,
        other => GraphWarmLeaseError::Rejected(format!("unexpected lease result: {other:?}")),
    }
}

#[async_trait]
impl GraphWarmLease for BuildLeaseGraphWarmAdapter {
    async fn acquire(
        &self,
        identity: GraphWarmLeaseIdentity,
        deadlines: LeaseDeadlines,
    ) -> Result<GraphWarmLeaseGrant, GraphWarmLeaseError> {
        let lease_identity = LeaseIdentity::GraphWarm(identity.clone());
        let grant = match self
            .service
            .queue(LeaseQueueRequest {
                identity: lease_identity.clone(),
                deadlines,
            })
            .await
        {
            LeaseResult::Granted(grant) => grant,
            other => return Err(unavailable(other)),
        };

        // This acknowledgement is the create fence. A queue grant alone is not
        // permission for Kubernetes POST because a recovered coordinator must
        // be able to distinguish Granted from an attempted launch.
        match self
            .service
            .grant(LeaseGrantRequest {
                identity: lease_identity,
                fencing_token: grant.fencing_token.clone(),
            })
            .await
        {
            LeaseResult::Status(status)
                if matches!(status.state, LeaseState::Launching | LeaseState::Bound)
                    && status.fencing_token == Some(grant.fencing_token.clone()) =>
            {
                Ok(GraphWarmLeaseGrant { identity, grant })
            }
            other => Err(unavailable(other)),
        }
    }

    async fn bind(
        &self,
        identity: &GraphWarmLeaseIdentity,
        fencing_token: LeaseFencingToken,
        pod_uid: String,
    ) -> Result<(), GraphWarmLeaseError> {
        let lease_identity = LeaseIdentity::GraphWarm(identity.clone());
        let result = self
            .service
            .bind(LeaseBindRequest {
                identity: lease_identity.clone(),
                fencing_token: fencing_token.clone(),
                pod_uid: pod_uid.clone(),
            })
            .await;
        if matches!(result, LeaseResult::Bound(_)) {
            return Ok(());
        }

        // A lost bind response is indistinguishable from an unavailable write
        // to this caller. Status is the durable recovery source; it also makes
        // same-UID bind replay harmless while rejecting a different UID.
        match self
            .service
            .status(LeaseStatusRequest {
                identity: lease_identity,
            })
            .await
        {
            LeaseResult::Status(status)
                if status.state == LeaseState::Bound
                    && status.fencing_token == Some(fencing_token)
                    && status.pod_uid.as_deref() == Some(pod_uid.as_str()) =>
            {
                Ok(())
            }
            other => Err(unavailable(other)),
        }
    }
}
