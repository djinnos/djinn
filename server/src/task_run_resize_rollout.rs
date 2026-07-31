//! The fenced operational cutover between launcher quota authorities, and its
//! reverse (task `eeky`, epic `xowm`, proposal `3i92`).
//!
//! Per-project images are built independently of the server and cannot be
//! swapped atomically by Helm. So the transition from `leaf-v1` (the launcher
//! writes each invocation leaf's `cpu.max`) to `resize-v2` (Kubernetes owns the
//! launcher sidecar's `limits.cpu`) is not a deploy — it is an ordered,
//! interruptible sequence over a live catalog, with a reverse that must be able
//! to refuse itself.
//!
//! # What this module adds, and what it only composes
//!
//! It adds no new SQL, no new Kubernetes primitive and no second copy of any
//! decision. It composes:
//!
//! * `vf7a` — [`LegacyDigestInventory`] and
//!   [`ImageRepository::signed_legacy_digest_allowlist`], the signed immutable
//!   allowlist.
//! * `z3gi` — [`LauncherAuthorityModeRepository::set_mode`], the transactional
//!   drain fence that holds the `build_pod_permit_pools` lock while it counts.
//! * `4acu` — [`BuildPodPermitRepository::list_nonterminal_resize`], the
//!   restart-readable view of every permit that still owes a lift, a drop or a
//!   quarantine decision. **Before this module that method had no production
//!   caller.** It has one now, and [`ResizeRollout::prove_drained`] is it.
//! * `#2836` — [`decide_admission`], the one place a missing protocol
//!   declaration can become an effective quota authority.
//!
//! # Why the drain proof is two checks and not one
//!
//! [`LauncherAuthorityModeRepository::set_mode`] already refuses a flip while
//! any permit row is live, and it does so unraceably. It is nevertheless not
//! sufficient, for two independent reasons:
//!
//! 1. **PostgreSQL cannot see a Pod.** A task-run Pod whose permit was released
//!    — or that outlived its permit through a crash — is invisible to every
//!    count `set_mode` can take. Flipping under one strands it: a `leaf-v1` Pod
//!    under `resize-v2` authority has a launcher that already wrote its leaf
//!    quota and a server that believes Kubernetes owns it. So
//!    [`TaskRunPodPlane::live_task_run_pods`] is enumerated too, and a
//!    non-empty answer blocks the flip before `set_mode` is ever called.
//! 2. **A census is not a diagnostic.** `set_mode`'s refusal reports *counts*.
//!    An operator staring at a blocked cutover at 03:00 needs the task-run ids
//!    and lifecycle states, which is exactly what `list_nonterminal_resize`
//!    returns and nothing else does.
//!
//! The two are not redundant and neither substitutes for the other: this module
//! blocks with [`RolloutBlocked::NonterminalResizeRows`] (rows, named) where
//! `set_mode` would block with [`RolloutBlocked::AuthorityDrainRefused`]
//! (counts). If the `list_nonterminal_resize` call were deleted, a seeded
//! nonterminal row would still block the flip — but with the *other* variant
//! and no row identities, which is what the tests assert on.
//!
//! # Ordering is enforced, not documented
//!
//! Every step is a method, every method declares the steps that must already
//! have run, and [`ResizeRollout::journal`] records what actually happened.
//! Calling the flip before the drain proof, or resuming admission before the
//! flip is confirmed, returns [`RolloutBlocked::StepOutOfOrder`] — it does not
//! merely violate a runbook. The journal is the assertion surface: a test reads
//! the observed sequence, not a checklist.
//!
//! # There is no state with two quota authorities, or none
//!
//! [`ResizeRollout::attempt_dispatch`] is the single admission path this module
//! exposes, and it resolves exactly one authority per dispatch through
//! [`decide_admission`]. A `resize-v2` image under `leaf-v1` mode is refused
//! before any Pod is created; a `leaf-v1` (or allowlisted no-handshake) Pod is
//! never handed to [`TaskRunPodPlane::resize_launcher_cpu`], so the count of
//! resize PATCHes against it is structurally zero rather than merely intended
//! to be.
//!
//! # What this module deliberately does not touch
//!
//! It renders nothing. In particular it does not reintroduce a blanket launcher
//! CPU limit: the `resize-v2`-only ceiling in
//! `djinn_k8s::launcher::render_authority_protocol` is read through the
//! bootstrap that already owns it (`task_run_resize_bootstrap`) and is not
//! re-decided here. Under `leaf-v1` a container limit on the launcher is an
//! ancestor clamp over every invocation leaf — task `7deu` measured a 4-core
//! leaf burning 0.25 — and nothing in this file can produce one.

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use djinn_db::launcher_compatibility::{
    AdmissionDecision, AdmissionRejection, LegacyDigestInventory, PreProtocolDigest,
};
use djinn_db::repositories::image::{LegacyAllowlistDefect, SignedLegacyAllowlist};
use djinn_db::{
    BuildPodPermitRepository, BuildPodPermitRow, Database, Image, ImageRepository,
    LauncherAuthorityDrainCensus, LauncherAuthorityModeRepository, SetLauncherAuthorityModeResult,
    decide_admission,
};
use djinn_launcher_protocol::LauncherAuthorityProtocol;
use sha2::{Digest as _, Sha256};
use tracing::{info, warn};

/// The CPU limit a `resize-v2` launcher sidecar is downsized to at birth.
///
/// Restated from [`crate::task_run_resize_bootstrap::BIRTH_CPU_MILLICORES`]
/// rather than re-derived, so a dispatch issued through this module and one
/// issued through bootstrap target the same value.
pub use crate::task_run_resize_bootstrap::BIRTH_CPU_MILLICORES;

// ── steps and the journal ───────────────────────────────────────────────────

/// One observable step of the cutover.
///
/// The forward sequence is the proposal's five steps expanded to the points
/// that are individually provable; the reverse re-uses the last five. Variants
/// are ordered as they must occur.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RolloutStep {
    /// Catalog mutation is frozen while the old server is still live, so the
    /// inventory that gets signed cannot go stale between signing and use.
    CatalogMutationFrozen,
    /// The signed immutable legacy-digest inventory is loaded and covers every
    /// dispatch-eligible no-handshake row.
    LegacyInventorySigned,
    /// The protocol-aware server and the namespaced `pods/resize` RBAC are
    /// deployed **with authority mode still `leaf-v1`**.
    ProtocolAwareServerDeployed,
    /// Images are rebuilt and cataloged as `resize-v2`, while the immutable
    /// legacy digests and the `leaf-v1` rollback digests are retained.
    CatalogRebuiltAsResizeV2,
    /// Every retained rollback digest was fetched from the registry and its
    /// content digest matched what the catalog records.
    RetentionVerified,
    /// Admission is paused. Proven by a refused dispatch, not by a row.
    AdmissionPaused,
    /// Zero live task-run Pods and zero nonterminal resize/lease rows.
    DrainProven,
    /// The authority mode compare-and-swap committed behind its own fence.
    AuthorityModeFlipped,
    /// Admission is resumed. Only reachable after a confirmed flip.
    AdmissionResumed,
}

impl RolloutStep {
    /// The steps that must already have run before this one may.
    ///
    /// This is the ordering itself, in one place. `AdmissionResumed` requires
    /// `AuthorityModeFlipped` and `AuthorityModeFlipped` requires `DrainProven`
    /// — the two reorderings the cutover has to make impossible.
    const fn prerequisites(self) -> &'static [Self] {
        match self {
            Self::CatalogMutationFrozen => &[],
            Self::LegacyInventorySigned => &[Self::CatalogMutationFrozen],
            Self::ProtocolAwareServerDeployed => &[Self::LegacyInventorySigned],
            Self::CatalogRebuiltAsResizeV2 => &[Self::ProtocolAwareServerDeployed],
            // Retention is provable on its own evidence: the rollback path runs
            // it first, with no forward step behind it.
            Self::RetentionVerified => &[],
            Self::AdmissionPaused => &[Self::RetentionVerified],
            Self::DrainProven => &[Self::AdmissionPaused],
            Self::AuthorityModeFlipped => &[Self::AdmissionPaused, Self::DrainProven],
            Self::AdmissionResumed => &[Self::AuthorityModeFlipped],
        }
    }
}

// ── blocking outcomes ───────────────────────────────────────────────────────

/// One nonterminal resize/lease row, named.
///
/// Projected from [`BuildPodPermitRow`] so a blocked operator reads task-run
/// ids and lifecycle states rather than a count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NonterminalResizeRow {
    /// `build_pod_permits.task_run_id`.
    pub task_run_id: String,
    /// The durable lifecycle state, as `migration 164` spells it.
    pub state: String,
    /// The captured Pod UID, when an identity exists.
    pub pod_uid: Option<String>,
}

impl From<&BuildPodPermitRow> for NonterminalResizeRow {
    fn from(row: &BuildPodPermitRow) -> Self {
        Self {
            task_run_id: row.task_run_id.clone(),
            state: format!("{:?}", row.state),
            pod_uid: row
                .resize_identity
                .as_ref()
                .map(|identity| identity.pod_uid.clone()),
        }
    }
}

/// A task-run Pod the apiserver still holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveTaskRunPod {
    /// `metadata.name`.
    pub pod_name: String,
    /// The immutable `metadata.uid` the fence would be taken against.
    pub pod_uid: String,
    /// The task run it carries, from the `djinn.app/task-run-id` label.
    pub task_run_id: String,
}

/// Why the cutover stopped. **No variant means "proceeded anyway".**
///
/// Every variant leaves the authority mode unchanged and admission in whatever
/// state the previous step left it — which, for every variant reachable after
/// [`RolloutStep::AdmissionPaused`], is paused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RolloutBlocked {
    /// A step ran before one of its prerequisites.
    #[error("step {step:?} requires {missing:?}, which has not run")]
    StepOutOfOrder {
        /// The step that was attempted.
        step: RolloutStep,
        /// The first missing prerequisite.
        missing: RolloutStep,
    },
    /// A step ran twice. A cutover is not idempotent by replay.
    #[error("step {0:?} has already run")]
    StepAlreadyRun(RolloutStep),
    /// The signed allowlist is absent, unusable, or does not cover the catalog.
    #[error("the signed legacy digest allowlist is unavailable: {0}")]
    AllowlistUnavailable(LegacyAllowlistDefect),
    /// A retained digest could not be proven pullable, or the registry returned
    /// content whose digest is not the recorded one.
    #[error("retained artifact {digest} is not provably retained: {detail}")]
    RetentionUnprovable {
        /// The digest the catalog records.
        digest: String,
        /// What the registry round trip actually reported.
        detail: String,
    },
    /// Dispatch-eligible catalog rows disagree with the authority mode.
    #[error("{} dispatch-eligible catalog image(s) are incompatible with the authority mode", .0.len())]
    CatalogIncompatible(Vec<CatalogDefect>),
    /// Task-run Pods are still live. PostgreSQL cannot see these.
    #[error("{} task-run Pod(s) are still live", .0.len())]
    LiveTaskRunPods(Vec<LiveTaskRunPod>),
    /// Permits still owe a resize/lease decision, named individually.
    #[error("{} permit(s) are in a nonterminal resize state", .0.len())]
    NonterminalResizeRows(Vec<NonterminalResizeRow>),
    /// The apiserver could not be enumerated. Never read as zero Pods.
    #[error("the task-run Pod census is unavailable: {0}")]
    PodCensusUnavailable(String),
    /// The permit relation could not be read. Never read as zero rows.
    #[error("the build pod permit relation is unavailable: {0}")]
    PermitsUnavailable(String),
    /// `set_mode`'s own transactional fence refused, reporting counts.
    ///
    /// Reaching this variant means the two checks above passed and the fenced
    /// count still found something — a row that appeared between the unlocked
    /// census and the locked one. It is not the variant a seeded nonterminal
    /// row produces.
    #[error("the authority drain fence refused the flip: {census:?}")]
    AuthorityDrainRefused {
        /// The fenced per-dimension counts.
        census: LauncherAuthorityDrainCensus,
    },
    /// The authority singleton could not be read or written.
    #[error("the launcher authority mode is unavailable")]
    AuthorityUnavailable,
    /// The authority singleton is absent. Never a default mode.
    #[error("the launcher authority mode singleton is unseeded")]
    AuthorityUninitialized,
    /// Another operator moved the epoch under this cutover.
    #[error("the authority mode epoch moved to {epoch} under this cutover")]
    AuthorityEpochConflict {
        /// The epoch the durable row actually holds.
        epoch: i64,
    },
    /// The durable authority mode is not the one this step requires.
    #[error("the authority mode is {found} but this step requires {expected}")]
    AuthorityModeUnexpected {
        /// What the step requires.
        expected: LauncherAuthorityProtocol,
        /// What the singleton actually holds.
        found: LauncherAuthorityProtocol,
    },
    /// A same-mode replay. A cutover that did not move the mode did not run.
    #[error("the authority mode was already {0}; this cutover moved nothing")]
    AuthorityModeUnchanged(LauncherAuthorityProtocol),
    /// Admission control could not be reached. A pause that cannot be proven is
    /// not a pause.
    #[error("admission control is unavailable: {0}")]
    AdmissionUnavailable(String),
    /// The pause was written but a dispatch attempt was still admitted.
    ///
    /// This is the assertion the pause step makes on itself: it does not read
    /// back the row it wrote, it dispatches and requires a refusal.
    #[error("admission was paused but a dispatch attempt was still admitted")]
    AdmissionPauseIneffective,
}

/// One dispatch-eligible catalog row that may not run under the mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogDefect {
    /// `images.id`.
    pub image_id: String,
    /// Why.
    pub reason: CatalogDefectReason,
}

/// Why a catalog row may not dispatch under the resolved authority mode.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CatalogDefectReason {
    /// The artifact declares no protocol AND carries no immutable digest, so
    /// there is no compatibility claim to evaluate. The render refuses it.
    #[error("the artifact declares no protocol and carries no immutable digest")]
    Undeclarable,
    /// The stored declaration is not one of the two wire forms.
    ///
    /// Reached by parsing into [`LauncherAuthorityProtocol`], never by
    /// comparing wire strings: a `==` against `"resize-v2"` would let
    /// `resize-v3` through as "not resize-v2, therefore leaf".
    #[error("the artifact declares {declared:?}, which is not a launcher authority protocol")]
    UnparseableDeclaration {
        /// The offending stored value.
        declared: String,
    },
    /// [`decide_admission`] refused it. Carried verbatim so there is exactly one
    /// vocabulary for "this artifact may not run here".
    #[error("{0}")]
    Rejected(AdmissionRejection),
}

/// What a dispatch-eligible catalog row resolves to under the mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogVerdict {
    /// The artifact declared a protocol and it is exactly the authority mode.
    Declared(LauncherAuthorityProtocol),
    /// The artifact declared nothing, is pinned to an immutable digest the
    /// signed inventory vouches for, and the mode is `leaf-v1`. Effective
    /// authority is launcher leaf authority.
    LegacyLeafAuthority(PreProtocolDigest),
}

impl CatalogVerdict {
    /// The single effective quota authority for a dispatch of this artifact.
    #[must_use]
    pub const fn authority(&self) -> LauncherAuthorityProtocol {
        match self {
            Self::Declared(protocol) => *protocol,
            // A no-handshake artifact is only ever admitted under `leaf-v1`
            // (`decide_admission` refuses it under `resize-v2` even when the
            // inventory vouches for it), so this is the mode, not a default.
            Self::LegacyLeafAuthority(_) => LauncherAuthorityProtocol::LeafV1,
        }
    }
}

/// **The catalog compatibility decision for one image, against one mode.**
///
/// Pure, so the whole matrix is exhaustible without a database, and so the two
/// mutations the criterion names are each one edit away from visible:
///
/// * dropping `mode` from the comparison would admit an allowlisted
///   no-handshake artifact under `resize-v2`, which
///   [`decide_admission`] refuses precisely because `resize-v2` is not the
///   behaviour that artifact was built against;
/// * comparing wire strings instead of the enum would read `resize-v3` as
///   "not `resize-v2`", i.e. as leaf authority. Parsing refuses it instead.
///
/// # Errors
///
/// [`CatalogDefectReason`] — the artifact may not dispatch under `mode`.
pub fn classify_catalog_image(
    mode: LauncherAuthorityProtocol,
    image: &Image,
    inventory: &LegacyDigestInventory,
) -> Result<CatalogVerdict, CatalogDefectReason> {
    // Parse, never compare. `declared_launcher_protocol` routes through
    // `LauncherAuthorityProtocol::from_str`, which has no fallback arm.
    let declared = image.declared_launcher_protocol().map_err(|error| {
        CatalogDefectReason::UnparseableDeclaration {
            declared: error.input().to_owned(),
        }
    })?;

    match decide_admission(
        mode,
        declared,
        image.registry_digest.as_deref(),
        inventory,
    ) {
        Ok(AdmissionDecision::Admitted(protocol)) => match declared {
            Some(_) => Ok(CatalogVerdict::Declared(protocol)),
            None => {
                // Admitted with no declaration means the legacy arm fired, and
                // that arm only fires on a canonical digest under `leaf-v1`.
                let raw = image.registry_digest.as_deref().unwrap_or_default();
                PreProtocolDigest::parse(raw)
                    .map(CatalogVerdict::LegacyLeafAuthority)
                    .map_err(|malformed| {
                        CatalogDefectReason::Rejected(AdmissionRejection::MalformedDigest(malformed))
                    })
            }
        },
        Ok(AdmissionDecision::Undeclarable) => Err(CatalogDefectReason::Undeclarable),
        Err(rejection) => Err(CatalogDefectReason::Rejected(rejection)),
    }
}

// ── seams ───────────────────────────────────────────────────────────────────

/// Administrative admission control, and the production predicate that answers
/// whether dispatch is currently refused.
///
/// [`Self::dispatch_is_paused`] deliberately does not return the pause record.
/// The cutover never asserts on a stored row: it asks the same question the
/// dispatch loop asks, and the pause step below proves the answer by attempting
/// a dispatch.
#[async_trait]
pub trait AdmissionControl: Send + Sync {
    /// Pause global dispatch.
    ///
    /// # Errors
    ///
    /// The durable write failed. Never reported as paused.
    async fn pause(&self, reason: &str) -> Result<(), String>;

    /// Resume global dispatch.
    ///
    /// # Errors
    ///
    /// The durable write failed.
    async fn resume(&self) -> Result<(), String>;

    /// Evaluate the production dispatch-pause predicate over freshly loaded
    /// durable state.
    ///
    /// # Errors
    ///
    /// The state could not be loaded. Never reported as "not paused".
    async fn dispatch_is_paused(&self) -> Result<bool, String>;
}

/// The task-run Pod plane: what creates Pods, what resizes them, and what can
/// enumerate the ones that are live.
#[async_trait]
pub trait TaskRunPodPlane: Send + Sync {
    /// Create the task-run Pod for `task_run_id` from catalog image `image_id`.
    ///
    /// # Errors
    ///
    /// The rendered apiserver failure.
    async fn create_task_run_pod(&self, task_run_id: &str, image_id: &str) -> Result<(), String>;

    /// One limits-only `pods/resize` PATCH against the launcher sidecar.
    ///
    /// Only ever called for a dispatch whose resolved authority is
    /// `resize-v2`.
    ///
    /// # Errors
    ///
    /// The rendered apiserver failure.
    async fn resize_launcher_cpu(&self, task_run_id: &str, millicores: u64) -> Result<(), String>;

    /// Every task-run Pod the apiserver currently holds in the namespace.
    ///
    /// # Errors
    ///
    /// The enumeration failed. An error is never an empty census.
    async fn live_task_run_pods(&self) -> Result<Vec<LiveTaskRunPod>, String>;
}

/// A read of an OCI registry manifest.
///
/// The check built on this compares the SHA-256 of the **returned bytes**
/// against the digest the catalog records. There is no `pullable` column
/// anywhere in the path, and a stored one would not be consulted if there were.
#[async_trait]
pub trait RegistryProbe: Send + Sync {
    /// Fetch the manifest bytes for `reference` in `repository`.
    ///
    /// # Errors
    ///
    /// Anything that prevented the round trip, rendered for the operator: a
    /// 404 for a deleted manifest, a transport failure, an auth refusal.
    async fn fetch_manifest(&self, repository: &str, reference: &str) -> Result<Vec<u8>, String>;
}

/// One artifact whose continued pullability the cutover depends on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedArtifact {
    /// `images.id`, for the operator's report.
    pub image_id: String,
    /// The registry repository path, e.g. `djinn-image-i1`.
    pub repository: String,
    /// The immutable manifest digest the catalog records.
    pub digest: String,
    /// Why it is retained.
    pub role: RetentionRole,
}

/// Why an artifact is in the retained set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetentionRole {
    /// A pre-protocol artifact vouched for by the signed inventory.
    LegacyNoHandshake,
    /// A `leaf-v1` artifact retained so rollback has something to run.
    LeafV1Rollback,
    /// The `resize-v2` artifact the forward cutover activates.
    ResizeV2Current,
}

// ── dispatch ────────────────────────────────────────────────────────────────

/// What one dispatch attempt did.
///
/// Only [`Self::Dispatched`] created a Pod. Every other variant created nothing
/// and issued no resize PATCH, which is what the Pod plane's counters are
/// asserted on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DispatchOutcome {
    /// A Pod was created under exactly one resolved quota authority.
    Dispatched {
        /// The single effective authority.
        authority: LauncherAuthorityProtocol,
        /// Whether a `pods/resize` PATCH was issued. False for every `leaf-v1`
        /// dispatch, structurally — not by intent.
        resized: bool,
    },
    /// Administrative pause refused it before the Pod plane was touched.
    RefusedByAdmissionPause,
    /// The artifact may not run under this authority mode.
    RefusedByCatalog(CatalogDefectReason),
    /// The authority mode could not be resolved. Never defaulted.
    RefusedByAuthority(String),
    /// The Pod plane refused the creation. No resize follows a failed create.
    PodPlaneRefused(String),
}

// ── the driver ──────────────────────────────────────────────────────────────

/// The cutover driver.
///
/// # Why the repositories are not injectable
///
/// [`Self::new`] takes a [`Database`] and constructs
/// [`BuildPodPermitRepository`], [`LauncherAuthorityModeRepository`] and
/// [`ImageRepository`] itself. There is deliberately no constructor that
/// accepts them, and therefore no seam through which a fake permit repository
/// could be substituted for the drain fence or a fake image repository for the
/// catalog check. The properties under test there are that a real partial index
/// selects real rows and that a real CHECK constrains a real column; a fake
/// holds both trivially and proves neither.
///
/// The three seams that *are* injectable — admission control, the Pod plane and
/// the registry — are the ones whose production implementations need a live
/// apiserver or a live registry, and none of them carries a durable predicate.
pub struct ResizeRollout {
    permits: BuildPodPermitRepository,
    authority: LauncherAuthorityModeRepository,
    images: ImageRepository,
    inventory: LegacyDigestInventory,
    admission: std::sync::Arc<dyn AdmissionControl>,
    pods: std::sync::Arc<dyn TaskRunPodPlane>,
    registry: std::sync::Arc<dyn RegistryProbe>,
    journal: Mutex<Vec<RolloutStep>>,
    dispatches_admitted_while_paused: AtomicU64,
}

impl ResizeRollout {
    /// Wire a cutover to a database and the three external seams.
    ///
    /// The database is the *only* way durable state enters: see the type docs.
    #[must_use]
    pub fn new(
        db: Database,
        inventory: LegacyDigestInventory,
        admission: std::sync::Arc<dyn AdmissionControl>,
        pods: std::sync::Arc<dyn TaskRunPodPlane>,
        registry: std::sync::Arc<dyn RegistryProbe>,
    ) -> Self {
        Self {
            permits: BuildPodPermitRepository::new(db.clone()),
            authority: LauncherAuthorityModeRepository::new(db.clone()),
            images: ImageRepository::new(db),
            inventory,
            admission,
            pods,
            registry,
            journal: Mutex::new(Vec::new()),
            dispatches_admitted_while_paused: AtomicU64::new(0),
        }
    }

    /// The steps that have actually run, in the order they ran.
    ///
    /// This is the ordering assertion surface. It is a record of observed
    /// effects, not a checklist: a step that returned
    /// [`RolloutBlocked`] never reaches it.
    #[must_use]
    pub fn journal(&self) -> Vec<RolloutStep> {
        self.journal.lock().expect("rollout journal poisoned").clone()
    }

    /// How many dispatch attempts were admitted while admission was paused.
    ///
    /// Structurally zero while [`Self::attempt_dispatch`] consults the pause
    /// predicate first; non-zero the moment that consultation is deleted, with
    /// or without anyone remembering to write an assertion about it.
    #[must_use]
    pub fn dispatches_admitted_while_paused(&self) -> u64 {
        self.dispatches_admitted_while_paused.load(Ordering::SeqCst)
    }

    /// Refuse `step` unless its prerequisites have run and it has not.
    ///
    /// Deliberately separate from [`Self::record`]: the journal is a record of
    /// what **happened**, so a step whose body then failed must not appear in
    /// it. Reserving on entry would let a refused flip satisfy the prerequisite
    /// of the resume that follows it — the exact reordering this task exists to
    /// make impossible.
    fn guard(&self, step: RolloutStep) -> Result<(), RolloutBlocked> {
        let journal = self.journal.lock().expect("rollout journal poisoned");
        if journal.contains(&step) {
            return Err(RolloutBlocked::StepAlreadyRun(step));
        }
        for required in step.prerequisites() {
            if !journal.contains(required) {
                return Err(RolloutBlocked::StepOutOfOrder {
                    step,
                    missing: *required,
                });
            }
        }
        Ok(())
    }

    /// Journal a step that actually completed.
    fn record(&self, step: RolloutStep) {
        self.journal
            .lock()
            .expect("rollout journal poisoned")
            .push(step);
    }

    // ── forward preparation ─────────────────────────────────────────────────

    /// **Step 1a.** Freeze catalog mutation while the old server is still live.
    ///
    /// Nothing durable changes here; the freeze is an operator action recorded
    /// so the ordering of everything after it is provable. The inventory signed
    /// in 1b is only meaningful if the catalog cannot move under it.
    ///
    /// # Errors
    ///
    /// [`RolloutBlocked::StepAlreadyRun`].
    pub fn freeze_catalog_mutation(&self) -> Result<(), RolloutBlocked> {
        self.guard(RolloutStep::CatalogMutationFrozen)?;
        self.record(RolloutStep::CatalogMutationFrozen);
        Ok(())
    }

    /// **Step 1b.** Load the signed immutable legacy-digest inventory and prove
    /// it covers every dispatch-eligible no-handshake row.
    ///
    /// The signature is verified over the document's exact bytes by
    /// [`LegacyDigestInventory::from_signed_document`] before this is called;
    /// what this adds is the catalog cross-check. A signed document that omits
    /// a live no-handshake row would otherwise produce a green cutover and a
    /// refused dispatch afterwards.
    ///
    /// # Errors
    ///
    /// [`RolloutBlocked::AllowlistUnavailable`] — absent, unusable, or short of
    /// the catalog. [`RolloutBlocked::StepOutOfOrder`] when the catalog is not
    /// frozen yet.
    pub async fn sign_legacy_inventory(&self) -> Result<SignedLegacyAllowlist, RolloutBlocked> {
        self.guard(RolloutStep::LegacyInventorySigned)?;
        let allowlist = self
            .images
            .signed_legacy_digest_allowlist(&self.inventory)
            .await
            .map_err(RolloutBlocked::AllowlistUnavailable)?;
        self.record(RolloutStep::LegacyInventorySigned);
        info!(
            issuer = %allowlist.provenance.issuer,
            issued_at = %allowlist.provenance.issued_at,
            digests = allowlist.digests.len(),
            covered_images = allowlist.inventoried.len(),
            "task_run_resize_rollout: signed legacy digest inventory loaded"
        );
        Ok(allowlist)
    }

    /// **Step 2.** Record that the protocol-aware server and the `pods/resize`
    /// RBAC are deployed, and prove the authority mode is still `leaf-v1`.
    ///
    /// The mode check is the point: deploying the new server is only safe
    /// *because* authority has not moved, and a deployment that finds
    /// `resize-v2` already set means someone flipped ahead of the sequence.
    ///
    /// # Errors
    ///
    /// [`RolloutBlocked::AuthorityUnavailable`], [`RolloutBlocked::AuthorityUninitialized`],
    /// or [`RolloutBlocked::StepOutOfOrder`]. Also blocks when the mode is
    /// already `resize-v2`.
    pub async fn deploy_protocol_aware_server(&self) -> Result<(), RolloutBlocked> {
        self.guard(RolloutStep::ProtocolAwareServerDeployed)?;
        let row = self.read_authority().await?;
        if row.mode != LauncherAuthorityProtocol::LeafV1 {
            return Err(RolloutBlocked::AuthorityModeUnexpected {
                expected: LauncherAuthorityProtocol::LeafV1,
                found: row.mode,
            });
        }
        self.record(RolloutStep::ProtocolAwareServerDeployed);
        Ok(())
    }

    /// **Step 3.** Validate every dispatch-eligible catalog row against the
    /// authority mode the cutover is preparing for.
    ///
    /// `target` is the mode the catalog is being prepared *for*, which during
    /// forward preparation is `resize-v2` while the server still runs
    /// `leaf-v1`. Validating against the target is what makes the preparation
    /// meaningful: a row that would be refused after the flip is found now,
    /// while nothing has moved.
    ///
    /// # Errors
    ///
    /// [`RolloutBlocked::CatalogIncompatible`] naming every offending row —
    /// not just the first, so one pass fixes the catalog. Also
    /// [`RolloutBlocked::PermitsUnavailable`] when the catalog cannot be read
    /// (never an empty catalog).
    pub async fn rebuild_and_catalog_as_resize_v2(
        &self,
        target: LauncherAuthorityProtocol,
    ) -> Result<Vec<(String, CatalogVerdict)>, RolloutBlocked> {
        self.guard(RolloutStep::CatalogRebuiltAsResizeV2)?;
        let verdicts = self.validate_catalog(target).await?;
        self.record(RolloutStep::CatalogRebuiltAsResizeV2);
        Ok(verdicts)
    }

    /// Classify every dispatch-eligible catalog row against `mode`.
    ///
    /// "Dispatch-eligible" is `status = 'ready'` and selected by at least one
    /// project — read through [`ImageRepository::list_selected_catalog_images`]
    /// and [`ImageRepository::list`], both production accessors.
    ///
    /// # Errors
    ///
    /// [`RolloutBlocked::CatalogIncompatible`] or
    /// [`RolloutBlocked::PermitsUnavailable`].
    pub async fn validate_catalog(
        &self,
        mode: LauncherAuthorityProtocol,
    ) -> Result<Vec<(String, CatalogVerdict)>, RolloutBlocked> {
        let selected: BTreeSet<String> = self
            .images
            .list_selected_catalog_images()
            .await
            .map_err(|error| RolloutBlocked::PermitsUnavailable(error.to_string()))?
            .into_iter()
            .map(|image| image.image_id)
            .collect();
        let all = self
            .images
            .list()
            .await
            .map_err(|error| RolloutBlocked::PermitsUnavailable(error.to_string()))?;

        let mut verdicts = Vec::new();
        let mut defects = Vec::new();
        for image in all {
            if !selected.contains(&image.id) || image.status != djinn_db::ImageStatus::READY {
                continue;
            }
            match classify_catalog_image(mode, &image, &self.inventory) {
                Ok(verdict) => verdicts.push((image.id, verdict)),
                Err(reason) => defects.push(CatalogDefect {
                    image_id: image.id,
                    reason,
                }),
            }
        }
        if defects.is_empty() {
            Ok(verdicts)
        } else {
            Err(RolloutBlocked::CatalogIncompatible(defects))
        }
    }

    /// **Step 4.** Prove every retained artifact is still pullable, by fetching
    /// its manifest and comparing the SHA-256 of the returned bytes to the
    /// digest the catalog records.
    ///
    /// A registry that 404s, that is unreachable, or that returns *different*
    /// content under the same reference all fail here. No stored column is
    /// consulted: there is nothing in this path that a `pullable = true` write
    /// could satisfy.
    ///
    /// # Errors
    ///
    /// [`RolloutBlocked::RetentionUnprovable`] naming the digest and what the
    /// round trip actually reported.
    pub async fn verify_retention(
        &self,
        retained: &[RetainedArtifact],
    ) -> Result<(), RolloutBlocked> {
        self.guard(RolloutStep::RetentionVerified)?;
        self.probe_retention(retained).await?;
        self.record(RolloutStep::RetentionVerified);
        Ok(())
    }

    /// The retention round trip without the journal entry, so rollback can
    /// re-prove retention that forward preparation already recorded.
    ///
    /// # Errors
    ///
    /// [`RolloutBlocked::RetentionUnprovable`].
    pub async fn probe_retention(
        &self,
        retained: &[RetainedArtifact],
    ) -> Result<(), RolloutBlocked> {
        for artifact in retained {
            let bytes = self
                .registry
                .fetch_manifest(&artifact.repository, &artifact.digest)
                .await
                .map_err(|detail| RolloutBlocked::RetentionUnprovable {
                    digest: artifact.digest.clone(),
                    detail,
                })?;
            let observed = format!("sha256:{:x}", Sha256::digest(&bytes));
            if observed != artifact.digest {
                return Err(RolloutBlocked::RetentionUnprovable {
                    digest: artifact.digest.clone(),
                    detail: format!(
                        "the registry served content whose digest is {observed}; the catalog \
                         records {} for image {}",
                        artifact.digest, artifact.image_id
                    ),
                });
            }
        }
        Ok(())
    }

    // ── the fenced flip ─────────────────────────────────────────────────────

    /// **Step 5.** Pause admission, then prove the pause by attempting a
    /// dispatch and requiring a refusal.
    ///
    /// The pause row is written first and then *disbelieved*: `probe` is
    /// dispatched through [`Self::attempt_dispatch`], the same path a task run
    /// takes, and anything other than
    /// [`DispatchOutcome::RefusedByAdmissionPause`] blocks the cutover with
    /// [`RolloutBlocked::AdmissionPauseIneffective`]. Writing the row without
    /// wiring the refusal therefore does not produce a paused cutover; it
    /// produces a blocked one.
    ///
    /// # Errors
    ///
    /// [`RolloutBlocked::AdmissionUnavailable`] or
    /// [`RolloutBlocked::AdmissionPauseIneffective`].
    pub async fn pause_admission(
        &self,
        reason: &str,
        probe: &DispatchProbe,
    ) -> Result<(), RolloutBlocked> {
        self.guard(RolloutStep::AdmissionPaused)?;
        self.admission
            .pause(reason)
            .await
            .map_err(RolloutBlocked::AdmissionUnavailable)?;
        match self
            .attempt_dispatch(&probe.task_run_id, &probe.image)
            .await
        {
            DispatchOutcome::RefusedByAdmissionPause => {
                self.record(RolloutStep::AdmissionPaused);
                Ok(())
            }
            other => {
                warn!(
                    outcome = ?other,
                    "task_run_resize_rollout: the pause row was written but dispatch was not refused"
                );
                Err(RolloutBlocked::AdmissionPauseIneffective)
            }
        }
    }

    /// **Step 6.** Prove the drain: zero live task-run Pods AND zero
    /// nonterminal resize/lease rows.
    ///
    /// **This is the production caller of
    /// [`BuildPodPermitRepository::list_nonterminal_resize`].** The rows are
    /// returned, not counted, so the block names task-run ids and lifecycle
    /// states.
    ///
    /// The Pod census runs first because it is the dimension PostgreSQL cannot
    /// answer, and because an unavailable apiserver must not be laundered into
    /// "no Pods" by a subsequent successful row read.
    ///
    /// # Errors
    ///
    /// [`RolloutBlocked::PodCensusUnavailable`], [`RolloutBlocked::LiveTaskRunPods`],
    /// [`RolloutBlocked::PermitsUnavailable`] or
    /// [`RolloutBlocked::NonterminalResizeRows`]. None of them is ever "drained".
    pub async fn prove_drained(&self) -> Result<(), RolloutBlocked> {
        self.guard(RolloutStep::DrainProven)?;
        self.probe_drain().await?;
        self.record(RolloutStep::DrainProven);
        Ok(())
    }

    /// The drain proof without the journal entry.
    ///
    /// # Errors
    ///
    /// As [`Self::prove_drained`].
    pub async fn probe_drain(&self) -> Result<(), RolloutBlocked> {
        let live = self
            .pods
            .live_task_run_pods()
            .await
            .map_err(RolloutBlocked::PodCensusUnavailable)?;
        if !live.is_empty() {
            return Err(RolloutBlocked::LiveTaskRunPods(live));
        }

        let nonterminal = self
            .permits
            .list_nonterminal_resize()
            .await
            .map_err(|error| RolloutBlocked::PermitsUnavailable(error.to_string()))?;
        if !nonterminal.is_empty() {
            return Err(RolloutBlocked::NonterminalResizeRows(
                nonterminal.iter().map(NonterminalResizeRow::from).collect(),
            ));
        }
        Ok(())
    }

    /// **Step 7.** Flip the authority mode, behind the drain proof and behind
    /// `set_mode`'s own transactional fence.
    ///
    /// Two fences, and both are load-bearing. [`Self::prove_drained`] catches
    /// what PostgreSQL cannot see and names what it finds;
    /// [`LauncherAuthorityModeRepository::set_mode`] catches what appeared
    /// between the unlocked census and the locked one, holding the same
    /// `build_pod_permit_pools` row lock admission takes before it inserts.
    ///
    /// # Errors
    ///
    /// [`RolloutBlocked::AuthorityDrainRefused`] when the fenced count refused,
    /// plus the authority read/epoch failures. `Unchanged` — a same-mode replay
    /// — is an error here rather than a success: a cutover that did not move
    /// the mode did not do its job.
    pub async fn flip_authority_mode(
        &self,
        expected_epoch: i64,
        next: LauncherAuthorityProtocol,
    ) -> Result<i64, RolloutBlocked> {
        self.guard(RolloutStep::AuthorityModeFlipped)?;
        match self.authority.set_mode(expected_epoch, next).await {
            SetLauncherAuthorityModeResult::Flipped { row, previous, .. } => {
                self.record(RolloutStep::AuthorityModeFlipped);
                info!(
                    previous = previous.as_wire(),
                    next = next.as_wire(),
                    epoch = row.epoch,
                    "task_run_resize_rollout: authority mode flipped behind a proven drain"
                );
                Ok(row.epoch)
            }
            SetLauncherAuthorityModeResult::Unchanged { row, .. } => {
                Err(RolloutBlocked::AuthorityModeUnchanged(row.mode))
            }
            SetLauncherAuthorityModeResult::DrainNotEmpty { drain, .. } => {
                Err(RolloutBlocked::AuthorityDrainRefused { census: drain })
            }
            SetLauncherAuthorityModeResult::EpochConflict { row } => {
                Err(RolloutBlocked::AuthorityEpochConflict { epoch: row.epoch })
            }
            SetLauncherAuthorityModeResult::Uninitialized => {
                Err(RolloutBlocked::AuthorityUninitialized)
            }
            SetLauncherAuthorityModeResult::Unavailable => Err(RolloutBlocked::AuthorityUnavailable),
        }
    }

    /// **Step 8.** Resume admission — only after a confirmed flip.
    ///
    /// The prerequisite is [`RolloutStep::AuthorityModeFlipped`], which is
    /// journaled by [`Self::flip_authority_mode`] and journaled *only* when the
    /// compare-and-swap returned [`SetLauncherAuthorityModeResult::Flipped`].
    /// Resuming ahead of the flip is [`RolloutBlocked::StepOutOfOrder`].
    ///
    /// # Errors
    ///
    /// [`RolloutBlocked::AdmissionUnavailable`] or
    /// [`RolloutBlocked::StepOutOfOrder`].
    pub async fn resume_admission(&self) -> Result<(), RolloutBlocked> {
        self.guard(RolloutStep::AdmissionResumed)?;
        self.admission
            .resume()
            .await
            .map_err(RolloutBlocked::AdmissionUnavailable)?;
        self.record(RolloutStep::AdmissionResumed);
        Ok(())
    }

    // ── the two whole sequences ─────────────────────────────────────────────

    /// The forward cutover, `leaf-v1` → `resize-v2`, in order.
    ///
    /// # Errors
    ///
    /// Any [`RolloutBlocked`]. Whatever the block, the mode is unchanged and —
    /// for every block after [`RolloutStep::AdmissionPaused`] — admission stays
    /// paused. The journal records exactly how far it got.
    pub async fn activate(&self, plan: &RolloutPlan<'_>) -> Result<i64, RolloutBlocked> {
        self.freeze_catalog_mutation()?;
        self.sign_legacy_inventory().await?;
        self.deploy_protocol_aware_server().await?;
        self.rebuild_and_catalog_as_resize_v2(LauncherAuthorityProtocol::ResizeV2)
            .await?;
        self.verify_retention(plan.retained).await?;
        self.pause_admission(plan.reason, &plan.probe).await?;
        self.prove_drained().await?;
        let epoch = self
            .flip_authority_mode(plan.expected_epoch, LauncherAuthorityProtocol::ResizeV2)
            .await?;
        self.resume_admission().await?;
        Ok(epoch)
    }

    /// The reverse, `resize-v2` → `leaf-v1`.
    ///
    /// Ordered identically from [`RolloutStep::RetentionVerified`] onward, and
    /// gated on the same two drain dimensions. The retention and allowlist
    /// checks run **first**, before anything is paused, and their failure is
    /// what "rollback is blocked" means: no mode flip, no resume, no Pod.
    ///
    /// # Errors
    ///
    /// Any [`RolloutBlocked`]. When a required artifact or the allowlist is
    /// unavailable the block happens before the flip, so the mode is untouched;
    /// when it happens after [`Self::pause_admission`], admission stays paused
    /// because [`Self::resume_admission`] is never reached.
    pub async fn rollback(&self, plan: &RolloutPlan<'_>) -> Result<i64, RolloutBlocked> {
        // The allowlist is a rollback prerequisite, not a formality: the target
        // is `leaf-v1`, under which no-handshake artifacts dispatch, and they
        // dispatch only because the signed inventory vouches for them.
        self.images
            .signed_legacy_digest_allowlist(&self.inventory)
            .await
            .map_err(RolloutBlocked::AllowlistUnavailable)?;
        self.verify_retention(plan.retained).await?;
        self.validate_catalog(LauncherAuthorityProtocol::LeafV1)
            .await?;
        self.pause_admission(plan.reason, &plan.probe).await?;
        self.prove_drained().await?;
        let epoch = self
            .flip_authority_mode(plan.expected_epoch, LauncherAuthorityProtocol::LeafV1)
            .await?;
        self.resume_admission().await?;
        Ok(epoch)
    }

    // ── dispatch ────────────────────────────────────────────────────────────

    /// Attempt one dispatch through the cutover's admission path.
    ///
    /// Order matters and is the whole content of the function:
    ///
    /// 1. the production dispatch-pause predicate, **before** the Pod plane is
    ///    touched at all;
    /// 2. the authority mode, read and never defaulted;
    /// 3. [`classify_catalog_image`], which resolves exactly one authority;
    /// 4. Pod creation;
    /// 5. a `pods/resize` PATCH **only** under `resize-v2`.
    ///
    /// A `resize-v2` image under `leaf-v1` mode never reaches (4), so no Pod is
    /// created for it. A `leaf-v1` (or allowlisted no-handshake) dispatch never
    /// reaches (5), so the count of resize PATCHes against it is zero by
    /// construction.
    pub async fn attempt_dispatch(&self, task_run_id: &str, image: &Image) -> DispatchOutcome {
        match self.admission.dispatch_is_paused().await {
            Ok(true) => return DispatchOutcome::RefusedByAdmissionPause,
            Ok(false) => {}
            Err(error) => {
                // An unknown pause state is a refusal, never an admission.
                return DispatchOutcome::RefusedByAuthority(format!(
                    "dispatch pause state unavailable: {error}"
                ));
            }
        }

        let mode = match self.read_authority().await {
            Ok(row) => row.mode,
            Err(blocked) => return DispatchOutcome::RefusedByAuthority(blocked.to_string()),
        };
        let verdict = match classify_catalog_image(mode, image, &self.inventory) {
            Ok(verdict) => verdict,
            Err(reason) => return DispatchOutcome::RefusedByCatalog(reason),
        };

        if let Err(error) = self.pods.create_task_run_pod(task_run_id, &image.id).await {
            return DispatchOutcome::PodPlaneRefused(error);
        }

        let authority = verdict.authority();
        if authority.launcher_owns_leaf_quota() {
            // `leaf-v1`: the launcher owns this leaf's `cpu.max`. A resize PATCH
            // here would be the second writer.
            return DispatchOutcome::Dispatched {
                authority,
                resized: false,
            };
        }
        if let Err(error) = self
            .pods
            .resize_launcher_cpu(task_run_id, BIRTH_CPU_MILLICORES)
            .await
        {
            return DispatchOutcome::PodPlaneRefused(error);
        }
        DispatchOutcome::Dispatched {
            authority,
            resized: true,
        }
    }

    /// Dispatch, recording the invariant violation if admission was paused and
    /// a Pod was nevertheless created.
    ///
    /// Used by callers that want the counter to move: the counter exists so the
    /// pause's absence is observable rather than merely assertable.
    pub async fn attempt_dispatch_observed(
        &self,
        task_run_id: &str,
        image: &Image,
    ) -> DispatchOutcome {
        let paused = self.admission.dispatch_is_paused().await.unwrap_or(false);
        let outcome = self.attempt_dispatch(task_run_id, image).await;
        if paused && matches!(outcome, DispatchOutcome::Dispatched { .. }) {
            self.dispatches_admitted_while_paused
                .fetch_add(1, Ordering::SeqCst);
            warn!(
                task_run_id,
                "task_run_resize_rollout: a dispatch was admitted while admission was paused"
            );
        }
        outcome
    }

    /// Read the authority singleton, mapping both "absent" and "unreadable" to
    /// their own blocks. Neither is a mode.
    async fn read_authority(&self) -> Result<djinn_db::LauncherAuthorityModeRow, RolloutBlocked> {
        match self.authority.read().await {
            Ok(Some(row)) => Ok(row),
            Ok(None) => Err(RolloutBlocked::AuthorityUninitialized),
            Err(error) => {
                warn!(%error, "task_run_resize_rollout: authority mode unreadable");
                Err(RolloutBlocked::AuthorityUnavailable)
            }
        }
    }
}

/// The dispatch a pause step proves itself against.
///
/// A real task-run id and a real catalog image, so the probe travels the same
/// path a production dispatch does. Nothing about it is special-cased inside
/// [`ResizeRollout::attempt_dispatch`].
#[derive(Clone, Debug)]
pub struct DispatchProbe {
    /// The task run the probe dispatch is issued for.
    pub task_run_id: String,
    /// The catalog image it would run.
    pub image: Image,
}

/// Everything one cutover run needs.
pub struct RolloutPlan<'a> {
    /// Every artifact whose continued pullability the cutover depends on.
    pub retained: &'a [RetainedArtifact],
    /// The dispatch the pause proves itself against.
    pub probe: DispatchProbe,
    /// The authority epoch this cutover was planned against.
    pub expected_epoch: i64,
    /// Operator-facing pause reason.
    pub reason: &'a str,
}

// ── production wiring ───────────────────────────────────────────────────────

/// Administrative pause backed by the durable `dispatch_pauses` state, with the
/// refusal predicate taken from the coordinator rather than restated.
///
/// [`Self::dispatch_is_paused`] evaluates
/// `djinn_coordinator::dispatch_pause::debug_view(&state).global`, which is
/// literally `active_global_dispatch_pause(state).is_some()` — the expression
/// the coordinator's dispatch loop guards on, expiry handling included. Copying
/// that predicate here would make the cutover's notion of "paused" free to
/// drift from the one that actually refuses task dispatch.
pub struct DurableAdmissionControl {
    db: Database,
    events: djinn_core::events::EventBus,
    paused_by: String,
}

impl DurableAdmissionControl {
    /// Bind to a database, recording `paused_by` on the pause it writes.
    #[must_use]
    pub fn new(db: Database, events: djinn_core::events::EventBus, paused_by: &str) -> Self {
        Self {
            db,
            events,
            paused_by: paused_by.to_owned(),
        }
    }

    fn repository(&self) -> djinn_db::DispatchPauseRepository {
        djinn_db::DispatchPauseRepository::new(self.db.clone(), self.events.clone())
    }
}

#[async_trait]
impl AdmissionControl for DurableAdmissionControl {
    async fn pause(&self, reason: &str) -> Result<(), String> {
        self.repository()
            .pause(
                djinn_db::DispatchPauseTarget::global(),
                djinn_core::models::DispatchPause {
                    paused_by: self.paused_by.clone(),
                    paused_at: now_rfc3339(),
                    reason: reason.to_owned(),
                    // Never expiring. A cutover pause that lapses on a timer
                    // would resume admission mid-flip.
                    expires_at: None,
                },
            )
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    async fn resume(&self) -> Result<(), String> {
        self.repository()
            .resume(djinn_db::DispatchPauseTarget::global())
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    async fn dispatch_is_paused(&self) -> Result<bool, String> {
        let state = djinn_coordinator::dispatch_pause::load_dispatch_pause_state(
            self.db.clone(),
            self.events.clone(),
        )
        .await
        .map_err(|error| error.to_string())?;
        Ok(djinn_coordinator::dispatch_pause::debug_view(&state).global)
    }
}

/// A registry probe over plain HTTP(S), speaking the OCI distribution manifest
/// API.
///
/// `GET /v2/<repository>/manifests/<reference>` with the manifest media types
/// the registry needs to see in `Accept`. The response *body* is what gets
/// hashed — not a header the registry supplies — so a registry that reports one
/// digest and serves another does not pass.
pub struct HttpRegistryProbe {
    base_url: String,
    client: reqwest::Client,
}

impl HttpRegistryProbe {
    /// Bind to a registry base URL, e.g. `http://registry:5000`.
    #[must_use]
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            client: reqwest::Client::new(),
        }
    }
}

/// The `Accept` header an OCI registry needs to return a v2 or OCI manifest
/// rather than a converted v1 one.
const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.manifest.v1+json, \
     application/vnd.oci.image.index.v1+json, \
     application/vnd.docker.distribution.manifest.v2+json, \
     application/vnd.docker.distribution.manifest.list.v2+json";

#[async_trait]
impl RegistryProbe for HttpRegistryProbe {
    async fn fetch_manifest(&self, repository: &str, reference: &str) -> Result<Vec<u8>, String> {
        let url = format!("{}/v2/{repository}/manifests/{reference}", self.base_url);
        let response = self
            .client
            .get(&url)
            .header(reqwest::header::ACCEPT, MANIFEST_ACCEPT)
            .send()
            .await
            .map_err(|error| format!("GET {url}: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("GET {url}: {status}"));
        }
        response
            .bytes()
            .await
            .map(|bytes| bytes.to_vec())
            .map_err(|error| format!("GET {url}: reading the manifest body: {error}"))
    }
}

/// RFC3339 now, matching what the control-plane pause tool writes.
fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .expect("a UTC timestamp always formats as RFC3339")
}
