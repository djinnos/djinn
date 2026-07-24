//! Revisioned private Unix control protocol for catalog service wrappers.
//!
//! Values returned by `CreateFresh` are execution-only and must never be put in
//! identity or evidence structures.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub const CONTROL_PROTOCOL_REVISION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceLease {
    pub lease_id: String,
    pub environment: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceProvisioningPhase {
    /// Catalog material could not be resolved into a safe lifecycle endpoint.
    Resolve,
    /// The fixed-port attempt proxy/session could not be established.
    Proxy,
    Create,
    Readiness,
    Teardown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceProvisioningCode {
    Unavailable,
    ProtocolMismatch,
    Timeout,
    Rejected,
    InvalidResponse,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceProvisioningError {
    pub phase: ServiceProvisioningPhase,
    pub code: ServiceProvisioningCode,
}

pub trait CatalogServiceProvisioner: Send + Sync {
    fn preset_id(&self) -> &str;
    fn create<'a>(
        &'a self,
        attempt: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<ServiceLease, ServiceProvisioningError>> + Send + 'a>>;
    fn ready<'a>(
        &'a self,
        lease: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), ServiceProvisioningError>> + Send + 'a>>;
    fn delete<'a>(
        &'a self,
        lease: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), ServiceProvisioningError>> + Send + 'a>>;
}
pub type ServiceProvisioners = Vec<Arc<dyn CatalogServiceProvisioner>>;

/// Create and ready leases in canonical preset order, rolling back in reverse
/// order if create or readiness fails.
pub async fn create_ready_leases(
    provisioners: &[Arc<dyn CatalogServiceProvisioner>],
    attempt: &str,
) -> Result<Vec<(Arc<dyn CatalogServiceProvisioner>, ServiceLease)>, ServiceProvisioningError> {
    let mut ordered = provisioners.to_vec();
    ordered.sort_by(|left, right| left.preset_id().cmp(right.preset_id()));
    let mut leases = Vec::with_capacity(ordered.len());
    for provisioner in ordered {
        let lease = match provisioner.create(attempt).await {
            Ok(lease) => lease,
            Err(error) => {
                // A failed rollback is more important than the create failure:
                // returning it ensures callers never regard an attempt with a
                // surviving tenant as eligible.
                return match delete_leases(&leases).await {
                    Ok(()) => Err(error),
                    Err(teardown) => Err(teardown),
                };
            }
        };
        if let Err(error) = provisioner.ready(&lease.lease_id).await {
            let mut rollback = leases;
            rollback.push((Arc::clone(&provisioner), lease));
            return match delete_leases(&rollback).await {
                Ok(()) => Err(error),
                Err(teardown) => Err(teardown),
            };
        }
        leases.push((provisioner, lease));
    }
    Ok(leases)
}

/// Delete every lease even if a preceding delete fails.
pub async fn delete_leases(
    leases: &[(Arc<dyn CatalogServiceProvisioner>, ServiceLease)],
) -> Result<(), ServiceProvisioningError> {
    let mut failure = None;
    for (provisioner, lease) in leases.iter().rev() {
        if let Err(error) = provisioner.delete(&lease.lease_id).await {
            failure.get_or_insert(error);
        }
    }
    failure.map_or(Ok(()), Err)
}

#[derive(Clone)]
pub struct UnixCatalogServiceProvisioner {
    preset_id: String,
    socket: PathBuf,
    exported: Vec<String>,
}
impl UnixCatalogServiceProvisioner {
    pub fn new(preset_id: String, socket: PathBuf, mut exported: Vec<String>) -> Self {
        exported.sort();
        Self {
            preset_id,
            socket,
            exported,
        }
    }
    async fn call(
        &self,
        request: Request,
        phase: ServiceProvisioningPhase,
    ) -> Result<Response, ServiceProvisioningError> {
        let operation = async {
            let stream = UnixStream::connect(&self.socket)
                .await
                .map_err(|_| err(phase, ServiceProvisioningCode::Unavailable))?;
            let (read, mut write) = stream.into_split();
            let body = serde_json::to_vec(&request)
                .map_err(|_| err(phase, ServiceProvisioningCode::InvalidResponse))?;
            write
                .write_all(&body)
                .await
                .map_err(|_| err(phase, ServiceProvisioningCode::Unavailable))?;
            write
                .write_all(b"\n")
                .await
                .map_err(|_| err(phase, ServiceProvisioningCode::Unavailable))?;
            let mut line = String::new();
            BufReader::new(read)
                .read_line(&mut line)
                .await
                .map_err(|_| err(phase, ServiceProvisioningCode::Unavailable))?;
            serde_json::from_str(&line)
                .map_err(|_| err(phase, ServiceProvisioningCode::InvalidResponse))
        };
        tokio::time::timeout(Duration::from_secs(15), operation)
            .await
            .map_err(|_| err(phase, ServiceProvisioningCode::Timeout))?
    }
}
fn err(phase: ServiceProvisioningPhase, code: ServiceProvisioningCode) -> ServiceProvisioningError {
    ServiceProvisioningError { phase, code }
}
impl CatalogServiceProvisioner for UnixCatalogServiceProvisioner {
    fn preset_id(&self) -> &str {
        &self.preset_id
    }
    fn create<'a>(
        &'a self,
        attempt: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<ServiceLease, ServiceProvisioningError>> + Send + 'a>>
    {
        Box::pin(async move {
            match self
                .call(
                    Request::CreateFresh {
                        revision: 1,
                        attempt_id: attempt.into(),
                        lease_nonce: format!("lease-{attempt}"),
                    },
                    ServiceProvisioningPhase::Create,
                )
                .await?
            {
                Response::Created {
                    revision,
                    lease_id,
                    environment,
                } if revision == 1
                    && environment
                        .keys()
                        .all(|name| self.exported.binary_search(name).is_ok()) =>
                {
                    Ok(ServiceLease {
                        lease_id,
                        environment,
                    })
                }
                Response::Created { revision, .. } | Response::Error { revision, .. }
                    if revision != CONTROL_PROTOCOL_REVISION => Err(err(
                    ServiceProvisioningPhase::Create,
                    ServiceProvisioningCode::ProtocolMismatch,
                )),
                Response::Error { .. } => Err(err(
                    ServiceProvisioningPhase::Create,
                    ServiceProvisioningCode::Rejected,
                )),
                _ => Err(err(
                    ServiceProvisioningPhase::Create,
                    ServiceProvisioningCode::InvalidResponse,
                )),
            }
        })
    }
    fn ready<'a>(
        &'a self,
        lease: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), ServiceProvisioningError>> + Send + 'a>> {
        Box::pin(async move {
            match self
                .call(
                    Request::Ready {
                        revision: 1,
                        lease_id: lease.into(),
                    },
                    ServiceProvisioningPhase::Readiness,
                )
                .await?
            {
                Response::Ready { revision } if revision == CONTROL_PROTOCOL_REVISION => Ok(()),
                Response::Ready { revision } | Response::Error { revision, .. }
                    if revision != CONTROL_PROTOCOL_REVISION => Err(err(
                    ServiceProvisioningPhase::Readiness,
                    ServiceProvisioningCode::ProtocolMismatch,
                )),
                Response::Error { .. } => Err(err(
                    ServiceProvisioningPhase::Readiness,
                    ServiceProvisioningCode::Rejected,
                )),
                _ => Err(err(
                    ServiceProvisioningPhase::Readiness,
                    ServiceProvisioningCode::InvalidResponse,
                )),
            }
        })
    }
    fn delete<'a>(
        &'a self,
        lease: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), ServiceProvisioningError>> + Send + 'a>> {
        Box::pin(async move {
            match self
                .call(
                    Request::Delete {
                        revision: 1,
                        lease_id: lease.into(),
                    },
                    ServiceProvisioningPhase::Teardown,
                )
                .await?
            {
                Response::Deleted { revision } if revision == CONTROL_PROTOCOL_REVISION => Ok(()),
                Response::Deleted { revision } | Response::Error { revision, .. }
                    if revision != CONTROL_PROTOCOL_REVISION => Err(err(
                    ServiceProvisioningPhase::Teardown,
                    ServiceProvisioningCode::ProtocolMismatch,
                )),
                Response::Error { .. } => Err(err(
                    ServiceProvisioningPhase::Teardown,
                    ServiceProvisioningCode::Rejected,
                )),
                _ => Err(err(
                    ServiceProvisioningPhase::Teardown,
                    ServiceProvisioningCode::InvalidResponse,
                )),
            }
        })
    }
}
/// Revision-1 newline-delimited JSON request shared with catalog wrapper servers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum Request {
    CreateFresh {
        revision: u32,
        attempt_id: String,
        lease_nonce: String,
    },
    Ready {
        revision: u32,
        lease_id: String,
    },
    Delete {
        revision: u32,
        lease_id: String,
    },
}
/// Revision-1 newline-delimited JSON response shared with catalog wrapper servers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    Created {
        revision: u32,
        lease_id: String,
        environment: BTreeMap<String, String>,
    },
    Ready {
        revision: u32,
    },
    Deleted {
        revision: u32,
    },
    Error {
        revision: u32,
        code: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FakeProvisioner {
        id: &'static str,
        fail_create: bool,
        fail_ready: bool,
        fail_delete: bool,
        events: Arc<Mutex<Vec<String>>>,
    }

    impl FakeProvisioner {
        fn failure(phase: ServiceProvisioningPhase) -> ServiceProvisioningError {
            ServiceProvisioningError {
                phase,
                code: ServiceProvisioningCode::Rejected,
            }
        }
    }

    impl CatalogServiceProvisioner for FakeProvisioner {
        fn preset_id(&self) -> &str { self.id }

        fn create<'a>(&'a self, _: &'a str) -> Pin<Box<dyn Future<Output = Result<ServiceLease, ServiceProvisioningError>> + Send + 'a>> {
            Box::pin(async move {
                self.events.lock().unwrap().push(format!("create:{}", self.id));
                if self.fail_create { return Err(Self::failure(ServiceProvisioningPhase::Create)); }
                Ok(ServiceLease { lease_id: self.id.into(), environment: BTreeMap::new() })
            })
        }

        fn ready<'a>(&'a self, _: &'a str) -> Pin<Box<dyn Future<Output = Result<(), ServiceProvisioningError>> + Send + 'a>> {
            Box::pin(async move {
                self.events.lock().unwrap().push(format!("ready:{}", self.id));
                if self.fail_ready { Err(Self::failure(ServiceProvisioningPhase::Readiness)) } else { Ok(()) }
            })
        }

        fn delete<'a>(&'a self, _: &'a str) -> Pin<Box<dyn Future<Output = Result<(), ServiceProvisioningError>> + Send + 'a>> {
            Box::pin(async move {
                self.events.lock().unwrap().push(format!("delete:{}", self.id));
                if self.fail_delete { Err(Self::failure(ServiceProvisioningPhase::Teardown)) } else { Ok(()) }
            })
        }
    }

    #[tokio::test]
    async fn create_failure_rolls_back_and_surfaces_teardown_failure() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let postgres = Arc::new(FakeProvisioner { id: "postgres", fail_create: false, fail_ready: false, fail_delete: true, events: Arc::clone(&events) });
        let redis = Arc::new(FakeProvisioner { id: "redis", fail_create: true, fail_ready: false, fail_delete: false, events: Arc::clone(&events) });
        let result = create_ready_leases(&[redis, postgres], "attempt").await;
        assert_eq!(
            result.err().expect("must fail").phase,
            ServiceProvisioningPhase::Teardown
        );
        assert_eq!(*events.lock().unwrap(), vec!["create:postgres", "ready:postgres", "create:redis", "delete:postgres"]);
    }

    #[tokio::test]
    async fn readiness_failure_deletes_current_then_prior_leases_in_reverse_order() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let postgres = Arc::new(FakeProvisioner { id: "postgres", fail_create: false, fail_ready: false, fail_delete: false, events: Arc::clone(&events) });
        let redis = Arc::new(FakeProvisioner { id: "redis", fail_create: false, fail_ready: true, fail_delete: false, events: Arc::clone(&events) });
        let result = create_ready_leases(&[redis, postgres], "attempt").await;
        assert_eq!(
            result.err().expect("must fail").phase,
            ServiceProvisioningPhase::Readiness
        );
        assert_eq!(*events.lock().unwrap(), vec!["create:postgres", "ready:postgres", "create:redis", "ready:redis", "delete:redis", "delete:postgres"]);
    }
}
