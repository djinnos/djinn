//! Coordinator adapter joining Kubernetes graph warming to the one v1 lease FIFO.
//!
//! This is intentionally an adapter, not another admission repository or cap.
//! Both task invocation and graph warming call the same `BuildLeaseService`.

use std::sync::Arc;

use async_trait::async_trait;
use djinn_db::{BuildLeaseConsumerKind, BuildLeaseState};
use djinn_k8s::{GraphWarmLease, GraphWarmLeaseError, GraphWarmLeaseGrant, GraphWarmLeaseRecovery};
use djinn_supervisor::services::{
    GraphWarmLeaseIdentity, LeaseBindRequest, LeaseDeadlines, LeaseFencingToken, LeaseGrantRequest,
    LeaseIdentity, LeaseQueueRequest, LeaseReleaseRequest, LeaseResult, LeaseState,
    LeaseStatusRequest,
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

    async fn recoverable(&self) -> Result<Vec<GraphWarmLeaseRecovery>, GraphWarmLeaseError> {
        let snapshot = self
            .service
            .recovery_snapshot()
            .await
            .map_err(|_| GraphWarmLeaseError::Unavailable)?;
        Ok(snapshot
            .rows
            .into_iter()
            .filter(|row| {
                row.key.consumer_kind == BuildLeaseConsumerKind::GraphWarm
                    && row.state != BuildLeaseState::Terminal
            })
            .filter_map(|row| {
                let mut fields = row.immutable_identity.splitn(4, ':');
                (fields.next() == Some("warm")).then_some(())?;
                Some(GraphWarmLeaseRecovery {
                    identity: GraphWarmLeaseIdentity {
                        project_id: fields.next()?.to_string(),
                        warm_request_id: fields.next()?.to_string(),
                        graph_revision: fields.next()?.to_string(),
                    },
                    fencing_token: LeaseFencingToken(row.fencing_token? as u64),
                    bound_pod_uid: row.bound_pod_uid,
                    state: match row.state {
                        BuildLeaseState::Launching => LeaseState::Launching,
                        BuildLeaseState::Bound => LeaseState::Bound,
                        BuildLeaseState::Active => LeaseState::Active,
                        BuildLeaseState::Suspect => LeaseState::Suspect,
                        _ => return None,
                    },
                    deadlines: LeaseDeadlines {
                        queue_deadline_ms: 0,
                        launch_deadline_ms: row
                            .launch_deadline
                            .as_deref()
                            .and_then(|v| {
                                time::OffsetDateTime::parse(
                                    v,
                                    &time::format_description::well_known::Rfc3339,
                                )
                                .ok()
                            })
                            .map_or(0, |v| v.unix_timestamp_nanos() as i64 / 1_000_000),
                    },
                })
            })
            .collect())
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

    async fn recoverable(&self) -> Result<Vec<GraphWarmLeaseRecovery>, GraphWarmLeaseError> {
        BuildLeaseGraphWarmAdapter::recoverable(self).await
    }
    async fn report(
        &self,
        identity: &GraphWarmLeaseIdentity,
        fencing_token: LeaseFencingToken,
        state: LeaseState,
    ) -> Result<(), GraphWarmLeaseError> {
        match self
            .service
            .report(
                LeaseIdentity::GraphWarm(identity.clone()),
                fencing_token,
                state,
            )
            .await
        {
            LeaseResult::Status(_) => Ok(()),
            other => Err(unavailable(other)),
        }
    }
    async fn release(
        &self,
        identity: &GraphWarmLeaseIdentity,
        fencing_token: LeaseFencingToken,
    ) -> Result<(), GraphWarmLeaseError> {
        match self
            .service
            .release(LeaseReleaseRequest {
                identity: LeaseIdentity::GraphWarm(identity.clone()),
                fencing_token,
                candidate_cleanup: false,
            })
            .await
        {
            LeaseResult::Released { .. } => Ok(()),
            other => Err(unavailable(other)),
        }
    }
}
