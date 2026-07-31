//! Limits-only `pods/resize` client for the `cgroup-launcher` **native sidecar**.
//!
//! Building block for proposal `3i92`'s CPU-quota cutover. Nothing calls this
//! module yet: `g8jk-3` owns post-admission ceiling capture and bootstrap,
//! `0ppk` owns the lift/drop lifecycle, and `i6ug` owns the `pods/resize` RBAC
//! rule. This slice lands the wire contract and its identity/confirmation
//! rules, nothing else.
//!
//! # The one thing this module exists to get right
//!
//! The resize target is a **native sidecar**: [`crate::job::build_job`] renders
//! it through [`crate::launcher::launcher_sidecar_container`] as a restartable
//! entry in `spec.initContainers` — *not* as a regular container. Three
//! consequences follow, and every one of them is load-bearing:
//!
//! 1. The PATCH addresses `spec.initContainers[name=cgroup-launcher]`. There is
//!    no `spec.containers[name=cgroup-launcher]` entry to address.
//! 2. Confirmation is read from
//!    `status.initContainerStatuses[name=cgroup-launcher].resources.limits.cpu`
//!    and from nowhere else. `status.containerStatuses` is never a valid
//!    confirmation source for this target — and because the worker container
//!    can carry a *coincidentally matching* limit, reading the wrong array does
//!    not merely read nothing, it reads a **false confirmation**. That is the
//!    failure this module is built to make impossible.
//! 3. Neither the HTTP status, nor the PATCH response body, nor the (mutable,
//!    immediately-updated) `spec` is confirmation. The apiserver accepts a
//!    resize *request* before the kubelet has actuated it; only the status
//!    array reports actuation.
//!
//! # Why strategic merge patch
//!
//! `initContainers` carries `patchMergeKey: name`, so a **strategic** merge
//! patch merges the single-element array into the existing entry by name. An
//! RFC 7386 merge patch would *replace the whole array*, deleting every other
//! init container. [`Patch::Strategic`] is therefore a correctness requirement,
//! not a style choice.
//!
//! # Why confirmation compares millicores, not strings
//!
//! `Quantity` is canonicalised by the apiserver on the way in: a PATCH of
//! `"4000m"` reads back as `"4"`. A string-equality confirmation would
//! therefore never confirm a whole-core target. Confirmation parses both sides
//! to millicores and compares numerically; the emitted PATCH value is rendered
//! in a single canonical form so the request itself stays byte-stable.
//!
//! # Retries are not here
//!
//! [`PodResizeClient::resize_launcher_cpu`] performs exactly one GET / PATCH /
//! GET cycle and returns a typed failure if the fresh status does not yet
//! agree. Backoff, the 30s confirmation deadline, UID fencing, and quarantine
//! belong to the lift/drop state machine in `0ppk` — putting a retry loop here
//! would mean two owners for the same deadline.

use std::collections::BTreeMap;
use std::fmt;

use async_trait::async_trait;
use k8s_openapi::api::core::v1::{Container, ContainerStatus, Pod};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::api::{Api, Patch, PatchParams};
use serde_json::{Value, json};
use tracing::debug;

use crate::launcher::LAUNCHER_CONTAINER_NAME;

/// Name of the Pod subresource that accepts limits-only in-place resizes.
pub const RESIZE_SUBRESOURCE: &str = "resize";

/// `status.conditions[].type` the kubelet sets when a resize request has been
/// accepted but cannot currently be actuated. Its presence means the observed
/// limit is not authoritative, so it can never be confirmation.
pub const POD_RESIZE_PENDING_CONDITION: &str = "PodResizePending";

/// Resource key mutated by every request this module emits.
const CPU_RESOURCE_KEY: &str = "cpu";

/// Which of the two per-Pod launcher lookups failed its uniqueness check.
///
/// Both are checked *before* any PATCH is issued: an ambiguous identity in
/// either place means we cannot name the resize target, and guessing (by index,
/// or by falling through to the regular-container array) is exactly the bug
/// this type exists to forbid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LauncherIdentitySite {
    /// `spec.initContainers` — where the PATCH lands.
    SpecInitContainers,
    /// `status.initContainerStatuses` — where confirmation is read.
    StatusInitContainerStatuses,
}

impl LauncherIdentitySite {
    /// Stable label, safe for logs and metrics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SpecInitContainers => "spec.initContainers",
            Self::StatusInitContainerStatuses => "status.initContainerStatuses",
        }
    }
}

impl fmt::Display for LauncherIdentitySite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a resize was not confirmed. Every variant is a *failure*: this client
/// has no "probably fine" branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotConfirmed {
    /// A [`POD_RESIZE_PENDING_CONDITION`] is present on the Pod. The kubelet is
    /// telling us the request is not actuated; any limit we can read is stale
    /// by definition.
    ResizePending,
    /// The launcher's init-container status carries no `resources.limits.cpu`
    /// at all. An absent value is never a match.
    StatusLimitAbsent,
    /// The launcher's init-container status reports a CPU limit that is not the
    /// target.
    StatusStale {
        /// Millicores currently reported by `status.initContainerStatuses`.
        observed_millis: u64,
        /// Millicores the PATCH requested.
        target_millis: u64,
    },
    /// The launcher's init-container status reports a CPU limit that is not a
    /// parseable quantity. Fail closed rather than guess.
    StatusLimitUnparseable {
        /// The raw `Quantity` string as returned by the apiserver.
        raw: String,
    },
}

impl fmt::Display for NotConfirmed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ResizePending => {
                write!(f, "a `{POD_RESIZE_PENDING_CONDITION}` condition is present")
            }
            Self::StatusLimitAbsent => write!(
                f,
                "`status.initContainerStatuses[{LAUNCHER_CONTAINER_NAME}].resources.limits.cpu` is absent"
            ),
            Self::StatusStale {
                observed_millis,
                target_millis,
            } => write!(
                f,
                "`status.initContainerStatuses[{LAUNCHER_CONTAINER_NAME}]` reports {observed_millis}m, want {target_millis}m"
            ),
            Self::StatusLimitUnparseable { raw } => write!(
                f,
                "`status.initContainerStatuses[{LAUNCHER_CONTAINER_NAME}]` reports unparseable cpu `{raw}`"
            ),
        }
    }
}

/// Every way a limits-only resize can fail.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PodResizeError {
    /// The Pod does not contain exactly one launcher entry at `site`. **No
    /// PATCH is issued** when this is raised.
    ///
    /// Zero entries and two entries are the same error deliberately: both mean
    /// "the launcher cannot be named", and a client that resolves either one by
    /// positional index (`[0]`) would silently resize the wrong container.
    #[error(
        "launcher identity ambiguous: expected exactly one `{LAUNCHER_CONTAINER_NAME}` entry in {site}, found {found}"
    )]
    LauncherIdentityAmbiguous {
        /// Which array failed the uniqueness check.
        site: LauncherIdentitySite,
        /// How many entries carried the launcher name.
        found: usize,
    },
    /// A CPU quantity could not be parsed into millicores.
    #[error("invalid cpu quantity `{raw}`")]
    InvalidCpuQuantity {
        /// The rejected input.
        raw: String,
    },
    /// The PATCH was accepted but the fresh status does not confirm it.
    #[error("resize not confirmed: {0}")]
    NotConfirmed(NotConfirmed),
    /// The apiserver call itself failed. Flattened to a string so the error
    /// type stays `Clone + PartialEq` for exhaustive fixture assertions.
    #[error("kubernetes api error during {op}: {message}")]
    Api {
        /// Which call failed (`get` or `patch`).
        op: &'static str,
        /// Rendered `kube::Error`.
        message: String,
        /// The apiserver's HTTP status, when the failure was an apiserver
        /// *verdict* rather than a transport failure.
        ///
        /// Carried as a number rather than left inside `message` because the
        /// lift state machine has to tell `403 Forbidden` (the controller's
        /// `pods/resize` RBAC rule is missing — an operator fact) from `422`
        /// (the apiserver refused this particular resize — a request fact) from
        /// a timeout (nothing is known at all), and each maps to a different
        /// settled degrade reason. Recovering that distinction by matching on a
        /// rendered error string would make the classification depend on
        /// `kube`'s `Display` impl.
        ///
        /// `None` means no HTTP response was received: a transport error, a
        /// timeout, or a client-side failure.
        status: Option<u16>,
    },
}

impl PodResizeError {
    /// The apiserver HTTP status, when this failure carries one.
    #[must_use]
    pub const fn api_status(&self) -> Option<u16> {
        match self {
            Self::Api { status, .. } => *status,
            _ => None,
        }
    }
}

/// Recover the apiserver's HTTP status from a `kube::Error`.
///
/// `kube::Error::Api(ErrorResponse)` is the only variant that carries an
/// apiserver verdict; every other variant is a transport, TLS, auth-plumbing or
/// serialization failure, for which "no status" is the correct answer rather
/// than a guessed one.
fn api_status(error: &kube::Error) -> Option<u16> {
    match error {
        kube::Error::Api(response) => Some(response.code),
        _ => None,
    }
}

/// A CPU limit, normalised to millicores.
///
/// Parsing here rather than passing `&str` around means an unparseable target
/// is rejected *before* a PATCH is built, and means confirmation can compare
/// numerically instead of trusting the apiserver not to re-render the quantity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CpuLimit {
    millis: u64,
}

impl CpuLimit {
    /// Build from a millicore count.
    #[must_use]
    pub const fn from_millis(millis: u64) -> Self {
        Self { millis }
    }

    /// Parse a Kubernetes CPU quantity (`"250m"`, `"4"`, `"0.5"`).
    ///
    /// # Errors
    ///
    /// Returns [`PodResizeError::InvalidCpuQuantity`] for anything that is not
    /// a non-negative decimal optionally suffixed with `m`, or for a fractional
    /// core value that is not a whole number of millicores.
    pub fn parse(raw: &str) -> Result<Self, PodResizeError> {
        let invalid = || PodResizeError::InvalidCpuQuantity {
            raw: raw.to_string(),
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(invalid());
        }
        if let Some(millis) = trimmed.strip_suffix('m') {
            if millis.is_empty() || !millis.bytes().all(|b| b.is_ascii_digit()) {
                return Err(invalid());
            }
            return millis
                .parse::<u64>()
                .map(Self::from_millis)
                .map_err(|_| invalid());
        }
        if !trimmed.bytes().all(|b| b.is_ascii_digit() || b == b'.')
            || trimmed.bytes().filter(|b| *b == b'.').count() > 1
        {
            return Err(invalid());
        }
        let cores: f64 = trimmed.parse().map_err(|_| invalid())?;
        if !cores.is_finite() || cores < 0.0 {
            return Err(invalid());
        }
        let scaled = cores * 1000.0;
        let rounded = scaled.round();
        if (scaled - rounded).abs() > 1e-6 {
            return Err(invalid());
        }
        Ok(Self::from_millis(rounded as u64))
    }

    /// Millicores.
    #[must_use]
    pub const fn millis(self) -> u64 {
        self.millis
    }

    /// Canonical wire form for the PATCH body.
    ///
    /// Always the `m` suffix, so the emitted request is byte-stable regardless
    /// of how the caller spelled the value. The apiserver is free to store
    /// `4000m` back as `4`; confirmation compares millicores, so it does not
    /// care.
    #[must_use]
    pub fn as_quantity(self) -> String {
        format!("{}m", self.millis)
    }
}

impl fmt::Display for CpuLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_quantity())
    }
}

/// Build the limits-only strategic-merge body for one launcher resize.
///
/// The result contains **exactly** `spec.initContainers[0].name` and
/// `spec.initContainers[0].resources.limits.cpu` — no `requests`, no memory, no
/// second field, no `spec.containers`, no other init container. Anything more
/// would either move requests (changing scheduling and Kueue accounting) or
/// address a container this client has no authority over.
#[must_use]
pub fn build_resize_patch(target: CpuLimit) -> Value {
    json!({
        "spec": {
            "initContainers": [{
                "name": LAUNCHER_CONTAINER_NAME,
                "resources": {
                    "limits": {
                        CPU_RESOURCE_KEY: target.as_quantity(),
                    }
                }
            }]
        }
    })
}

/// Locate the unique launcher entry in `spec.initContainers`.
///
/// # Errors
///
/// [`PodResizeError::LauncherIdentityAmbiguous`] when the count is not exactly
/// one. Deliberately *not* falling back to `spec.containers`: no regular
/// container by that name exists, so a fallback could only ever match something
/// wrong.
pub fn locate_launcher_spec(pod: &Pod) -> Result<&Container, PodResizeError> {
    let entries: Vec<&Container> = pod
        .spec
        .as_ref()
        .and_then(|spec| spec.init_containers.as_ref())
        .map(|list| {
            list.iter()
                .filter(|c| c.name == LAUNCHER_CONTAINER_NAME)
                .collect()
        })
        .unwrap_or_default();
    unique(entries, LauncherIdentitySite::SpecInitContainers)
}

/// Locate the unique launcher entry in `status.initContainerStatuses`.
///
/// # Errors
///
/// [`PodResizeError::LauncherIdentityAmbiguous`] when the count is not exactly
/// one. `status.containerStatuses` is never consulted, at any count.
pub fn locate_launcher_status(pod: &Pod) -> Result<&ContainerStatus, PodResizeError> {
    let entries: Vec<&ContainerStatus> = pod
        .status
        .as_ref()
        .and_then(|status| status.init_container_statuses.as_ref())
        .map(|list| {
            list.iter()
                .filter(|c| c.name == LAUNCHER_CONTAINER_NAME)
                .collect()
        })
        .unwrap_or_default();
    unique(entries, LauncherIdentitySite::StatusInitContainerStatuses)
}

fn unique<T>(mut entries: Vec<T>, site: LauncherIdentitySite) -> Result<T, PodResizeError> {
    if entries.len() == 1 {
        Ok(entries.remove(0))
    } else {
        Err(PodResizeError::LauncherIdentityAmbiguous {
            site,
            found: entries.len(),
        })
    }
}

/// Whether the Pod carries a [`POD_RESIZE_PENDING_CONDITION`].
///
/// Presence alone blocks confirmation; the condition's `status` field is
/// deliberately not consulted. The kubelet only publishes this condition while
/// a resize is unactuated, so treating a present-but-`False` condition as
/// "actuated" would be inferring liveness from a field we have no contract for.
/// Fail closed: an extra unconfirmed cycle costs a GET, a false confirmation
/// hands a lease grant to a command that never got the CPU.
#[must_use]
pub fn has_resize_pending_condition(pod: &Pod) -> bool {
    pod.status
        .as_ref()
        .and_then(|status| status.conditions.as_ref())
        .is_some_and(|conditions| {
            conditions
                .iter()
                .any(|c| c.type_ == POD_RESIZE_PENDING_CONDITION)
        })
}

/// Decide whether `pod` confirms that the launcher now holds `target`.
///
/// Reads `status.initContainerStatuses[name=cgroup-launcher]` and nothing else.
/// The Pod's `spec`, its `status.containerStatuses`, and any HTTP metadata are
/// all outside this function's reach by construction — it takes a whole `Pod`
/// precisely so a test can hand it one whose *regular* container status already
/// matches the target and watch it refuse.
///
/// # Errors
///
/// * [`PodResizeError::LauncherIdentityAmbiguous`] — no unique launcher status.
/// * [`PodResizeError::NotConfirmed`] — a unique status was found and it does
///   not (yet) agree, or a resize is pending.
pub fn confirm_launcher_cpu(pod: &Pod, target: CpuLimit) -> Result<(), PodResizeError> {
    if has_resize_pending_condition(pod) {
        return Err(PodResizeError::NotConfirmed(NotConfirmed::ResizePending));
    }
    let status = locate_launcher_status(pod)?;
    let raw = status
        .resources
        .as_ref()
        .and_then(|r| r.limits.as_ref())
        .and_then(|limits| limits.get(CPU_RESOURCE_KEY))
        .map(|q: &Quantity| q.0.clone());
    let Some(raw) = raw else {
        return Err(PodResizeError::NotConfirmed(
            NotConfirmed::StatusLimitAbsent,
        ));
    };
    let observed = CpuLimit::parse(&raw)
        .map_err(|_| PodResizeError::NotConfirmed(NotConfirmed::StatusLimitUnparseable { raw }))?;
    if observed == target {
        Ok(())
    } else {
        Err(PodResizeError::NotConfirmed(NotConfirmed::StatusStale {
            observed_millis: observed.millis(),
            target_millis: target.millis(),
        }))
    }
}

/// The two apiserver calls this client makes, behind a trait so the whole
/// GET / PATCH / GET sequence is drivable from fixtures with no cluster.
#[async_trait]
pub trait PodResizeApi: Send + Sync {
    /// Fetch the Pod fresh. Never served from a cache: identity checks and
    /// confirmation are only meaningful against current state.
    ///
    /// # Errors
    ///
    /// [`PodResizeError::Api`] when the apiserver call fails.
    async fn get_pod(&self, name: &str) -> Result<Pod, PodResizeError>;

    /// PATCH the `resize` subresource with a strategic merge patch.
    ///
    /// # Errors
    ///
    /// [`PodResizeError::Api`] when the apiserver call fails.
    async fn patch_resize(&self, name: &str, body: &Value) -> Result<Pod, PodResizeError>;
}

/// Real [`PodResizeApi`] over a namespaced `kube` client.
pub struct KubePodResizeApi {
    pods: Api<Pod>,
    field_manager: String,
}

impl KubePodResizeApi {
    /// Bind to one namespace.
    #[must_use]
    pub fn new(client: kube::Client, namespace: &str, field_manager: impl Into<String>) -> Self {
        Self {
            pods: Api::namespaced(client, namespace),
            field_manager: field_manager.into(),
        }
    }
}

#[async_trait]
impl PodResizeApi for KubePodResizeApi {
    async fn get_pod(&self, name: &str) -> Result<Pod, PodResizeError> {
        self.pods.get(name).await.map_err(|e| PodResizeError::Api {
            op: "get",
            message: e.to_string(),
            status: api_status(&e),
        })
    }

    async fn patch_resize(&self, name: &str, body: &Value) -> Result<Pod, PodResizeError> {
        // Strategic, not Merge: `initContainers` has `patchMergeKey: name`, so
        // a strategic patch merges into the named entry. A Merge patch would
        // replace the entire array and drop every other init container.
        let params = PatchParams::apply(&self.field_manager).force();
        self.pods
            .patch_subresource(RESIZE_SUBRESOURCE, name, &params, &Patch::Strategic(body))
            .await
            .map_err(|e| PodResizeError::Api {
                op: "patch",
                message: e.to_string(),
                status: api_status(&e),
            })
    }
}

/// Limits-only resize client for the launcher native sidecar.
pub struct PodResizeClient<A: PodResizeApi> {
    api: A,
}

impl<A: PodResizeApi> PodResizeClient<A> {
    /// Wrap an API surface.
    pub const fn new(api: A) -> Self {
        Self { api }
    }

    /// Borrow the underlying API surface (fixtures inspect their recorder).
    pub const fn api(&self) -> &A {
        &self.api
    }

    /// GET the Pod, verify the launcher is uniquely identifiable in **both**
    /// `spec.initContainers` and `status.initContainerStatuses`, PATCH the
    /// `resize` subresource with a limits-only body, then GET again and confirm
    /// from `status.initContainerStatuses`.
    ///
    /// Returns `Ok(())` only on a status-confirmed apply. One cycle, no
    /// retries — see the module docs.
    ///
    /// # Errors
    ///
    /// * [`PodResizeError::LauncherIdentityAmbiguous`] — raised *before* the
    ///   PATCH, so an ambiguous Pod is never mutated.
    /// * [`PodResizeError::Api`] — the GET or the PATCH failed.
    /// * [`PodResizeError::NotConfirmed`] — the PATCH was accepted but the
    ///   fresh init-container status does not agree.
    pub async fn resize_launcher_cpu(
        &self,
        pod_name: &str,
        target: CpuLimit,
    ) -> Result<(), PodResizeError> {
        let pod = self.api.get_pod(pod_name).await?;
        // Both identity checks run before the PATCH. Checking only the spec
        // would let a Pod with a duplicated *status* entry be mutated and then
        // fail confirmation — a mutation we could not confirm or undo by name.
        locate_launcher_spec(&pod)?;
        locate_launcher_status(&pod)?;

        let body = build_resize_patch(target);
        debug!(pod = pod_name, target = %target, "patching pods/resize (limits-only)");
        // The PATCH response is discarded on purpose: it echoes the accepted
        // *spec*, which is not confirmation of actuation.
        let _accepted = self.api.patch_resize(pod_name, &body).await?;

        let fresh = self.api.get_pod(pod_name).await?;
        confirm_launcher_cpu(&fresh, target)
    }
}

/// Read the launcher's currently *declared* CPU limit out of
/// `spec.initContainers` — the post-admission ceiling `g8jk-3` captures.
///
/// This is a spec read and is therefore explicitly **not** confirmation of
/// anything; it exists so the ceiling-capture slice does not have to re-derive
/// the launcher lookup rules.
///
/// # Errors
///
/// * [`PodResizeError::LauncherIdentityAmbiguous`] — no unique launcher spec.
/// * [`PodResizeError::InvalidCpuQuantity`] — the declared limit is absent or
///   unparseable.
pub fn declared_launcher_cpu_limit(pod: &Pod) -> Result<CpuLimit, PodResizeError> {
    let container = locate_launcher_spec(pod)?;
    let limits: Option<&BTreeMap<String, Quantity>> =
        container.resources.as_ref().and_then(|r| r.limits.as_ref());
    let raw = limits
        .and_then(|l| l.get(CPU_RESOURCE_KEY))
        .map(|q| q.0.clone())
        .ok_or_else(|| PodResizeError::InvalidCpuQuantity { raw: String::new() })?;
    CpuLimit::parse(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_limit_parses_millicores_and_cores() {
        assert_eq!(CpuLimit::parse("250m").unwrap().millis(), 250);
        assert_eq!(CpuLimit::parse("4").unwrap().millis(), 4000);
        assert_eq!(CpuLimit::parse("0.5").unwrap().millis(), 500);
        assert_eq!(CpuLimit::parse(" 4000m ").unwrap().millis(), 4000);
    }

    #[test]
    fn cpu_limit_rejects_junk() {
        for raw in ["", "m", "abc", "-1", "1.2.3", "4Gi", "250 m"] {
            assert!(
                CpuLimit::parse(raw).is_err(),
                "expected `{raw}` to be rejected"
            );
        }
    }

    #[test]
    fn canonical_quantity_round_trips_through_apiserver_canonicalisation() {
        // The apiserver stores `4000m` as `4`; confirmation must still match.
        let target = CpuLimit::parse("4000m").unwrap();
        assert_eq!(target.as_quantity(), "4000m");
        assert_eq!(CpuLimit::parse("4").unwrap(), target);
    }
}
