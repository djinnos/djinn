//! Fail-closed preflight for proposal `3i92`'s CPU-quota cutover.
//!
//! # What this is for
//!
//! Flipping a deployment from `leaf-v1` (the launcher writes each invocation
//! leaf's `cpu.max`) to `resize-v2` (the launcher writes nothing and
//! `PATCH pods/resize` moves the sidecar's own limit) changes who owns quota.
//! Six things have to be true at the moment of the flip, and every one of them
//! fails *silently* if it is not:
//!
//! 1. **`pods/resize` RBAC.** Absent, every lift returns 403 and every brokered
//!    build runs at the unleased floor. Nothing crashes.
//! 2. **The launcher's `resize-v2` CPU ceiling.** Under `resize-v2` the sidecar's
//!    own `limits.cpu` is the ONLY bound a brokered build has. Absent, a build
//!    is bounded by the node.
//! 3. **Birth-downsize confirmation.** Read from the wrong status array it is
//!    not merely unavailable, it is *falsely available* — see [`crate::pod_resize`].
//! 4. **Catalog protocol agreement.** An image that declares `leaf-v1` running
//!    under a `resize-v2` server has two components each believing the other
//!    writes `cpu.max`; the result is a leaf pinned at the unleased floor.
//! 5. **The mode-flip drain fence.** A Pod born under one protocol and resized
//!    under the other is the one state neither side has a recovery path for.
//! 6. **The task-run credential boundary.** `pods/resize` is a namespaced
//!    controller grant; it stays safe only while repository-controlled child
//!    code holds no apiserver credential at all.
//!
//! # Why it is shaped like this
//!
//! [`run`] is a pure function over *observations*, and it is the only entry
//! point. `bin/cutover-preflight.rs` and any startup caller both go through it,
//! so a deploy-time verdict and a startup verdict can never be two different
//! rules that happen to agree today. Gathering is separate and explicit:
//! [`observe_drain_fence`] runs the production `list_nonterminal_resize` query
//! against a real pool, and an unobservable fence is a DEFECT rather than a
//! silent pass.
//!
//! # The one asymmetry: the ceiling is protocol-conditional
//!
//! Epic `xowm` states the ceiling requirement unconditionally ("absent or below
//! its request"). Applied unconditionally it is wrong, and destructively so: a
//! launcher CPU limit under `leaf-v1` is an **ancestor clamp** over every
//! invocation leaf, which is task `7deu`'s measured defect — a leaf set to 4
//! cores burned 0.25 of one, with the leaf's own `nr_throttled` reading 0
//! because the throttling happened at the parent. `launcher.rs` documents that
//! absence as deliberate. So:
//!
//! * `resize-v2` — an absent ceiling is a defect, and so is one below the
//!   sidecar's own 50m request or one that disagrees with the rendered lease.
//! * `leaf-v1` — an absent ceiling is REQUIRED. A present one is the defect.
//!
//! The branch is taken on [`LauncherAuthorityProtocol::launcher_owns_leaf_quota`],
//! never on a protocol string, because the string is a wire spelling and the
//! predicate is the meaning.
//!
//! # Every CPU comparison is numeric
//!
//! The apiserver canonicalises `4000m` to `4`, and this repository's own stock
//! worker `cpu_limit` is the bare string `"4"`. Comparing `Quantity` strings
//! would therefore report a correctly-resized Pod as unconfirmed forever.
//! Ceilings go through [`crate::pod_resize::CpuLimit::parse`]; the lease goes
//! through [`crate::launcher_cpu::rendered_lease_millicores`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use djinn_cgroup_launcher::LauncherAuthorityProtocol;
use djinn_db::BuildPodPermitRepository;
use djinn_db::launcher_compatibility::{AdmissionDecision, LegacyDigestInventory, decide_admission};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{Container, Pod, PodSpec};
use serde_json::Value;

use crate::launcher::{
    LAUNCHER_CONTAINER_NAME, LAUNCHER_CPU_REQUEST, LAUNCHER_CPU_REQUEST_MILLICORES,
};
use crate::launcher_cpu::rendered_lease_millicores;
use crate::pod_resize::{CpuLimit, confirm_launcher_cpu};

/// The exact RBAC triple `pods/resize` requires. Stated once, as a triple,
/// because each element independently makes the grant useless: the wrong verb
/// is a 403 on every lift, the wrong apiGroup addresses a subresource that does
/// not exist, and `pods` without `/resize` grants nothing on the subresource
/// while still looking like a rule "about pods".
pub const RESIZE_RULE_API_GROUP: &str = "";
/// Resource half of the [`RESIZE_RULE_API_GROUP`] triple.
pub const RESIZE_RULE_RESOURCE: &str = "pods/resize";
/// Verb half of the [`RESIZE_RULE_API_GROUP`] triple. PATCH only — the resize
/// subresource accepts nothing else, so any other verb is dead breadth.
pub const RESIZE_RULE_VERB: &str = "patch";

/// Label the chart stamps on the task-run ServiceAccount. Read back from the
/// render rather than hard-coding the SA's name, so a chart that renames the
/// account is still checked instead of silently skipped.
const TASKRUN_COMPONENT_LABEL: &str = "app.kubernetes.io/component";
/// Value of [`TASKRUN_COMPONENT_LABEL`] on the task-run ServiceAccount.
const TASKRUN_COMPONENT_VALUE: &str = "taskrun";

/// Audience the task-run Pod's single projected token must carry.
///
/// Re-exported from [`crate::job`] rather than re-typed: the whole point of the
/// credential boundary is that the token is NOT an apiserver credential, and a
/// second copy of the string is exactly how that drifts.
pub use crate::job::TOKEN_AUDIENCE;

/// The six independent defect classes this preflight is built to detect.
///
/// Named as a closed set so a caller — or a test — can assert *which* classes
/// fired rather than only that something did. `ALL` exists so the report can
/// state that every class was evaluated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DefectClass {
    /// `pods/resize` is not granted as the exact triple.
    PodsResizeRbac,
    /// The launcher sidecar's protocol-conditional CPU ceiling is wrong.
    LauncherCpuCeiling,
    /// Birth downsize cannot be confirmed from `status.initContainerStatuses`.
    BirthConfirmation,
    /// A dispatch-eligible catalog image disagrees with the authority mode.
    CatalogProtocol,
    /// The mode-flip drain fence is nonzero or unobservable.
    DrainFence,
    /// Task-run Pods can reach the apiserver.
    CredentialBoundary,
}

impl DefectClass {
    /// Every class, so a report can prove it evaluated all of them.
    pub const ALL: [Self; 6] = [
        Self::PodsResizeRbac,
        Self::LauncherCpuCeiling,
        Self::BirthConfirmation,
        Self::CatalogProtocol,
        Self::DrainFence,
        Self::CredentialBoundary,
    ];

    /// Stable label, safe for logs, exit-code contracts and shell assertions.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PodsResizeRbac => "pods-resize-rbac",
            Self::LauncherCpuCeiling => "launcher-cpu-ceiling",
            Self::BirthConfirmation => "birth-confirmation",
            Self::CatalogProtocol => "catalog-protocol",
            Self::DrainFence => "drain-fence",
            Self::CredentialBoundary => "credential-boundary",
        }
    }
}

impl fmt::Display for DefectClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One reason the cutover must not proceed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Defect {
    class: DefectClass,
    detail: String,
}

impl Defect {
    fn new(class: DefectClass, detail: impl Into<String>) -> Self {
        Self {
            class,
            detail: detail.into(),
        }
    }

    /// Which of the six classes this defect belongs to.
    #[must_use]
    pub const fn class(&self) -> DefectClass {
        self.class
    }

    /// Operator-facing explanation.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for Defect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.class, self.detail)
    }
}

/// A clean verdict. Carries the classes that were evaluated, so "no defects"
/// can be told apart from "no checks ran".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    evaluated: Vec<DefectClass>,
}

impl Report {
    /// The classes [`run`] actually evaluated on this input.
    #[must_use]
    pub fn evaluated(&self) -> &[DefectClass] {
        &self.evaluated
    }
}

/// The preflight refused the cutover. Carries every defect found, not just the
/// first: an operator fixing one at a time across six deploys is how a cutover
/// window is lost.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("cutover preflight refused: {}", .0.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "))]
pub struct Blocked(Vec<Defect>);

impl Blocked {
    /// Every defect found, in class order.
    #[must_use]
    pub fn defects(&self) -> &[Defect] {
        &self.0
    }

    /// The distinct classes that fired.
    #[must_use]
    pub fn classes(&self) -> BTreeSet<DefectClass> {
        self.0.iter().map(Defect::class).collect()
    }
}

// ---------------------------------------------------------------------------
// Observations
// ---------------------------------------------------------------------------

/// How a dispatch-eligible catalog image declares (or fails to declare) who
/// owns invocation CPU quota inside it.
///
/// Four variants, and they are the ROWS of the truth table in
/// [`catalog_verdict`]. Adding a fifth is a compile error at every exhaustive
/// match over this type, which is the point: a new way for an image to describe
/// itself must not silently inherit an existing cell's verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CatalogDeclaration {
    /// No declaration recorded, but the image is pinned to an exact `sha256:`
    /// digest that the pre-cutover signed inventory lists.
    NoHandshakeAllowlistedDigest,
    /// No declaration, and no allowlisted `sha256:` digest — a mutable tag, an
    /// absent digest, or a digest nobody inventoried.
    NoHandshakeUnknownDigest,
    /// The image declares `leaf-v1`.
    DeclaredLeafV1,
    /// The image declares `resize-v2`.
    DeclaredResizeV2,
}

impl CatalogDeclaration {
    /// Every variant, so a matrix is derived rather than hand-listed.
    pub const ALL: [Self; 4] = [
        Self::NoHandshakeAllowlistedDigest,
        Self::NoHandshakeUnknownDigest,
        Self::DeclaredLeafV1,
        Self::DeclaredResizeV2,
    ];

    /// Stable label for logs and shell assertions.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoHandshakeAllowlistedDigest => "no-handshake-allowlisted-digest",
            Self::NoHandshakeUnknownDigest => "no-handshake-unknown-digest",
            Self::DeclaredLeafV1 => "declared-leaf-v1",
            Self::DeclaredResizeV2 => "declared-resize-v2",
        }
    }
}

impl fmt::Display for CatalogDeclaration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl CatalogDeclaration {
    /// A representative catalog image for this row of the truth table.
    ///
    /// `inventoried` must be a digest the caller's [`LegacyDigestInventory`]
    /// vouches for and `uninventoried` one it does not. Materialising the rows
    /// here — rather than in a test — means the matrix a proof iterates is
    /// built from the same exhaustive match the validator compiles against: a
    /// fifth variant fails compilation instead of quietly skipping a cell.
    #[must_use]
    pub fn sample_image(self, inventoried: &str, uninventoried: &str) -> CatalogImage {
        match self {
            Self::NoHandshakeAllowlistedDigest => CatalogImage {
                pull_ref: format!("registry.example/legacy@{inventoried}"),
                declared: None,
                digest: Some(inventoried.to_string()),
            },
            Self::NoHandshakeUnknownDigest => CatalogImage {
                pull_ref: format!("registry.example/legacy@{uninventoried}"),
                declared: None,
                digest: Some(uninventoried.to_string()),
            },
            Self::DeclaredLeafV1 => CatalogImage {
                pull_ref: format!("registry.example/declared@{inventoried}"),
                declared: Some(LauncherAuthorityProtocol::LeafV1),
                digest: Some(inventoried.to_string()),
            },
            Self::DeclaredResizeV2 => CatalogImage {
                pull_ref: format!("registry.example/declared@{inventoried}"),
                declared: Some(LauncherAuthorityProtocol::ResizeV2),
                digest: Some(inventoried.to_string()),
            },
        }
    }
}

/// A dispatch-eligible catalog image, as the preflight sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogImage {
    /// The pull reference a Pod would actually run.
    pub pull_ref: String,
    /// `launcher_authority_protocol` as recorded by the build (migration 166).
    pub declared: Option<LauncherAuthorityProtocol>,
    /// `images.registry_digest`.
    pub digest: Option<String>,
}

/// Whether `image` may dispatch while the server's authority mode is `mode`.
///
/// **This is not a rule of its own.** It delegates to
/// [`djinn_db::launcher_compatibility::decide_admission`], the centralized
/// admission decision that composes task `z3gi`'s declaration verdict with the
/// signed legacy-digest inventory. A preflight with its own copy would be a
/// second opinion about which images may run, and the whole point of a cutover
/// preflight is to answer, in advance, the question dispatch will ask later.
///
/// Two properties come for free from that delegation and are the reason it is
/// the right seam:
///
/// * **The authority mode is in the comparison.** A pre-handshake image reaches
///   leaf authority only under `leaf-v1`; under `resize-v2` it is
///   `MissingDeclarationUnderMode`. Dropping the mode would let a legacy
///   artifact — whose launcher writes leaf `cpu.max` — dispatch under a server
///   that believes pod resize owns quota.
/// * **Only exact digests are compared.** `PreProtocolDigest::parse` requires
///   `sha256:` plus 64 lowercase hex, so a mutable tag can never satisfy the
///   inventory however carefully it was listed.
///
/// The extra check on top is that the ADMITTED authority equals the mode being
/// flipped to. `decide_admission` can only admit the mode it was handed, so
/// this is a fail-closed assertion rather than a live branch — and it is what
/// makes a future third variant land as a defect instead of a silent pass.
pub fn catalog_verdict(
    image: &CatalogImage,
    mode: LauncherAuthorityProtocol,
    inventory: &LegacyDigestInventory,
) -> Result<(), String> {
    match decide_admission(mode, image.declared, image.digest.as_deref(), inventory) {
        Ok(AdmissionDecision::Admitted(effective)) if effective == mode => Ok(()),
        Ok(AdmissionDecision::Admitted(effective)) => Err(format!(
            "the image would dispatch under {effective} while the server authority mode is \
             {mode}; two components would believe they own one leaf's cpu.max"
        )),
        Ok(AdmissionDecision::Undeclarable) => Err(
            "the image declares no launcher authority protocol and carries no registry digest, so \
             nothing identifies which component owns quota in it (the render refuses this shape \
             too, at djinn_k8s::launcher::render_authority_protocol)"
                .to_string(),
        ),
        Err(rejection) => Err(rejection.to_string()),
    }
}

/// Birth-downsize evidence for one live task-run Pod.
///
/// Deliberately carries the WHOLE Pod rather than an extracted limit: the
/// property under test is that confirmation comes from
/// `status.initContainerStatuses[name=cgroup-launcher]` and from nowhere else,
/// and that is only assertable if a caller can hand over a Pod whose
/// `status.containerStatuses` carries a *coincidentally matching* entry.
#[derive(Clone, Debug, PartialEq)]
pub struct BirthObservation {
    /// The Pod as the apiserver returned it.
    pub pod: Pod,
    /// The birth-downsize target, as a Kubernetes CPU quantity. Compared in
    /// millicores, so `"4000m"` and `"4"` are the same target.
    pub target_cpu: String,
}

/// The mode-flip drain fence: what is still in flight under the old protocol.
///
/// `unobservable` is a first-class state rather than an empty list, because
/// "the database was unreachable" and "nothing is in flight" are the two
/// answers a cutover must never confuse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DrainFenceObservation {
    nonterminal_resize: Vec<String>,
    live_task_run_pods: Vec<String>,
    unobservable: Option<String>,
}

impl DrainFenceObservation {
    /// A fence that was actually read.
    #[must_use]
    pub fn observed(nonterminal_resize: Vec<String>, live_task_run_pods: Vec<String>) -> Self {
        Self {
            nonterminal_resize,
            live_task_run_pods,
            unobservable: None,
        }
    }

    /// A fence that could not be read. Fails closed.
    #[must_use]
    pub fn unobservable(reason: impl Into<String>) -> Self {
        Self {
            nonterminal_resize: Vec::new(),
            live_task_run_pods: Vec::new(),
            unobservable: Some(reason.into()),
        }
    }
}

/// Read the durable half of the drain fence with the PRODUCTION query.
///
/// Calls [`BuildPodPermitRepository::list_nonterminal_resize`] — the same
/// statement, and therefore the same `state IN (...)` predicate and the same
/// `build_pod_permits_resize_nonterminal_idx` partial index, that the
/// controller's restart recovery reads. A preflight with its own copy of the
/// predicate would pass a cutover whose recovery path is blind to a state the
/// copy happens to list and the production query does not.
///
/// The Pod half is passed in: it comes from the apiserver, which this function
/// deliberately does not talk to.
pub async fn observe_drain_fence(
    permits: &BuildPodPermitRepository,
    live_task_run_pods: Vec<String>,
) -> DrainFenceObservation {
    match permits.list_nonterminal_resize().await {
        Ok(rows) => DrainFenceObservation::observed(
            rows.iter()
                .map(|row| format!("{}={:?}", row.task_run_id, row.state))
                .collect(),
            live_task_run_pods,
        ),
        Err(error) => DrainFenceObservation::unobservable(format!(
            "list_nonterminal_resize failed: {error}"
        )),
    }
}

/// Everything [`run`] judges. Borrowed, so the caller keeps ownership of a
/// render it may also want to print.
#[derive(Clone, Copy, Debug)]
pub struct CutoverPreflightInput<'a> {
    /// Every document of a LIVE `helm template` render, as JSON. The RBAC rule
    /// and the ServiceAccount/RoleBinding surface live only here.
    pub manifests: &'a [Value],
    /// The dispatch-ready task-run Job the Rust renderer produces, AFTER
    /// [`crate::launcher::apply_launcher_authority_protocol`]. `automountServiceAccountToken`,
    /// the projected token audience and the launcher ceiling live only here —
    /// Helm never sees them.
    pub task_run_job: &'a Job,
    /// The protocol the deployment is being flipped TO (or is running).
    pub authority_mode: LauncherAuthorityProtocol,
    /// Every catalog image that could be dispatched right now.
    pub catalog: &'a [CatalogImage],
    /// The deployment's signed pre-protocol digest inventory, as
    /// `djinn-db` resolves it. `Unconfigured` keeps the pre-existing
    /// membership rule; `Unusable` vouches for nothing at all.
    pub legacy_digest_inventory: &'a LegacyDigestInventory,
    /// Live Pods whose birth downsize must already be confirmed.
    pub births: &'a [BirthObservation],
    /// The mode-flip drain fence.
    pub drain: &'a DrainFenceObservation,
}

// ---------------------------------------------------------------------------
// The entry point
// ---------------------------------------------------------------------------

/// Decide whether the cutover may proceed.
///
/// The ONLY entry point. `bin/cutover-preflight.rs` calls this, a startup
/// caller calls this, and the integration suite calls this — so there is no
/// second rule to drift from.
///
/// # Errors
///
/// [`Blocked`] carrying every defect found. Never the first one only.
pub fn run(input: &CutoverPreflightInput<'_>) -> Result<Report, Blocked> {
    let mut defects = Vec::new();
    check_pods_resize_rbac(input.manifests, &mut defects);
    check_launcher_ceiling(input.task_run_job, input.authority_mode, &mut defects);
    check_birth_confirmation(input.births, &mut defects);
    check_catalog_protocol(
        input.catalog,
        input.legacy_digest_inventory,
        input.authority_mode,
        &mut defects,
    );
    check_drain_fence(input.drain, &mut defects);
    check_credential_boundary(input.manifests, input.task_run_job, &mut defects);

    defects.sort_by(|left, right| left.class.cmp(&right.class));
    if defects.is_empty() {
        Ok(Report {
            evaluated: DefectClass::ALL.to_vec(),
        })
    } else {
        Err(Blocked(defects))
    }
}

// ---------------------------------------------------------------------------
// 1. `pods/resize` RBAC
// ---------------------------------------------------------------------------

/// The rendered `pods/resize` grant, checked as the exact triple.
///
/// Each element is checked because each element independently breaks the grant
/// while leaving a rule that a lenient reader would accept:
///
/// * verb — `get` on `pods/resize` authorises nothing the lift needs; the
///   subresource takes PATCH only.
/// * apiGroup — `pods/resize` lives in the core group. Under `apps` the rule
///   names a resource that does not exist.
/// * resource — `pods` is not `pods/resize`. RBAC does not imply subresources.
///
/// Matching "a rule that mentions pods" would stay green under the verb
/// mutation, which is why the triple is matched as a triple.
fn check_pods_resize_rbac(manifests: &[Value], defects: &mut Vec<Defect>) {
    let mut granted_by_role = false;
    let mut granted_cluster_wide = Vec::new();

    for document in manifests {
        let kind = document.get("kind").and_then(Value::as_str).unwrap_or("");
        if kind != "Role" && kind != "ClusterRole" {
            continue;
        }
        let name = metadata_name(document);
        let rules = document
            .get("rules")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for rule in rules {
            if !rule_contains(rule, "apiGroups", RESIZE_RULE_API_GROUP)
                || !rule_contains(rule, "resources", RESIZE_RULE_RESOURCE)
                || !rule_contains(rule, "verbs", RESIZE_RULE_VERB)
            {
                continue;
            }
            if kind == "Role" {
                granted_by_role = true;
            } else {
                granted_cluster_wide.push(name.to_string());
            }
        }
    }

    if !granted_by_role {
        defects.push(Defect::new(
            DefectClass::PodsResizeRbac,
            format!(
                "no rendered namespaced Role grants the exact triple \
                 apiGroups=[{RESIZE_RULE_API_GROUP:?}] resources=[{RESIZE_RULE_RESOURCE:?}] \
                 verbs=[{RESIZE_RULE_VERB:?}]. Without all three the resize PATCH is a 403 and \
                 every brokered build silently runs at the unleased floor"
            ),
        ));
    }
    for name in granted_cluster_wide {
        defects.push(Defect::new(
            DefectClass::PodsResizeRbac,
            format!(
                "ClusterRole {name:?} grants {RESIZE_RULE_RESOURCE} cluster-wide. The grant is \
                 deliberately namespaced; promoting it hands the resize subresource on every \
                 namespace's Pods to the controller identity"
            ),
        ));
    }
}

/// Whether `rule[field]` contains `wanted` exactly. A missing `apiGroups` is
/// NOT treated as the core group: RBAC requires the field, and inferring it
/// would make the apiGroup half of the triple unfalsifiable.
fn rule_contains(rule: &Value, field: &str, wanted: &str) -> bool {
    rule.get(field)
        .and_then(Value::as_array)
        .is_some_and(|values| {
            values
                .iter()
                .any(|value| value.as_str().is_some_and(|value| value == wanted))
        })
}

fn metadata_name(document: &Value) -> &str {
    document
        .get("metadata")
        .and_then(|meta| meta.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("<unnamed>")
}

// ---------------------------------------------------------------------------
// 2. The protocol-conditional launcher CPU ceiling
// ---------------------------------------------------------------------------

/// The launcher sidecar's CPU ceiling, judged against the authority protocol.
///
/// See the module header for why this is conditional and must stay conditional.
/// The branch is [`LauncherAuthorityProtocol::launcher_owns_leaf_quota`], not a
/// string comparison: `leaf-v1` is a wire spelling, "the launcher owns the
/// leaf's quota" is the property that decides whether a container limit is a
/// ceiling or an ancestor clamp.
fn check_launcher_ceiling(
    job: &Job,
    mode: LauncherAuthorityProtocol,
    defects: &mut Vec<Defect>,
) {
    let sidecar = pod_spec(job).and_then(|spec| {
        spec.init_containers
            .as_ref()
            .and_then(|list| list.iter().find(|c| c.name == LAUNCHER_CONTAINER_NAME))
    });
    let Some(sidecar) = sidecar else {
        if mode.launcher_owns_leaf_quota() {
            // leaf-v1 with the launcher disabled is the pre-cutover status quo:
            // no sidecar, no broker, nothing to bound. Not a defect.
            return;
        }
        defects.push(Defect::new(
            DefectClass::LauncherCpuCeiling,
            format!(
                "the rendered task-run Job has no `spec.initContainers[{LAUNCHER_CONTAINER_NAME}]`, \
                 so under resize-v2 there is no container whose limit pod resize could move. The \
                 launcher is the resize target; flipping to resize-v2 without it leaves every \
                 brokered build bounded only by the node"
            ),
        ));
        return;
    };

    let rendered = sidecar
        .resources
        .as_ref()
        .and_then(|resources| resources.limits.as_ref())
        .and_then(|limits| limits.get("cpu"))
        .map(|quantity| quantity.0.clone());

    if mode.launcher_owns_leaf_quota() {
        if let Some(rendered) = rendered {
            defects.push(Defect::new(
                DefectClass::LauncherCpuCeiling,
                format!(
                    "under leaf-v1 the `{LAUNCHER_CONTAINER_NAME}` sidecar carries \
                     limits.cpu={rendered:?}. The launcher writes each invocation leaf's cpu.max \
                     under this protocol, so a container limit here is an ANCESTOR CLAMP over \
                     every leaf, not a ceiling: task 7deu measured a leaf set to 4 cores burning \
                     0.25, with the leaf's own nr_throttled reading 0 because the throttling \
                     happened at the parent. Its absence is required, not missing"
                ),
            ));
        }
        return;
    }

    let Some(rendered) = rendered else {
        defects.push(Defect::new(
            DefectClass::LauncherCpuCeiling,
            format!(
                "under resize-v2 the `{LAUNCHER_CONTAINER_NAME}` sidecar carries no \
                 resources.limits.cpu. The launcher writes no leaf cpu.max under this protocol, so \
                 this limit is the ONLY ceiling a brokered build has and pod resize has nothing to \
                 move"
            ),
        ));
        return;
    };

    let Ok(ceiling) = CpuLimit::parse(&rendered) else {
        defects.push(Defect::new(
            DefectClass::LauncherCpuCeiling,
            format!(
                "the `{LAUNCHER_CONTAINER_NAME}` sidecar's resources.limits.cpu is {rendered:?}, \
                 which is not a parseable Kubernetes CPU quantity"
            ),
        ));
        return;
    };

    if ceiling.millis() < u64::from(LAUNCHER_CPU_REQUEST_MILLICORES) {
        defects.push(Defect::new(
            DefectClass::LauncherCpuCeiling,
            format!(
                "the resize-v2 launcher CPU ceiling is {ceiling}, below the sidecar's own CPU \
                 request {LAUNCHER_CPU_REQUEST}. Kubernetes rejects a container whose limit is \
                 under its request, so this Pod never admits at all"
            ),
        ));
        return;
    }

    // Compared against the lease READ BACK OFF THE RENDER, never recomputed
    // from config: `build_resources::apply_resolved_resources` may already have
    // re-pointed the lease at a per-project `build_resources.task.cpu_limit`
    // override, and a ceiling derived from the deployment default would then
    // clamp such a Pod BELOW the lease its own launcher grants — the 7deu
    // ancestor clamp, re-entered through the override path.
    match rendered_lease_millicores(sidecar) {
        Ok(lease) if u64::from(lease) == ceiling.millis() => {}
        Ok(lease) => defects.push(Defect::new(
            DefectClass::LauncherCpuCeiling,
            format!(
                "the resize-v2 launcher CPU ceiling is {ceiling} but the rendered lease is \
                 {lease}m. A brokered build is granted the lease and bounded by the ceiling; the \
                 two disagreeing means every lift either overshoots the pod limit or is clamped \
                 below the quota the launcher just granted"
            ),
        )),
        Err(found) => defects.push(Defect::new(
            DefectClass::LauncherCpuCeiling,
            format!(
                "the `{LAUNCHER_CONTAINER_NAME}` sidecar carries no usable lease value (found: \
                 {found}), so the ceiling {ceiling} cannot be checked against the quota it is \
                 supposed to bound"
            ),
        )),
    }
}

fn pod_spec(job: &Job) -> Option<&PodSpec> {
    job.spec
        .as_ref()
        .and_then(|spec| spec.template.spec.as_ref())
}

// ---------------------------------------------------------------------------
// 3. Birth-downsize confirmation
// ---------------------------------------------------------------------------

/// Confirm each live Pod's birth downsize.
///
/// Delegates to the production [`confirm_launcher_cpu`], which reads
/// `status.initContainerStatuses[name=cgroup-launcher]` and consults neither
/// `status.containerStatuses` nor the (mutable, immediately-updated) `spec`.
/// That delegation is the point: reading the regular-container array does not
/// merely read nothing, it reads a FALSE confirmation, because the worker
/// container can legitimately carry a coincidentally matching CPU limit.
///
/// The target is parsed before use, so `"4000m"` and the `"4"` the apiserver
/// canonicalises it into are the same target.
fn check_birth_confirmation(births: &[BirthObservation], defects: &mut Vec<Defect>) {
    for birth in births {
        let name = birth
            .pod
            .metadata
            .name
            .as_deref()
            .unwrap_or("<unnamed pod>");
        let Ok(target) = CpuLimit::parse(&birth.target_cpu) else {
            defects.push(Defect::new(
                DefectClass::BirthConfirmation,
                format!(
                    "pod {name}: birth target {:?} is not a parseable CPU quantity",
                    birth.target_cpu
                ),
            ));
            continue;
        };
        if let Err(error) = confirm_launcher_cpu(&birth.pod, target) {
            defects.push(Defect::new(
                DefectClass::BirthConfirmation,
                format!(
                    "pod {name}: birth downsize to {target} is not confirmed by \
                     status.initContainerStatuses[{LAUNCHER_CONTAINER_NAME}]: {error}"
                ),
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// 4. Catalog protocol vs authority mode
// ---------------------------------------------------------------------------

fn check_catalog_protocol(
    catalog: &[CatalogImage],
    inventory: &LegacyDigestInventory,
    mode: LauncherAuthorityProtocol,
    defects: &mut Vec<Defect>,
) {
    for image in catalog {
        if let Err(reason) = catalog_verdict(image, mode, inventory) {
            defects.push(Defect::new(
                DefectClass::CatalogProtocol,
                format!(
                    "dispatch-eligible image {:?} may not run under authority mode {mode}: \
                     {reason}",
                    image.pull_ref
                ),
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// 5. The mode-flip drain fence
// ---------------------------------------------------------------------------

fn check_drain_fence(drain: &DrainFenceObservation, defects: &mut Vec<Defect>) {
    if let Some(reason) = &drain.unobservable {
        defects.push(Defect::new(
            DefectClass::DrainFence,
            format!(
                "the mode-flip drain fence could not be read ({reason}). An unreadable fence is \
                 not an empty one; flipping now can strand a Pod born under one protocol and \
                 resized under the other, which is the single state neither side has a recovery \
                 path for"
            ),
        ));
        return;
    }
    if !drain.nonterminal_resize.is_empty() {
        defects.push(Defect::new(
            DefectClass::DrainFence,
            format!(
                "{} nonterminal resize/lease row(s) are still in flight: {}",
                drain.nonterminal_resize.len(),
                drain.nonterminal_resize.join(", ")
            ),
        ));
    }
    if !drain.live_task_run_pods.is_empty() {
        defects.push(Defect::new(
            DefectClass::DrainFence,
            format!(
                "{} live task-run Pod(s) are still running under the outgoing protocol: {}",
                drain.live_task_run_pods.len(),
                drain.live_task_run_pods.join(", ")
            ),
        ));
    }
}

// ---------------------------------------------------------------------------
// 6. The task-run credential boundary
// ---------------------------------------------------------------------------

/// The credential boundary, across BOTH surfaces that carry it.
///
/// `automountServiceAccountToken: false` and the `djinn`-audience projected
/// token are rendered in Rust ([`crate::job`]); the task-run ServiceAccount and
/// the (deliberately absent) RoleBindings are rendered in Helm. Checking either
/// surface alone leaves the other free to reopen the boundary, and the whole
/// safety argument for a namespaced `pods/resize` grant is that repository-
/// controlled child code holds no apiserver credential to reach it with.
///
/// Every assertion below is made on the RENDERED artifact. A comment in
/// `job.rs` and a constant in this crate are not evidence about what a Pod
/// receives.
fn check_credential_boundary(manifests: &[Value], job: &Job, defects: &mut Vec<Defect>) {
    let Some(spec) = pod_spec(job) else {
        defects.push(Defect::new(
            DefectClass::CredentialBoundary,
            "the rendered task-run Job carries no pod template spec, so neither \
             automountServiceAccountToken nor the projected token audience can be read",
        ));
        return;
    };

    match spec.automount_service_account_token {
        Some(false) => {}
        other => defects.push(Defect::new(
            DefectClass::CredentialBoundary,
            format!(
                "the task-run Pod renders automountServiceAccountToken={other:?}; anything but \
                 Some(false) mounts the ServiceAccount's apiserver credential into every \
                 repository-controlled command in the pod"
            ),
        )),
    }

    check_projected_audiences(spec, defects);
    check_taskrun_service_account_bindings(manifests, defects);
}

/// Every ServiceAccountToken projection in the Pod must name exactly the
/// `djinn` audience.
///
/// An audience-bound token is only usable against the peer that audience names.
/// An absent audience is the apiserver's own default audience — i.e. a real
/// apiserver credential — which is why "absent" is a defect and not a
/// formatting detail.
fn check_projected_audiences(spec: &PodSpec, defects: &mut Vec<Defect>) {
    let mut projections = 0usize;
    for volume in spec.volumes.iter().flatten() {
        let Some(projected) = volume.projected.as_ref() else {
            continue;
        };
        for source in projected.sources.iter().flatten() {
            let Some(token) = source.service_account_token.as_ref() else {
                continue;
            };
            projections += 1;
            if token.audience.as_deref() != Some(TOKEN_AUDIENCE) {
                defects.push(Defect::new(
                    DefectClass::CredentialBoundary,
                    format!(
                        "volume {:?} projects a ServiceAccount token with audience {:?}; only the \
                         exact audience {TOKEN_AUDIENCE:?} is a djinn-server credential. An absent \
                         audience is the apiserver's own default audience — a real apiserver \
                         credential handed to repository-controlled code",
                        volume.name, token.audience
                    ),
                ));
            }
        }
    }
    if projections == 0 {
        defects.push(Defect::new(
            DefectClass::CredentialBoundary,
            format!(
                "the task-run Pod projects no ServiceAccount token at all, so the worker has no \
                 {TOKEN_AUDIENCE:?}-audience credential to authenticate back to djinn-server with \
                 and the boundary this check describes is not the one the pod is running"
            ),
        ));
    }
}

/// No RoleBinding or ClusterRoleBinding may name the task-run ServiceAccount.
///
/// The account's name is read back off the render (by its `component: taskrun`
/// label) rather than hard-coded, so a chart that renames it is still checked
/// instead of matching nothing and passing.
fn check_taskrun_service_account_bindings(manifests: &[Value], defects: &mut Vec<Defect>) {
    let accounts: BTreeSet<&str> = manifests
        .iter()
        .filter(|doc| doc.get("kind").and_then(Value::as_str) == Some("ServiceAccount"))
        .filter(|doc| {
            doc.get("metadata")
                .and_then(|meta| meta.get("labels"))
                .and_then(|labels| labels.get(TASKRUN_COMPONENT_LABEL))
                .and_then(Value::as_str)
                == Some(TASKRUN_COMPONENT_VALUE)
        })
        .map(|doc| metadata_name(doc))
        .collect();

    if accounts.is_empty() {
        defects.push(Defect::new(
            DefectClass::CredentialBoundary,
            format!(
                "the render contains no ServiceAccount labelled \
                 {TASKRUN_COMPONENT_LABEL}={TASKRUN_COMPONENT_VALUE}, so the identity task-run \
                 Pods run as cannot be named and the binding check below would match nothing and \
                 pass vacuously"
            ),
        ));
        return;
    }

    for document in manifests {
        let kind = document.get("kind").and_then(Value::as_str).unwrap_or("");
        if kind != "RoleBinding" && kind != "ClusterRoleBinding" {
            continue;
        }
        let subjects = document
            .get("subjects")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for subject in subjects {
            let named = subject.get("name").and_then(Value::as_str).unwrap_or("");
            let is_sa = subject.get("kind").and_then(Value::as_str) == Some("ServiceAccount");
            if is_sa && accounts.contains(named) {
                defects.push(Defect::new(
                    DefectClass::CredentialBoundary,
                    format!(
                        "{kind} {:?} binds the task-run ServiceAccount {named:?} to a role. The \
                         task-run identity must hold no RBAC at all: it is the identity \
                         repository-controlled child code runs as, and the namespaced \
                         {RESIZE_RULE_RESOURCE} grant is only safe while that code has no \
                         apiserver credential",
                        metadata_name(document)
                    ),
                ));
            }
        }
    }
}

/// Sidecar lookup shared with the driver, so a caller can report the sidecar it
/// is about to hand to [`run`] without a second traversal.
#[must_use]
pub fn launcher_sidecar(job: &Job) -> Option<&Container> {
    pod_spec(job).and_then(|spec| {
        spec.init_containers
            .as_ref()
            .and_then(|list| list.iter().find(|c| c.name == LAUNCHER_CONTAINER_NAME))
    })
}

/// Human-readable one-line summary of the inputs, echoed by the driver on both
/// verdicts: a gate that says only "BLOCKED" leaves an operator guessing which
/// observation produced it.
#[must_use]
pub fn summarize(input: &CutoverPreflightInput<'_>) -> String {
    let ceiling = launcher_sidecar(input.task_run_job)
        .and_then(|sidecar| sidecar.resources.as_ref())
        .and_then(|resources| resources.limits.as_ref())
        .and_then(|limits| limits.get("cpu"))
        .map_or_else(|| "<absent>".to_string(), |quantity| quantity.0.clone());
    let kinds: BTreeMap<&str, usize> =
        input
            .manifests
            .iter()
            .fold(BTreeMap::new(), |mut counts, document| {
                let kind = document.get("kind").and_then(Value::as_str).unwrap_or("?");
                *counts.entry(kind).or_default() += 1;
                counts
            });
    format!(
        "authority_mode={} launcher_ceiling={ceiling} catalog_images={} legacy_inventory={} \
         births={} manifests={} roles={} bindings={}",
        input.authority_mode,
        input.catalog.len(),
        match input.legacy_digest_inventory {
            LegacyDigestInventory::Unconfigured => "unconfigured".to_string(),
            LegacyDigestInventory::Verified { digests, .. } => format!("verified:{}", digests.len()),
            LegacyDigestInventory::Unusable(fault) => format!("unusable:{fault}"),
        },
        input.births.len(),
        input.manifests.len(),
        kinds.get("Role").copied().unwrap_or(0),
        kinds.get("RoleBinding").copied().unwrap_or(0),
    )
}
