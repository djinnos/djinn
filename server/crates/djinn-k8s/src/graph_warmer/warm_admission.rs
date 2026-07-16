use async_trait::async_trait;

/// Immutable identity for one admission-controlled warm Job.
///
/// The caller fixes all values before reserving capacity. `object_name` is the
/// deterministic Kubernetes Job name for this generation, never inferred from
/// a later dispatch attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WarmAdmissionRequest {
    /// Admission namespace/domain that owns this work identity.
    pub domain: String,
    /// Stable identity of the work being warmed.
    pub work_id: String,
    /// Monotonically increasing incarnation of `work_id`.
    pub generation: i64,
    /// Deterministic Kubernetes Job name for this generation.
    pub object_name: String,
}

/// Opaque, admission-controller-issued capability for one warm lifecycle.
///
/// Callers can retain, clone, and return a permit to its issuer, but cannot
/// inspect or manufacture ledger identity from its private token. Controllers
/// must recognize only permits they issued.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct WarmAdmissionPermit {
    token: uuid::Uuid,
}

impl WarmAdmissionPermit {
    /// Create an opaque token for an admission implementation's private state.
    ///
    /// This does not grant admission. Implementations issue it only after
    /// recording their own reservation.
    #[must_use]
    pub fn new() -> Self {
        Self {
            token: uuid::Uuid::now_v7(),
        }
    }
}

impl Default for WarmAdmissionPermit {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for WarmAdmissionPermit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("WarmAdmissionPermit(..)")
    }
}

/// Durable lifecycle facts reported for an admitted warm Job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WarmAdmissionTransition {
    /// The durable create intent has been recorded before Kubernetes POST.
    CreateStarted,
    /// Kubernetes confirmed the object exists and assigned this UID.
    Live { uid: String },
    /// The create result is ambiguous; retain occupancy and diagnostic detail.
    CreateUnknown { diagnostic: String },
    /// Kubernetes definitively rejected or failed the create operation.
    DefinitiveFailure { diagnostic: String },
    /// The observed Job reached a terminal state with this Kubernetes UID.
    Terminal { uid: String },
}

/// Failure returned by a [`WarmAdmission`] implementation.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WarmAdmissionError {
    /// Policy or capacity denied the requested admission.
    #[error("warm admission denied: {diagnostic}")]
    Denied { diagnostic: String },
    /// The admission ledger could not durably process the operation.
    #[error("warm admission unavailable: {diagnostic}")]
    Unavailable { diagnostic: String },
    /// A permit was not issued by this controller or is no longer valid.
    #[error("warm admission permit is not recognized")]
    UnknownPermit,
}

/// Coordinator-owned admission boundary for graph warm Jobs.
///
/// `djinn-k8s` owns only this data-only protocol. A higher crate may implement
/// it with a durable ledger and inject it into `K8sGraphWarmer` without a
/// reverse crate dependency.
#[async_trait]
pub trait WarmAdmission: Send + Sync {
    /// Reserve admission for a deterministic warm Job identity.
    async fn admit(
        &self,
        request: WarmAdmissionRequest,
    ) -> Result<WarmAdmissionPermit, WarmAdmissionError>;

    /// Persist a lifecycle fact for a permit previously returned by `admit`.
    async fn transition(
        &self,
        permit: &WarmAdmissionPermit,
        transition: WarmAdmissionTransition,
    ) -> Result<(), WarmAdmissionError>;
}
