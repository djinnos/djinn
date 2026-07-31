// Test: `eprintln!` is the skip-reason and measurement channel for the gated
// half, mirroring `djinn-k8s`'s live harnesses.
#![allow(clippy::print_stderr)]
//! The mixed-version launcher-authority matrix (omp4, proposal 3i92).
//!
//! Three REAL image classes against four server modes, with two properties to
//! prove across the cross product:
//!
//! 1. **Exactly one quota authority for every admitted Pod** — never two, never
//!    zero.
//! 2. **Rejection before shell execution** for every incompatible combination.
//!
//! # The matrix is derived, never listed
//!
//! [`matrix`] is `ImageClass::ALL` x `ServerMode::ALL` and nothing else. There
//! is no hand-written list of cells to fall out of date, and [`expectation`] is
//! an exhaustive `match` over the pair with no wildcard arm — so a fourth image
//! class or a fifth server mode is a COMPILE error until somebody states what it
//! should do. `matrix_is_the_whole_cross_product` closes the other direction: a
//! variant added to the enum but not to `ALL` trips it.
//!
//! # What "old" means here, precisely
//!
//! [`ServerMode::Old`] is a server that predates the protocol. Such a server has
//! no authority-mode row to consult and no `apply_launcher_authority_protocol`
//! to call, so this harness emulates it by **not invoking the admission seam at
//! all and rendering no `DJINN_LAUNCHER_AUTHORITY_PROTOCOL`**. The launcher then
//! takes its documented absent-means-`leaf-v1` default and writes leaf quota.
//! That is the real compatibility guarantee the cutover depends on: an image
//! that declares `resize-v2` degrades to leaf authority on a server that cannot
//! read the declaration, rather than ending up with nobody writing quota.
//!
//! # What is real
//!
//! * The three images are three independently built artifacts with three
//!   distinct digests (`tests/fixtures/resize-matrix/`). The legacy launcher is
//!   COMPILED from a pre-protocol revision, and
//!   `legacy_image_binary_predates_the_protocol` scans the binary inside the
//!   built image for both wire strings.
//! * Admission goes through the production pair
//!   `LauncherAuthorityModeRepository::admit_declared_protocol` (a real
//!   PostgreSQL read of `launcher_authority_mode`) composed with
//!   `admit_with_legacy_inventory` — the same two calls in the same order that
//!   `ProjectRepository::admit_dispatch_image` makes. No `Fake`, no `Mock`.
//!   `Database::open_in_memory` is a template-cloned REAL PostgreSQL database
//!   despite the name.
//! * The legacy allowlist is a genuinely Ed25519-signed document verified by
//!   `LegacyDigestInventory::from_signed_document`.
//! * Every resize PATCH body is `pod_resize::build_resize_patch`, applied with
//!   strategic-merge semantics against the `resize` subresource.
//! * Every CPU comparison goes through `pod_resize::CpuLimit`, i.e. parsed
//!   millicores.
//!
//! # The two flip tests go through the production driver (task `li91`)
//!
//! `omp4` branched before `eeky` (#2858) and `zpen` (#2859) merged, so it
//! composed the mode flip out of `LauncherAuthorityModeRepository::set_mode`
//! and could not call the cutover preflight at all. That is not a cosmetic
//! difference. `set_mode`'s own fence refuses a seeded nonterminal row anyway —
//! with a BARE CENSUS — so a test that flips through `set_mode` passes whether
//! or not `ResizeRollout`, the production rollout driver, is wired to anything.
//! `ResizeRollout::prove_drained` refuses the same row NAMING it
//! (`task_run_id`, `state`, `pod_uid`), so asserting on the named rows is what
//! makes deleting the driver detectable.
//!
//! Both flips are therefore driven by [`ResizeRollout`], and both are GATED on
//! `deploy/preflight/cutover-preflight.sh` — the deploy-time gate itself, run
//! as a subprocess over a live `helm template` render, not a second copy of any
//! of its six checks. `the_cutover_preflight_gates_the_flip_and_one_defect_class_refuses_it`
//! mutates that render by exactly one line and requires the flip to be refused.
//!
//! # Confirmation site
//!
//! Confirmation for the native sidecar comes ONLY from
//! `status.initContainerStatuses[name=cgroup-launcher]`. The regular-container
//! status list is never consulted, and
//! `no_helper_in_this_suite_reads_the_regular_container_status_list` is a
//! source-level gate over this file that fails if the token so much as appears.
//! The live half backs the gate up with a Pod that carries a MISLEADING,
//! perfectly-matching regular-container status and no init-container status at
//! all: a reader that fell back to it would call that Pod confirmed.
//!
//! # Running the live half
//!
//! ```text
//! scripts/kind/setup-resize-matrix-cluster.sh up
//! tests/fixtures/resize-matrix/build.sh djinn-resize-omp4
//! # BEFORE cargo test, never from inside it: the gate script builds this
//! # binary itself when CUTOVER_PREFLIGHT_BIN is unset, and a `cargo build`
//! # launched from a running `cargo test` blocks on the build-directory lock.
//! (cd server && cargo build -p djinn-k8s --bin cutover-preflight)
//! DJINN_TEST_RESIZE_MATRIX=1 cargo test -p djinn-server \
//!     --test task_run_resize_mixed_version -- --ignored --test-threads=1
//! scripts/kind/setup-resize-matrix-cluster.sh down       # ALWAYS, pass or fail
//! scripts/kind/setup-resize-matrix-cluster.sh selfcheck
//! ```
//!
//! The preflight-gated proof additionally needs `helm` and `python3`; it needs
//! no cluster, so it can be run on its own.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use djinn_db::launcher_compatibility::{
    AdmissionDecision, AdmissionRejection, InventoryFault, LegacyDigestInventory,
    PreProtocolDigest, admit_with_legacy_inventory,
};
use djinn_db::{
    AcquireBuildPodPermitResult, BuildPodPermitRepository, BuildPodPermitRow,
    BuildPodResizeIdentity, CaptureBuildPodResizeIdentityResult, Database, Image, ImageRepository,
    LauncherAuthorityModeRepository, LauncherProtocolAdmission, SetLauncherAuthorityModeResult,
};
use djinn_k8s::config::KubernetesConfig;
use djinn_k8s::job::{
    COMPONENT_TASK_RUN_WORKER, LABEL_COMPONENT, LABEL_TASK_RUN_ID, build_task_run_job,
};
use djinn_k8s::launcher::{
    CgroupLauncherMode, LAUNCHER_CONTAINER_NAME, apply_launcher_authority_protocol,
    render_authority_protocol,
};
use djinn_k8s::pod_resize::{CpuLimit, build_resize_patch};
use djinn_launcher_protocol::{LEAF_V1_WIRE, LauncherAuthorityProtocol, RESIZE_V2_WIRE};
use djinn_server::task_run_resize_rollout::{
    DispatchProbe, DurableAdmissionControl, LiveTaskRunPod, RegistryProbe, ResizeRollout,
    RolloutBlocked, RolloutStep, TaskRunPodPlane,
};
use serde_json::{Value, json};

// ===========================================================================
// The two axes.
// ===========================================================================

/// One of the three REAL image classes. Not a configuration of one image: the
/// three are independently built artifacts with three distinct digests, and
/// `tests/fixtures/resize-matrix/build.sh` refuses to finish if two of them
/// collide.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ImageClass {
    /// A launcher compiled from a revision that predates the handshake. It
    /// makes no declaration because it cannot.
    LegacyNoHandshake,
    /// Current launcher, declaring `leaf-v1`.
    LeafV1,
    /// Current launcher, declaring `resize-v2`.
    ResizeV2,
}

impl ImageClass {
    const ALL: [Self; 3] = [Self::LegacyNoHandshake, Self::LeafV1, Self::ResizeV2];

    /// Position in [`Self::ALL`]. Exhaustive on purpose: a fourth variant forces
    /// a fourth arm with a fourth ordinal, and `ALL` must then grow or
    /// `matrix_is_the_whole_cross_product` indexes past its end.
    const fn ordinal(self) -> usize {
        match self {
            Self::LegacyNoHandshake => 0,
            Self::LeafV1 => 1,
            Self::ResizeV2 => 2,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::LegacyNoHandshake => "legacy",
            Self::LeafV1 => "leaf-v1",
            Self::ResizeV2 => "resize-v2",
        }
    }

    /// What the artifact declares, as the catalog would store it. `None` is the
    /// legacy class's defining property and is what routes it through the signed
    /// digest inventory instead of the declaration path.
    const fn declared(self) -> Option<LauncherAuthorityProtocol> {
        match self {
            Self::LegacyNoHandshake => None,
            Self::LeafV1 => Some(LauncherAuthorityProtocol::LeafV1),
            Self::ResizeV2 => Some(LauncherAuthorityProtocol::ResizeV2),
        }
    }

    const fn declared_wire(self) -> Option<&'static str> {
        match self.declared() {
            None => None,
            Some(protocol) => Some(protocol.as_wire()),
        }
    }
}

/// One of the four server modes the cutover walks through.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ServerMode {
    /// A server that predates the protocol: no authority-mode row, no admission
    /// seam, no rendered protocol env. See the module docs.
    Old,
    /// Protocol-aware, authority `leaf-v1`. The state a fleet sits in after the
    /// server rolls out and before activation.
    Preparation,
    /// Authority `resize-v2`.
    Activated,
    /// Back to `leaf-v1` after a `resize-v2` window. Distinct from
    /// [`Self::Preparation`] because it is reached through a drain fence that
    /// preparation never has to satisfy.
    Rollback,
}

impl ServerMode {
    const ALL: [Self; 4] = [
        Self::Old,
        Self::Preparation,
        Self::Activated,
        Self::Rollback,
    ];

    const fn ordinal(self) -> usize {
        match self {
            Self::Old => 0,
            Self::Preparation => 1,
            Self::Activated => 2,
            Self::Rollback => 3,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Old => "old",
            Self::Preparation => "preparation",
            Self::Activated => "activated",
            Self::Rollback => "rollback",
        }
    }

    /// The authority the server enforces, or `None` for a server that has no
    /// concept of one.
    const fn authority(self) -> Option<LauncherAuthorityProtocol> {
        match self {
            Self::Old => None,
            Self::Preparation | Self::Rollback => Some(LauncherAuthorityProtocol::LeafV1),
            Self::Activated => Some(LauncherAuthorityProtocol::ResizeV2),
        }
    }
}

/// Which component wrote the Pod's CPU quota. Exactly one, always.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Authority {
    /// The launcher wrote a numeric quota into the invocation leaf's `cpu.max`,
    /// and no `pods/resize` PATCH was ever issued against the Pod.
    Leaf,
    /// Pod resize moved the sidecar's limit, and the launcher wrote NOTHING into
    /// the leaf — not even `max` as a rewrite.
    Resize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Admitted(Authority),
    /// Refused at admission, before any Job or Pod object exists and therefore
    /// before any shell in that Pod could run.
    RefusedBeforeDispatch,
}

/// The expected outcome of one cell.
///
/// Exhaustive over the pair with NO wildcard arm. That is the compile-time half
/// of AC1: adding a variant to either axis stops this function compiling until
/// somebody decides what the new combination means.
const fn expectation(image: ImageClass, mode: ServerMode) -> Outcome {
    use Authority::{Leaf, Resize};
    use ImageClass::{LeafV1, LegacyNoHandshake, ResizeV2};
    use Outcome::{Admitted, RefusedBeforeDispatch};
    use ServerMode::{Activated, Old, Preparation, Rollback};

    match (image, mode) {
        // A pre-protocol server reads no declaration and renders no protocol
        // env, so every class falls to the launcher's absent-means-leaf-v1
        // default. This row is the compatibility guarantee that lets the server
        // roll out ahead of any image change.
        (LegacyNoHandshake, Old) | (LeafV1, Old) | (ResizeV2, Old) => Admitted(Leaf),

        // No declaration is admitted as leaf authority ONLY for an exact,
        // signed, pre-inventoried digest, and only under a leaf-v1 mode.
        (LegacyNoHandshake, Preparation | Rollback) => Admitted(Leaf),
        // ... and refused under resize-v2, because a launcher that cannot
        // handshake cannot be told to stop writing leaf quota, and resize would
        // then be the second authority on the same Pod.
        (LegacyNoHandshake, Activated) => RefusedBeforeDispatch,

        (LeafV1, Preparation | Rollback) => Admitted(Leaf),
        // `ProtocolMismatch`: the artifact declares leaf-v1 and would write leaf
        // quota while resize also moved the limit.
        (LeafV1, Activated) => RefusedBeforeDispatch,

        // The one resize-authority cell in the matrix.
        (ResizeV2, Activated) => Admitted(Resize),
        // A resize-v2 image under a leaf-v1 mode does not dispatch: its launcher
        // would decline to write leaf quota and nothing else would write any,
        // leaving the Pod with ZERO authorities.
        (ResizeV2, Preparation | Rollback) => RefusedBeforeDispatch,
    }
}

/// One cell of the cross product.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cell {
    image: ImageClass,
    mode: ServerMode,
    expected: Outcome,
}

impl Cell {
    fn name(self) -> String {
        format!("{}@{}", self.image.as_str(), self.mode.as_str())
    }
}

const MATRIX_LEN: usize = ImageClass::ALL.len() * ServerMode::ALL.len();

/// The matrix, derived from the two `ALL` arrays at construction time.
///
/// The RETURN TYPE is the completeness proof: it is spelled in terms of the two
/// axes' lengths, so a variant added to either `ALL` widens the array and a cell
/// cannot be deleted without deleting a variant.
fn matrix() -> [Cell; MATRIX_LEN] {
    std::array::from_fn(|index| {
        let image = ImageClass::ALL[index / ServerMode::ALL.len()];
        let mode = ServerMode::ALL[index % ServerMode::ALL.len()];
        Cell {
            image,
            mode,
            expected: expectation(image, mode),
        }
    })
}

// ===========================================================================
// Hermetic half: the matrix's shape.
// ===========================================================================

/// **AC1.** Every pair appears exactly once, and every variant of both enums is
/// reachable through its `ALL`.
///
/// The ordinal round trip is what catches a variant added to the enum but not to
/// `ALL`: `ordinal` is exhaustive, so the new variant gets an index, and the
/// index is then out of bounds.
#[test]
fn matrix_is_the_whole_cross_product() {
    for image in ImageClass::ALL {
        assert_eq!(
            ImageClass::ALL[image.ordinal()],
            image,
            "{image:?} is missing from ImageClass::ALL, or is listed out of ordinal order; the \
             matrix would silently skip every cell that names it",
        );
    }
    for mode in ServerMode::ALL {
        assert_eq!(
            ServerMode::ALL[mode.ordinal()],
            mode,
            "{mode:?} is missing from ServerMode::ALL, or is listed out of ordinal order",
        );
    }

    let cells = matrix();
    assert_eq!(cells.len(), MATRIX_LEN);
    assert_eq!(
        MATRIX_LEN, 12,
        "the matrix is 3 image classes x 4 server modes; if this changed on purpose, the count \
         here is the one place that says so out loud",
    );

    let pairs: BTreeSet<(usize, usize)> = cells
        .iter()
        .map(|cell| (cell.image.ordinal(), cell.mode.ordinal()))
        .collect();
    assert_eq!(
        pairs.len(),
        MATRIX_LEN,
        "a pair appears twice, so some pair does not appear at all: {cells:?}",
    );
}

/// **AC1.** Every cell has an explicit expected outcome, and both outcome kinds
/// are actually exercised.
///
/// A matrix in which every cell expected the same thing would pass every other
/// assertion in this file while testing nothing.
#[test]
fn every_cell_has_an_expectation_and_both_outcomes_occur() {
    let cells = matrix();
    let admitted_leaf = cells
        .iter()
        .filter(|cell| cell.expected == Outcome::Admitted(Authority::Leaf))
        .count();
    let admitted_resize = cells
        .iter()
        .filter(|cell| cell.expected == Outcome::Admitted(Authority::Resize))
        .count();
    let refused = cells
        .iter()
        .filter(|cell| cell.expected == Outcome::RefusedBeforeDispatch)
        .count();

    assert_eq!(admitted_leaf + admitted_resize + refused, MATRIX_LEN);
    assert!(
        admitted_leaf > 0 && admitted_resize > 0 && refused > 0,
        "the matrix must exercise leaf authority, resize authority AND refusal; \
         leaf={admitted_leaf} resize={admitted_resize} refused={refused}",
    );
}

// ===========================================================================
// Hermetic half: admission, against real PostgreSQL.
// ===========================================================================

/// The signed inventory the legacy class is vouched for by.
///
/// Genuinely signed with a freshly generated Ed25519 key and verified by
/// `LegacyDigestInventory::from_signed_document`, so the whole verification path
/// runs — a hand-constructed `LegacyDigestInventory::verified(..)` would skip
/// exactly the code the fence depends on.
fn signed_inventory(digests: &[&str]) -> LegacyDigestInventory {
    use base64::Engine as _;
    use ring::signature::KeyPair as _;

    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng)
        .expect("generate an ed25519 key for the test inventory");
    let key = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
        .expect("parse the generated key");

    let document = serde_json::to_vec(&json!({
        "schema_version": 1,
        "issuer": "omp4-mixed-version-matrix",
        "issued_at": "2026-07-31T00:00:00Z",
        "digests": digests,
    }))
    .expect("the inventory document serializes");

    let signature = key.sign(&document);
    let base64 = base64::engine::general_purpose::STANDARD;
    LegacyDigestInventory::from_signed_document(
        &document,
        Some(&base64.encode(key.public_key().as_ref())),
        Some(&base64.encode(signature.as_ref())),
    )
}

/// A syntactically valid, immutable digest that stands in for one image class's
/// content address in the hermetic half. The live half uses the real one.
fn stub_digest(class: ImageClass) -> String {
    format!("sha256:{:0>64}", format!("{}0", class.ordinal() + 1))
}

/// Drive the production admission pair for one cell.
///
/// `LauncherAuthorityModeRepository::admit_declared_protocol` reads the mode out
/// of real PostgreSQL, and `admit_with_legacy_inventory` composes that verdict
/// with the signed inventory. This is the same two calls, in the same order,
/// that `ProjectRepository::admit_dispatch_image` makes on the dispatch path —
/// reached directly so the cell does not also depend on project-row plumbing
/// that has nothing to do with the property under test.
async fn admit(
    modes: &LauncherAuthorityModeRepository,
    image: ImageClass,
    digest: Option<&str>,
    inventory: &LegacyDigestInventory,
) -> Result<AdmissionDecision, AdmissionRejection> {
    let verdict: LauncherProtocolAdmission =
        modes.admit_declared_protocol(image.declared_wire()).await;
    admit_with_legacy_inventory(verdict, digest, inventory)
}

/// A real `task_runs` row, because `build_pod_permits.task_run_id` carries a
/// foreign key to it.
///
/// Every repository here is the production one against real PostgreSQL: the
/// permit row whose presence the drain fence counts is reachable only through
/// the same chain dispatch walks, and a `Fake` would have let the fence pass
/// with nothing behind it.
async fn seed_task_run(db: &Database) -> String {
    use djinn_db::{
        CreateTaskRunParams, EffectiveCreatorProvenance, ProjectRepository, TaskRepository,
        TaskRunRepository, UserRepository,
    };

    let unique = uuid::Uuid::now_v7();
    let user = UserRepository::new(db.clone())
        .upsert_from_github(
            i64::try_from(unique.as_u128() % 8_000_000_000_000_000_000).expect("github id"),
            &format!("resize-matrix-{unique}"),
            None,
            None,
        )
        .await
        .expect("seed user");
    let project = ProjectRepository::new(db.clone(), djinn_core::events::EventBus::noop())
        .create(
            &format!("resize-matrix-{unique}"),
            "djinnos",
            &format!("resize-matrix-{unique}"),
        )
        .await
        .expect("seed project");
    let task = TaskRepository::new(db.clone(), djinn_core::events::EventBus::noop())
        .create_in_project_with_provenance(
            &project.id,
            None,
            EffectiveCreatorProvenance::explicit_user_id(&user.id),
            "resize matrix",
            "description",
            "design",
            "task",
            2,
            "owner",
            None,
            None,
        )
        .await
        .expect("seed task");

    let task_run_id = uuid::Uuid::now_v7().to_string();
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: &task_run_id,
            project_id: &project.id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .expect("seed task run");
    task_run_id
}

/// Put the server into `mode`, satisfying the drain fence.
async fn force_mode(modes: &LauncherAuthorityModeRepository, mode: LauncherAuthorityProtocol) {
    let row = modes
        .read()
        .await
        .expect("read the authority mode")
        .expect("migration 167 seeds the singleton row");
    if row.mode == mode {
        return;
    }
    match modes.set_mode(row.epoch, mode).await {
        SetLauncherAuthorityModeResult::Flipped { .. }
        | SetLauncherAuthorityModeResult::Unchanged { .. } => {}
        other => panic!("could not reach {mode}: {other:?}"),
    }
}

/// **AC5.** Every cell's admission outcome, decided by the production functions
/// against a real database and a real signed inventory.
///
/// This is the hermetic shadow of the live matrix: it proves the DECISION for
/// all twelve cells. The live half proves the decision was acted on before a
/// shell ran.
#[tokio::test]
async fn every_cell_admits_exactly_as_the_matrix_expects() {
    let db = Database::ephemeral().await.expect("a real test database");
    let modes = LauncherAuthorityModeRepository::new(db.clone());

    let legacy_digest = stub_digest(ImageClass::LegacyNoHandshake);
    let inventory = signed_inventory(&[&legacy_digest]);

    for cell in matrix() {
        let Some(authority) = cell.mode.authority() else {
            // `Old` has no admission seam at all — see the module docs. Its
            // render behaviour is asserted by
            // `an_old_server_renders_no_protocol_declaration`.
            continue;
        };
        force_mode(&modes, authority).await;

        let digest = stub_digest(cell.image);
        let decision = admit(&modes, cell.image, Some(&digest), &inventory).await;

        match cell.expected {
            Outcome::Admitted(Authority::Leaf) => assert_eq!(
                decision,
                Ok(AdmissionDecision::Admitted(
                    LauncherAuthorityProtocol::LeafV1
                )),
                "cell {} must admit as leaf authority",
                cell.name(),
            ),
            Outcome::Admitted(Authority::Resize) => assert_eq!(
                decision,
                Ok(AdmissionDecision::Admitted(
                    LauncherAuthorityProtocol::ResizeV2
                )),
                "cell {} must admit as resize authority",
                cell.name(),
            ),
            Outcome::RefusedBeforeDispatch => assert!(
                decision.is_err(),
                "cell {} must be refused, got {decision:?}",
                cell.name(),
            ),
        }
    }
}

/// **AC5, mutation 2.** A no-handshake image whose digest is NOT in the signed
/// allowlist is refused under EVERY mode, including the leaf-v1 ones where an
/// inventoried digest would have been admitted.
#[tokio::test]
async fn an_uninventoried_no_handshake_digest_is_refused_under_every_mode() {
    let db = Database::ephemeral().await.expect("a real test database");
    let modes = LauncherAuthorityModeRepository::new(db.clone());

    let inventoried = stub_digest(ImageClass::LegacyNoHandshake);
    let inventory = signed_inventory(&[&inventoried]);
    let stranger = format!("sha256:{}", "b".repeat(64));

    for mode in ServerMode::ALL {
        let Some(authority) = mode.authority() else {
            continue;
        };
        force_mode(&modes, authority).await;

        // The control: the inventoried digest is admitted under leaf-v1. Without
        // it, "everything is refused" would satisfy this test.
        let vouched = admit(
            &modes,
            ImageClass::LegacyNoHandshake,
            Some(&inventoried),
            &inventory,
        )
        .await;
        if authority == LauncherAuthorityProtocol::LeafV1 {
            assert_eq!(
                vouched,
                Ok(AdmissionDecision::Admitted(
                    LauncherAuthorityProtocol::LeafV1
                )),
                "the inventoried digest must still be admitted under {mode:?}",
            );
        }

        let refused = admit(
            &modes,
            ImageClass::LegacyNoHandshake,
            Some(&stranger),
            &inventory,
        )
        .await;
        assert!(
            matches!(refused, Err(AdmissionRejection::UninventoriedDigest(_)))
                || matches!(
                    refused,
                    Err(AdmissionRejection::MissingDeclarationUnderMode { .. })
                ),
            "an uninventoried no-handshake digest must be refused under {mode:?}, got {refused:?}",
        );
    }
}

/// **AC5, mutation 3.** An allowlist entry expressed as a mutable tag is
/// refused, and so is a dispatch that presents one.
///
/// This falls out of `PreProtocolDigest::parse` rather than being a separate
/// check, which is why the rewiring to `decide_admission` was worth doing: the
/// inventory and the dispatch path agree about what a digest is because they use
/// the same parser.
#[test]
fn a_mutable_tag_is_not_a_digest_anywhere() {
    for tag in [
        "djinn-agent-runtime:v1",
        "latest",
        "sha256:short",
        // Upper-case hex is a different string for the same content and is
        // refused rather than folded: two spellings of one digest is two
        // allowlist entries to keep in sync.
        &format!("sha256:{}", "A".repeat(64)),
        "",
    ] {
        assert!(
            PreProtocolDigest::parse(tag).is_err(),
            "{tag:?} must not parse as an immutable digest",
        );
    }
    assert!(
        PreProtocolDigest::parse(&format!("sha256:{}", "a".repeat(64))).is_ok(),
        "a well-formed digest must still parse, or the assertions above are vacuous",
    );

    // And an inventory that lists a tag is UNUSABLE, not quietly shorter.
    let inventory = signed_inventory(&["djinn-agent-runtime:v1"]);
    assert!(
        matches!(
            inventory,
            LegacyDigestInventory::Unusable(InventoryFault::DigestMalformed(_))
        ),
        "an inventory containing a mutable tag must be unusable, got {inventory:?}",
    );
}

/// **AC5, mutation 4 / AC3 "zero authorities".** An explicit `resize-v2` image
/// under a leaf-v1 mode does not dispatch.
#[tokio::test]
async fn a_resize_v2_image_does_not_dispatch_under_leaf_v1() {
    let db = Database::ephemeral().await.expect("a real test database");
    let modes = LauncherAuthorityModeRepository::new(db.clone());
    force_mode(&modes, LauncherAuthorityProtocol::LeafV1).await;

    let inventory = signed_inventory(&[]);
    let decision = admit(
        &modes,
        ImageClass::ResizeV2,
        Some(&stub_digest(ImageClass::ResizeV2)),
        &inventory,
    )
    .await;

    assert!(
        matches!(
            decision,
            Err(AdmissionRejection::Declared(
                LauncherProtocolAdmission::ProtocolMismatch { .. }
            ))
        ),
        "a resize-v2 image under leaf-v1 must be a protocol mismatch, got {decision:?}",
    );
}

// ===========================================================================
// Hermetic half: the render.
// ===========================================================================

fn harness_config(image: &str) -> KubernetesConfig {
    KubernetesConfig {
        namespace: NAMESPACE.into(),
        image: image.into(),
        image_pull_policy: "IfNotPresent".into(),
        cgroup_launcher_mode: CgroupLauncherMode::Required,
        task_run_cgroup_writable_enabled: true,
        kueue_armed: false,
        // Requests low so several cells fit on one kind node; the LIMIT is what
        // the launcher's lease and the resize ceiling derive from, and it is
        // deliberately the stock bare `4` (see the millicore test).
        cpu_request: "100m".into(),
        cpu_limit: "4".into(),
        memory_request: "64Mi".into(),
        memory_limit: "1Gi".into(),
        ..KubernetesConfig::for_testing()
    }
}

/// The rendered Job for one cell, as JSON, plus its task-run id.
fn render_cell_job(config: &KubernetesConfig, cell: Cell) -> (Value, String) {
    render_cell_job_as(config, cell, uuid::Uuid::now_v7().to_string())
}

/// As [`render_cell_job`], for a task-run id chosen by the caller — used where a
/// durable `task_runs` row has to exist for the same id the Pod is labelled with.
fn render_cell_job_as(
    config: &KubernetesConfig,
    cell: Cell,
    task_run_id: String,
) -> (Value, String) {
    let task_run_id: uuid::Uuid = task_run_id.parse().expect("a task-run id is a uuid");
    let mut job = build_task_run_job(
        config,
        &task_run_id,
        "resize-matrix-project",
        &format!("djinn-taskrun-{task_run_id}"),
        &config.image,
        &[],
        None,
        false,
        None,
    );

    if let Some(authority) = cell.mode.authority() {
        // The production render. `render_authority_protocol` is what the
        // dispatch path uses to turn an admitted decision into the protocol the
        // sidecar is told about, and it is called here for the same reason.
        let protocol =
            render_authority_protocol(Some(authority), Some(&format!("sha256:{}", "c".repeat(64))))
                .expect("a declared authority always renders");
        apply_launcher_authority_protocol(&mut job, config.cgroup_launcher_mode, protocol)
            .expect("the armed render emits a launcher sidecar");
    }

    let mut manifest = serde_json::to_value(&job).expect("the rendered Job serializes");
    manifest["apiVersion"] = Value::String("batch/v1".into());
    manifest["kind"] = Value::String("Job".into());
    (manifest, task_run_id.to_string())
}

/// Read `spec.template.spec` container env off a rendered manifest.
fn rendered_env(manifest: &Value, list: &str, container: &str, key: &str) -> Option<String> {
    manifest
        .pointer(&format!("/spec/template/spec/{list}"))?
        .as_array()?
        .iter()
        .find(|entry| entry["name"] == container)?["env"]
        .as_array()?
        .iter()
        .find(|entry| entry["name"] == key)?["value"]
        .as_str()
        .map(str::to_owned)
}

/// **AC3, "old" row.** A pre-protocol server renders NO protocol declaration, so
/// the launcher takes its absent-means-leaf-v1 default and leaf authority
/// applies. Nobody is told about resize, so resize cannot be a second authority.
#[test]
fn an_old_server_renders_no_protocol_declaration() {
    let config = harness_config("djinn-resize-matrix-leaf-v1:omp4");
    for image in ImageClass::ALL {
        let cell = Cell {
            image,
            mode: ServerMode::Old,
            expected: expectation(image, ServerMode::Old),
        };
        let (manifest, _) = render_cell_job(&config, cell);
        assert_eq!(
            rendered_env(
                &manifest,
                "initContainers",
                LAUNCHER_CONTAINER_NAME,
                "DJINN_LAUNCHER_AUTHORITY_PROTOCOL",
            ),
            None,
            "an old server has no authority mode to declare; cell {} rendered one",
            cell.name(),
        );
        assert_eq!(
            cell.expected,
            Outcome::Admitted(Authority::Leaf),
            "with no declaration the launcher default is the only authority",
        );
    }
}

/// **AC3, "resize-v2 renders a ceiling; leaf-v1 renders none".** The two
/// authorities are distinguishable in the RENDER, before any Pod exists.
///
/// The deliberate absence of a launcher CPU limit under `leaf-v1` is what makes
/// the live "zero resize PATCHes" assertion observable: a resize PATCH must
/// introduce a `limits.cpu` key that a leaf-authority Pod never has.
#[test]
fn only_resize_v2_renders_a_launcher_cpu_ceiling() {
    let config = harness_config("djinn-resize-matrix-resize-v2:omp4");

    let launcher_limits = |mode: ServerMode| -> Option<Value> {
        let cell = Cell {
            image: ImageClass::ResizeV2,
            mode,
            expected: expectation(ImageClass::ResizeV2, mode),
        };
        let (manifest, _) = render_cell_job(&config, cell);
        manifest
            .pointer("/spec/template/spec/initContainers")?
            .as_array()?
            .iter()
            .find(|entry| entry["name"] == LAUNCHER_CONTAINER_NAME)?["resources"]["limits"]
            .get("cpu")
            .cloned()
    };

    assert_eq!(
        launcher_limits(ServerMode::Preparation),
        None,
        "leaf-v1 must render NO launcher CPU limit; PR #2840's ceiling is resize-v2 only and a \
         blanket clamp would silently re-enable the nsdelegate ancestor clamp launcher.rs documents",
    );
    assert!(
        launcher_limits(ServerMode::Activated).is_some(),
        "resize-v2 must render a launcher CPU ceiling for resize to move",
    );
}

// ===========================================================================
// Hermetic half: the PATCH body and the millicore rule.
// ===========================================================================

/// **AC6, mutation 2.** The resize body touches exactly
/// `spec.initContainers[cgroup-launcher].resources.limits.cpu` and nothing else.
///
/// Enumerated rather than spot-checked: a body that also set `requests`, a
/// second field, a second init container or any `spec.containers` entry adds a
/// key this walk reports.
#[test]
fn the_resize_body_addresses_exactly_one_field() {
    let body = build_resize_patch(CpuLimit::from_millis(2_000));

    let mut paths = Vec::new();
    fn walk(value: &Value, prefix: &str, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    walk(child, &format!("{prefix}/{key}"), out);
                }
            }
            Value::Array(items) => {
                for (index, child) in items.iter().enumerate() {
                    walk(child, &format!("{prefix}/{index}"), out);
                }
            }
            leaf => out.push(format!("{prefix}={leaf}")),
        }
    }
    walk(&body, "", &mut paths);
    paths.sort();

    assert_eq!(
        paths,
        vec![
            format!("/spec/initContainers/0/name=\"{LAUNCHER_CONTAINER_NAME}\""),
            "/spec/initContainers/0/resources/limits/cpu=\"2000m\"".to_owned(),
        ],
        "the resize body must carry the merge key and the one field it changes, and nothing else",
    );
}

/// **AC8.** Every comparison is in parsed millicores.
///
/// The apiserver canonicalises `4000m` to `4`, and the repository's stock worker
/// `cpu_limit` is the bare string `"4"`. Replace `CpuLimit::parse` with string
/// equality and this goes red on the first pair.
#[test]
fn cpu_comparison_is_millicores_not_strings() {
    let pairs = [
        ("4", "4000m"),
        ("2", "2000m"),
        ("0.5", "500m"),
        ("1", "1000m"),
    ];
    for (canonical, spelled) in pairs {
        let left = CpuLimit::parse(canonical).expect("the apiserver form parses");
        let right = CpuLimit::parse(spelled).expect("the millicore form parses");
        assert_eq!(
            left, right,
            "{canonical} and {spelled} are the same quantity; a string comparison would report \
             `never reported {spelled}; last observed Some({canonical})`",
        );
        assert_ne!(
            canonical, spelled,
            "the pair must differ AS STRINGS or this test proves nothing",
        );
    }

    // And the stock config's limit really is the bare form, so the rule above is
    // load bearing rather than hypothetical.
    assert_eq!(
        harness_config("x").cpu_limit,
        "4",
        "the harness renders the stock bare-string CPU limit on purpose",
    );
}

// ===========================================================================
// Hermetic half: the confirmation-site gate and the harness guards.
// ===========================================================================

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(1)
        .expect("the server crate lives one level below the repository root")
        .to_path_buf()
}

fn this_file() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/task_run_resize_mixed_version.rs")
}

/// `text` with whole-line comments removed.
///
/// Copied from `server/tests/task_run_resize_kind.rs`, whose doc comment is the
/// canonical statement of this defect class. These are separate test binaries,
/// so the helper is duplicated rather than shared. A comment cannot call a
/// function, set a variable or arm a shell step, so it must neither satisfy a
/// positive assertion nor trip a negative one. Both directions are live here:
/// the teardown-trap gate below matched the harness script's PROSE, and the
/// 1.29 ban below sat next to a header paragraph discussing 1.29.
///
/// Only WHOLE-line comments are dropped, deliberately: a trailing comment must
/// not be able to launder the code in front of it.
fn code_lines(text: &str, comment_prefix: &str) -> String {
    text.lines()
        .filter(|line| !line.trim_start().starts_with(comment_prefix))
        .collect::<Vec<_>>()
        .join("\n")
}

/// [`code_lines`] for Rust. Covers `//`, `///` and `//!`.
fn rust_code(source: &str) -> String {
    code_lines(source, "//")
}

/// [`code_lines`] for shell and YAML, which share the `#` comment marker.
fn script_code(text: &str) -> String {
    code_lines(text, "#")
}

/// Whether `needle` appears in the harness script's CODE rather than its prose.
///
/// Split out from its guard so the guard's logic is testable against synthetic
/// input: otherwise "ignores comments" and "ignores everything" are
/// indistinguishable from a green run.
fn shell_code_contains(script: &str, needle: &str) -> bool {
    script_code(script).contains(needle)
}

/// Whether the harness script really ARMS the failure-path teardown.
///
/// This is the `1j64` defect verbatim: delete the real `trap` line, leave
/// `# trap cleanup_on_failure EXIT` behind, and a raw `contains` stays green
/// while a failed `up` leaves a cluster standing.
fn arms_the_failure_teardown(script: &str) -> bool {
    shell_code_contains(script, "trap cleanup_on_failure EXIT")
}

/// Whether the harness script SETS the measured-false 1.29 floor.
///
/// The script's header discusses 1.29 at length — it has to, that is where the
/// correction is recorded. A ban that fires on the paragraph explaining why the
/// floor moved is a ban that argues against being told why the code is the way
/// it is.
fn restores_the_measured_false_floor(script: &str) -> bool {
    shell_code_contains(script, "MIN_K8S_MINOR=29")
}

/// Whether this suite READS the one permitted confirmation site.
///
/// The needle is assembled at compile time AND required in indexing position,
/// because the token is spelled in this gate's own doc comment and in its own
/// failure message. `body.contains("initContainerStatuses")` was satisfied by
/// those, so it could never fail.
fn reads_the_init_container_status_list(source: &str) -> bool {
    rust_code(source).contains(concat!("[\"initContainer", "Statuses\"]"))
}

/// The forbidden regular-container-status spelling this suite CONSTRUCTS, if any.
fn regular_status_list_token_in(source: &str) -> Option<String> {
    let code = rust_code(source);
    [
        format!("{}{}", "container", "Statuses"),
        format!("{}_{}", "container", "statuses"),
    ]
    .into_iter()
    .find(|token| code.contains(token))
}

/// **AC7, source gate.** No helper in this suite reads the regular-container
/// status list.
///
/// The forbidden token is ASSEMBLED at compile time rather than written out, so
/// this gate does not trip on itself. `initContainerStatuses` is not a match: the
/// forbidden spelling has a lower-case `c` that `initContainerStatuses` does not
/// contain at that position.
///
/// Both halves run over CODE only. The presence half asserted
/// `body.contains("initContainerStatuses")` against the raw file, which this
/// paragraph, the assertion's own line and the failure message below each
/// satisfied on their own — the gate could not fail, so "or this gate guards
/// nothing" was exactly what it did.
#[test]
fn no_helper_in_this_suite_reads_the_regular_container_status_list() {
    let body = std::fs::read_to_string(this_file()).expect("this test file is readable");
    assert!(
        reads_the_init_container_status_list(&body),
        "the suite must actually READ the ONLY confirmation site, in an indexing expression and \
         not merely in prose, or this gate guards nothing",
    );
    assert_eq!(
        regular_status_list_token_in(&body),
        None,
        "the forbidden regular-container-status spelling appears in this suite's CODE. \
         Confirmation for the native sidecar comes ONLY from the init-container status list at \
         name={LAUNCHER_CONTAINER_NAME}; the regular-container list can carry a matching, stale or \
         misleading entry and reading it is the defect this gate exists to prevent",
    );
}

/// The body of the first `fn <name>` in this file, up to its closing brace at
/// column zero. Enough to make a source gate about ONE helper, without the
/// brittleness of matching a whole formatted call expression.
fn function_body(source: &str, name: &str) -> String {
    let signature = format!("\nfn {name}(");
    let start = source
        .find(&signature)
        .unwrap_or_else(|| panic!("this suite must define `fn {name}`"))
        + 1;
    let rest = &source[start..];
    let end = rest
        .find("\n}\n")
        .unwrap_or_else(|| panic!("`fn {name}` must be closed at column zero"));
    rest[..end].to_owned()
}

/// **AC4 source gate: the `managedFields` read is paired with
/// `--show-managed-fields`.**
///
/// `kubectl` STRIPS `metadata.managedFields` from `-o json` unless the flag is
/// passed. Without it the whole block reads `null`, and
/// [`resize_patches_recorded`] returns `0` — so "zero `pods/resize` PATCHes
/// landed" passed for a Pod that had just been resized. That is a live-run
/// defect `omp4` measured and fixed, and it is invisible to every hermetic
/// assertion about the Pod: the only thing that can protect it in the ordinary
/// PR lane is a gate over this file's own source.
///
/// Both halves are asserted, so the pairing is real rather than incidental: the
/// reader must still read `managedFields`, and the ONLY helper that fetches a
/// Pod for it must still pass the flag.
#[test]
fn the_managed_fields_read_is_paired_with_show_managed_fields() {
    let source = std::fs::read_to_string(this_file()).expect("this test file is readable");
    let flag = format!("--show-{}", "managed-fields");

    let reader = function_body(&source, "resize_patches_recorded");
    assert!(
        reader.contains("managedFields"),
        "`resize_patches_recorded` must read metadata.managedFields — the apiserver's OWN record \
         of the PATCH — or the zero-PATCH assertion is reading something this test wrote",
    );

    let fetcher = function_body(&source, "pod_json");
    assert!(
        fetcher.contains(&flag),
        "`pod_json` must pass `{flag}`. kubectl strips metadata.managedFields from `-o json` \
         without it, the block reads null, `resize_patches_recorded` returns 0, and \
         `the_live_matrix_has_exactly_one_quota_authority_per_admitted_pod` passes for a Pod that \
         WAS resized. Measured on omp4's branch before the flag was added.",
    );
    assert!(
        fetcher.contains("get") && fetcher.contains("pod"),
        "`pod_json` must be the helper that fetches the Pod, or this gate guards the wrong body",
    );
}

/// **AC10, hermetic half.** The harness script exists, is disposable, refuses
/// every sibling's target, pins its context, and cannot have its floor walked
/// back to the measured-false 1.29.
///
/// Every needle below that names shell CODE is matched against
/// [`script_code`], never the raw file. That script is heavily commented — its
/// header explains the 1.29 correction, its usage block lists the default
/// cluster and registry names — so the raw file satisfied most of these
/// assertions on prose alone. The teardown trap is the sharp case: delete the
/// real `trap` line, leave `# trap cleanup_on_failure EXIT` behind, and a raw
/// `contains` reports an armed teardown that does not exist.
#[test]
fn the_kind_harness_is_disposable_and_isolated() {
    let script = repo_root().join("scripts/kind/setup-resize-matrix-cluster.sh");
    let raw = std::fs::read_to_string(&script)
        .unwrap_or_else(|error| panic!("{} must exist: {error}", script.display()));
    let body = script_code(&raw);

    assert!(
        body.contains(HARNESS_CLUSTER) && body.contains(HARNESS_REGISTRY),
        "the script must create THIS harness's cluster and registry",
    );
    assert_eq!(
        HARNESS_CONTEXT,
        format!("kind-{HARNESS_CLUSTER}"),
        "the context must be DERIVED from the cluster name, never discovered from the kubeconfig",
    );

    // Disjoint from every sibling harness, in both directions: the script must
    // reserve their names, and must not have adopted one of them as its own.
    for sibling in SIBLING_CLUSTERS {
        assert!(
            body.contains(sibling),
            "the script no longer reserves sibling cluster `{sibling}`; a `down` aimed at it \
             would destroy a concurrent run",
        );
        assert_ne!(
            *sibling, HARNESS_CLUSTER,
            "this harness has adopted a sibling's cluster name",
        );
    }
    for port in SIBLING_REG_PORTS {
        assert!(
            body.contains(&port.to_string()),
            "the script no longer reserves registry port {port}",
        );
        assert_ne!(
            *port, HARNESS_REG_PORT,
            "this harness has adopted a sibling's registry port",
        );
    }

    // The teardown and its proof.
    assert!(
        arms_the_failure_teardown(&raw),
        "the failure path must ARM the teardown trap in code; without the trap a failed `up` \
         leaves a cluster behind and the host fills up",
    );
    assert!(
        body.contains("survived teardown"),
        "teardown must PROVE the cluster and registry are gone rather than assume it",
    );
    assert!(
        body.contains("df -Pk /"),
        "the script must check free disk before pulling a node image and three image classes",
    );

    // The floor. 1.29 was measured false and corrected in #2818.
    assert!(
        body.contains("EPIC_MIN_K8S_MINOR=30"),
        "the epic's Kubernetes floor is 1.30",
    );
    assert!(
        !restores_the_measured_false_floor(&raw),
        "the 1.29 floor was measured false and corrected in #2818; it must not come back",
    );
    // The one assertion here that is deliberately about PROSE. "Say out loud" is
    // satisfied by a comment: the point is that a reader of the script learns why
    // a non-kind API server is refused, and the header comment is a legitimate
    // place to say it. Matched against the raw file on purpose.
    assert!(
        raw.contains("eks") || raw.contains("EKS"),
        "the script must say out loud why a non-kind server is refused",
    );
}

// ═══ the gates' own self-tests ═══════════════════════════════════════════════
//
// A source gate that ignores comments and a source gate that ignores everything
// look identical from a green run, so each predicate above is driven against
// synthetic input in BOTH directions.
//
// Two mutations, not one. A PRESENCE gate is mutated by REMOVING the required
// call; swapping it for a different valid token reds the presence arm and proves
// nothing about a ban. A BAN is mutated by ADDING the banned token while LEAVING
// the required one in place.

#[test]
fn a_whole_line_comment_is_not_code_but_a_trailing_one_does_not_launder_the_line() {
    assert_eq!(
        script_code("set -e\n# note\n  # indented\ntrap x EXIT"),
        "set -e\ntrap x EXIT"
    );
    assert_eq!(rust_code("a\n// b\n  /// c\n//! d\ne"), "a\ne");
    assert_eq!(
        script_code("trap cleanup_on_failure EXIT  # armed here"),
        "trap cleanup_on_failure EXIT  # armed here",
        "a trailing comment must NOT drop the code in front of it",
    );
}

#[test]
fn the_teardown_trap_gate_reads_the_trap_and_not_the_paragraph_about_it() {
    let armed = "cleanup_on_failure() { :; }\ntrap cleanup_on_failure EXIT\n";
    assert!(arms_the_failure_teardown(armed));
    assert!(
        !arms_the_failure_teardown("cleanup_on_failure() { :; }\n# trap cleanup_on_failure EXIT\n"),
        "a commented-out trap arms nothing; this is the 1j64 defect verbatim",
    );
    assert!(
        !arms_the_failure_teardown(
            "# the failure path runs `trap cleanup_on_failure EXIT` before it creates anything\n"
        ),
        "prose describing the trap must not stand in for the trap",
    );
    assert!(
        arms_the_failure_teardown("trap cleanup_on_failure EXIT  # before anything is created\n"),
        "a trailing comment must not hide the real trap in front of it",
    );
}

#[test]
fn the_measured_false_floor_ban_fires_on_an_assignment_and_not_on_the_correction_note() {
    // The BAN mutation ADDS the 1.29 assignment and LEAVES the 1.30 epic floor
    // in place, so the only thing that can go red is the ban.
    let corrected = "# the \"1.29\" older docs carried was measured false (#2818); never restore\n\
                     # MIN_K8S_MINOR=29 here or anywhere\nEPIC_MIN_K8S_MINOR=30\nMIN_K8S_MINOR=33\n";
    assert!(
        !restores_the_measured_false_floor(corrected),
        "the paragraph recording the correction must not be punished as the defect it warns about",
    );
    assert!(
        restores_the_measured_false_floor(&format!("{corrected}MIN_K8S_MINOR=29\n")),
        "a real assignment must red the ban even with the epic floor still present",
    );
    assert!(
        restores_the_measured_false_floor("MIN_K8S_MINOR=29  # restored, wrongly\n"),
        "a trailing comment must not launder a real assignment",
    );
    assert!(
        shell_code_contains(corrected, "EPIC_MIN_K8S_MINOR=30"),
        "the epic floor is set in code and must still satisfy its own presence gate",
    );
}

#[test]
fn the_confirmation_site_gate_reads_an_indexing_expression_and_not_prose() {
    let real = concat!(
        "let raw = pod_json[\"status\"][\"initContainer",
        "Statuses\"]\n"
    );
    assert!(reads_the_init_container_status_list(real));
    assert!(
        !reads_the_init_container_status_list(&format!("    // {real}")),
        "a commented-out read must not satisfy the gate",
    );
    assert!(
        !reads_the_init_container_status_list(concat!(
            "//! confirmation comes from status.initContainer",
            "Statuses[name=cgroup-launcher]\n"
        )),
        "prose naming the confirmation site must not stand in for reading it",
    );
    assert!(
        reads_the_init_container_status_list(&format!("{} // AC7", real.trim_end())),
        "a trailing comment must not hide the real read",
    );
}

#[test]
fn the_regular_status_list_ban_fires_on_code_and_not_on_a_comment() {
    // ADD the forbidden read; the permitted init-container read stays put.
    let permitted = concat!(
        "let raw = pod_json[\"status\"][\"initContainer",
        "Statuses\"];\n"
    );
    let offending = concat!("let s = pod_json[\"status\"][\"container", "Statuses\"];\n");
    assert_eq!(regular_status_list_token_in(permitted), None);
    assert!(
        regular_status_list_token_in(&format!("{permitted}{offending}")).is_some(),
        "a real read of the regular list must red the ban with the permitted read still present",
    );
    assert_eq!(
        regular_status_list_token_in(&format!("{permitted}// {offending}")),
        None,
        "a comment explaining why the regular list is forbidden must not red the ban",
    );
    assert!(
        regular_status_list_token_in(&format!("{} // stale\n", offending.trim_end())).is_some(),
        "a trailing comment must not launder a real read",
    );
    assert!(
        reads_the_init_container_status_list(&format!("{permitted}{offending}")),
        "the presence arm must still hold under the ban's mutation, or the two arms are confounded",
    );
}

/// **AC10.** `check` runs every guard and creates nothing, and a context this
/// harness does not own is refused with a non-zero exit.
#[test]
fn the_kind_harness_refuses_a_target_it_does_not_own() {
    let script = repo_root().join("scripts/kind/setup-resize-matrix-cluster.sh");
    if !script.exists() {
        panic!("{} must exist", script.display());
    }

    let run = |args: &[&str]| -> Output {
        Command::new("bash")
            .arg(&script)
            .args(args)
            .current_dir(repo_root())
            .output()
            .expect("the harness script is runnable")
    };

    let accepted = run(&["check"]);
    assert!(
        accepted.status.success(),
        "`check` must pass on the harness's own names: {}",
        String::from_utf8_lossy(&accepted.stderr),
    );
    let report = String::from_utf8_lossy(&accepted.stdout).into_owned();
    assert!(
        report.contains(&format!("cluster={HARNESS_CLUSTER}"))
            && report.contains(&format!("context={HARNESS_CONTEXT}")),
        "`check` reports the target it would create; got {report:?}",
    );

    // A sibling's cluster: refused, exit 3.
    let refused = run(&["check", "--cluster-name", SIBLING_CLUSTERS[0]]);
    assert_eq!(
        refused.status.code(),
        Some(3),
        "a reserved cluster name must be refused with EXIT_REFUSED_TARGET; stderr: {}",
        String::from_utf8_lossy(&refused.stderr),
    );

    // A context that is not the derived one: refused, exit 3.
    let hijacked = run(&["check", "--context", "arn-aws-eks-eu-west-3"]);
    assert_eq!(
        hijacked.status.code(),
        Some(3),
        "a context this harness does not own must be refused",
    );

    // Below the floor: refused, exit 7.
    let ancient = run(&["check", "--k8s-version", "1.29.0"]);
    assert_eq!(
        ancient.status.code(),
        Some(7),
        "1.29 is below the floor and must be refused with EXIT_VERSION_FLOOR",
    );
}

/// **AC2, hermetic half.** The pins are immutable content addresses, never tags.
#[test]
fn the_image_pins_are_immutable() {
    let pins = repo_root().join("tests/fixtures/resize-matrix/pins.env");
    let body =
        std::fs::read_to_string(&pins).unwrap_or_else(|error| panic!("read pins.env: {error}"));

    let value = |key: &str| -> String {
        body.lines()
            .find_map(|line| line.strip_prefix(&format!("{key}=")))
            .unwrap_or_else(|| panic!("pins.env must set {key}"))
            .trim()
            .to_owned()
    };

    let base = value("DJINN_RESIZE_MATRIX_BASE_DIGEST");
    PreProtocolDigest::parse(&base).unwrap_or_else(|error| {
        panic!("the base pin must be an immutable sha256: digest, got {error}")
    });

    let commit = value("DJINN_RESIZE_MATRIX_PREPROTOCOL_COMMIT");
    assert_eq!(
        commit.len(),
        40,
        "the pre-protocol pin must be a full commit sha, not a branch or tag: {commit:?}",
    );
    assert!(
        commit
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "the pre-protocol pin must be lower-case hex: {commit:?}",
    );

    // Three Dockerfiles, three classes, and none of them may pin a tag.
    for class in ImageClass::ALL {
        let dockerfile = repo_root().join(format!(
            "tests/fixtures/resize-matrix/Dockerfile.{}",
            class.as_str()
        ));
        let text = std::fs::read_to_string(&dockerfile)
            .unwrap_or_else(|error| panic!("read {}: {error}", dockerfile.display()));
        assert!(
            text.contains("@${DJINN_RESIZE_MATRIX_BASE_DIGEST}"),
            "{} must pin its base by digest, never by tag",
            dockerfile.display(),
        );
    }
}

// ===========================================================================
// The live half.
// ===========================================================================

const HARNESS_CLUSTER: &str = "djinn-resize-omp4";
const HARNESS_CONTEXT: &str = "kind-djinn-resize-omp4";
const HARNESS_REGISTRY: &str = "djinn-resize-omp4-registry";
const HARNESS_REG_PORT: u16 = 5079;
const NAMESPACE: &str = "djinn";
const SENTINEL_DIR: &str = "/var/tmp/djinn-resize-matrix-sentinels";
const SENTINEL_MOUNT: &str = "/sentinel";
const WORKER_CONTAINER_NAME: &str = "worker";
const PROBE_BIN: &str = "/opt/djinn/bin/djinn-governor-probe";
const PROBE_WORKLOAD: &str = "/opt/djinn/workload.bin";
const PROBE_DECISION_PATH: &str = "/var/tmp/djinn-probe/decision";
const LAUNCHER_BIN_IN_IMAGE: &str = "/opt/djinn/bin/djinn-cgroup-launcher";
const IMAGE_MANIFEST: &str = "server/target/resize-matrix/images.json";
const BUILD_SCRIPT: &str = "tests/fixtures/resize-matrix/build.sh";

/// Every other disposable harness in the repository. Named so the disjointness
/// guard is a statement about them and not only about this script's defaults.
const SIBLING_CLUSTERS: &[&str] = &[
    "djinn-kueue-harness",
    "djinn-kueue-b2b",
    "djinn-kueue-c1",
    "djinn-resize-harness",
    "djinn-resize-pcod",
    "djinn-resize-1j64",
];
const SIBLING_REG_PORTS: &[u16] = &[5051, 5052, 5055, 5061, 5067, 5071];

const TICK: Duration = Duration::from_millis(500);
const AWAIT_TICKS: usize = 360;

fn live_tests_enabled() -> bool {
    if std::env::var("DJINN_TEST_RESIZE_MATRIX").as_deref() != Ok("1") {
        eprintln!(
            "SKIP: set DJINN_TEST_RESIZE_MATRIX=1 (and stand the harness up) to run the live matrix"
        );
        return false;
    }
    for tool in ["kubectl", "kind", "docker"] {
        if Command::new(tool)
            .arg("--help")
            .output()
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        {
            panic!("DJINN_TEST_RESIZE_MATRIX=1 but {tool} is not on PATH");
        }
    }
    true
}

/// Refuse anything that is not this harness's local kind cluster.
///
/// Guard 1 is the NAME, checked by construction: the constant is the only
/// context this file ever passes to `kubectl`. Guard 2 is where that name
/// resolves to, which is what catches a kubeconfig entry called
/// `kind-djinn-resize-omp4` aimed at EKS.
fn harness_context() -> &'static str {
    let output = Command::new("kubectl")
        .args([
            "--context",
            HARNESS_CONTEXT,
            "config",
            "view",
            "--minify",
            "-o",
            "jsonpath={.clusters[0].cluster.server}",
        ])
        .output()
        .expect("kubectl is on PATH");
    let server = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        is_local_apiserver(server.trim()),
        "refusing to run against {HARNESS_CONTEXT}: its API server is {server:?}, not a local \
         kind cluster. Every context in a Djinn developer's kubeconfig is a live EKS cluster.",
    );
    HARNESS_CONTEXT
}

/// Host-anchored: `https://127.0.0.1.evil.example` starts with the loopback
/// address and is a remote host.
fn is_local_apiserver(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split('/').next().unwrap_or_default();
    let host = host.rsplit_once(':').map_or(host, |(head, _)| head);
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

/// **AC10, hermetic.** The refusal predicate, exercised without a cluster so it
/// cannot rot unnoticed.
#[test]
fn a_non_kind_apiserver_is_refused() {
    for hostile in [
        "https://ABCDEF.gr7.eu-west-3.eks.amazonaws.com",
        "https://kubernetes.default.svc",
        "https://10.0.0.1:6443",
        "https://127.0.0.1.evil.example:6443",
        "http://127.0.0.1:6443",
    ] {
        assert!(
            !is_local_apiserver(hostile),
            "{hostile} must be refused: it is not a local kind API server",
        );
    }
    for benign in [
        "https://127.0.0.1:6443",
        "https://localhost:41234",
        "https://[::1]:6443",
    ] {
        assert!(
            is_local_apiserver(benign),
            "{benign} is a local kind server"
        );
    }
}

fn kubectl(args: &[&str]) -> Output {
    Command::new("kubectl")
        .arg("--context")
        .arg(HARNESS_CONTEXT)
        .args(args)
        .output()
        .expect("kubectl is on PATH")
}

fn kubectl_ok(args: &[&str]) -> String {
    let output = kubectl(args);
    assert!(
        output.status.success(),
        "kubectl {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn kubectl_json(args: &[&str]) -> Value {
    let mut full = args.to_vec();
    full.extend(["-o", "json"]);
    serde_json::from_str(&kubectl_ok(&full)).expect("kubectl -o json emits JSON")
}

/// The three built images and their immutable digests.
struct BuiltImages {
    manifest: Value,
}

impl BuiltImages {
    /// Build (or reuse) the three classes and load them onto the harness node.
    fn ensure() -> Self {
        let built = Command::new("bash")
            .arg(repo_root().join(BUILD_SCRIPT))
            .arg(HARNESS_CLUSTER)
            .current_dir(repo_root())
            .output()
            .expect("the image build script is runnable");
        assert!(
            built.status.success(),
            "building the three image classes failed:\n{}\n{}",
            String::from_utf8_lossy(&built.stdout),
            String::from_utf8_lossy(&built.stderr),
        );
        let text = std::fs::read_to_string(repo_root().join(IMAGE_MANIFEST))
            .expect("the build script writes the image manifest");
        Self {
            manifest: serde_json::from_str(&text).expect("the image manifest is JSON"),
        }
    }

    fn tag(&self, class: ImageClass) -> String {
        self.manifest["classes"][class.as_str()]["tag"]
            .as_str()
            .unwrap_or_else(|| panic!("the manifest names a tag for {}", class.as_str()))
            .to_owned()
    }

    fn digest(&self, class: ImageClass) -> String {
        self.manifest["classes"][class.as_str()]["digest"]
            .as_str()
            .unwrap_or_else(|| panic!("the manifest names a digest for {}", class.as_str()))
            .to_owned()
    }
}

/// How many times `needle` occurs in the launcher binary INSIDE `image`.
fn wire_string_occurrences(image: &str, needle: &str) -> usize {
    let output = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--entrypoint",
            "/bin/sh",
            image,
            "-c",
            &format!("grep -ac -- '{needle}' {LAUNCHER_BIN_IN_IMAGE} || true"),
        ])
        .output()
        .expect("docker is on PATH");
    assert!(
        output.status.success(),
        "scanning {image} for {needle} failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

/// **AC2.** The legacy image's launcher binary genuinely predates the protocol,
/// proven by scanning the ARTIFACT rather than by reading its configuration.
///
/// The two declaring classes are the counter-proof and are asserted in the same
/// test on purpose: without them, "the legacy binary contains neither string"
/// would also pass against an empty file, a stripped binary or a scan that never
/// matched anything.
#[test]
#[ignore = "requires docker and tests/fixtures/resize-matrix/build.sh"]
fn legacy_image_binary_predates_the_protocol() {
    if !live_tests_enabled() {
        return;
    }
    let images = BuiltImages::ensure();

    let legacy = images.tag(ImageClass::LegacyNoHandshake);
    assert_eq!(
        wire_string_occurrences(&legacy, LEAF_V1_WIRE),
        0,
        "the legacy launcher binary carries `{LEAF_V1_WIRE}`. It was built from a modern \
         agent-runtime with the environment withheld, which is a launcher that COULD handshake \
         and chose not to — not a launcher that predates the handshake.",
    );
    assert_eq!(
        wire_string_occurrences(&legacy, RESIZE_V2_WIRE),
        0,
        "the legacy launcher binary carries `{RESIZE_V2_WIRE}`; see above",
    );

    for declaring in [ImageClass::LeafV1, ImageClass::ResizeV2] {
        let tag = images.tag(declaring);
        assert!(
            wire_string_occurrences(&tag, LEAF_V1_WIRE) > 0
                && wire_string_occurrences(&tag, RESIZE_V2_WIRE) > 0,
            "the {} image's launcher must carry BOTH wire strings, or the legacy assertion above \
             is not a discriminator",
            declaring.as_str(),
        );
    }

    // Three classes, three artifacts.
    let digests: BTreeSet<String> = ImageClass::ALL
        .iter()
        .map(|class| images.digest(*class))
        .collect();
    assert_eq!(
        digests.len(),
        ImageClass::ALL.len(),
        "two image classes share a digest; they are config variants of one image, not classes",
    );
    for class in ImageClass::ALL {
        PreProtocolDigest::parse(&images.digest(class))
            .expect("every built class has an immutable sha256 content address");
    }

    // The artifact's own claim must agree with the class it is dispatched as.
    for class in [ImageClass::LeafV1, ImageClass::ResizeV2] {
        let baked = docker_read_file(&images.tag(class), "/opt/djinn/authority");
        assert_eq!(
            baked.trim(),
            class.declared_wire().expect("a declaring class declares"),
            "the {} artifact's baked declaration disagrees with the class it is registered as",
            class.as_str(),
        );
    }
    assert!(
        docker_file_absent(
            &images.tag(ImageClass::LegacyNoHandshake),
            "/opt/djinn/authority"
        ),
        "the legacy artifact must make NO declaration",
    );
}

fn docker_read_file(image: &str, path: &str) -> String {
    let output = Command::new("docker")
        .args(["run", "--rm", "--entrypoint", "/bin/cat", image, path])
        .output()
        .expect("docker is on PATH");
    assert!(
        output.status.success(),
        "reading {path} from {image} failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn docker_file_absent(image: &str, path: &str) -> bool {
    let output = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--entrypoint",
            "/bin/sh",
            image,
            "-c",
            &format!("test -e {path}"),
        ])
        .output()
        .expect("docker is on PATH");
    !output.status.success()
}

// ---------------------------------------------------------------------------
// Live cells.
// ---------------------------------------------------------------------------

/// One dispatched cell on the live cluster.
struct LiveCell {
    cell: Cell,
    task_run_id: String,
    job_name: String,
    invocation: String,
    pod: String,
    pod_uid: String,
}

fn sentinel_name(cell: Cell, task_run_id: &str) -> String {
    format!("{}-{}", cell.name(), task_run_id)
}

/// Attach the AC4 sentinel and the probe to a rendered manifest.
///
/// Exactly two mutations, both declared: the worker's `command` becomes a shell
/// that creates the sentinel and then execs the probe, and a hostPath volume is
/// added for the sentinel to land in. Everything that decides the behaviour
/// under test — `runtimeClassName`, `shareProcessNamespace`, both security
/// contexts, the sidecar's command, env, capabilities, resources and the
/// presence or absence of its CPU limit — stays as the production renderer
/// emitted it.
fn attach_probe_and_sentinel(manifest: &mut Value, cell: Cell, task_run_id: &str) -> String {
    let invocation = format!("matrix-{task_run_id}");
    let sentinel = sentinel_name(cell, task_run_id);

    let volumes = manifest
        .pointer_mut("/spec/template/spec/volumes")
        .and_then(Value::as_array_mut)
        .expect("the renderer emits volumes");
    volumes.push(json!({
        "name": "matrix-sentinel",
        "hostPath": { "path": SENTINEL_DIR, "type": "Directory" },
    }));

    let containers = manifest
        .pointer_mut("/spec/template/spec/containers")
        .and_then(Value::as_array_mut)
        .expect("the rendered Job has containers");
    let worker = containers
        .iter_mut()
        .find(|container| container["name"] == WORKER_CONTAINER_NAME)
        .expect("the rendered Job has a worker container");

    // The sentinel is created by a REAL shell in the Pod, and only then is the
    // probe exec'd. A cell that is refused before dispatch never reaches this
    // command, and the sentinel's absence is the observable proof of that.
    worker["command"] = json!([
        "/bin/sh",
        "-c",
        format!("touch {SENTINEL_MOUNT}/{sentinel} && exec {PROBE_BIN}"),
    ]);
    worker["args"] = Value::Null;
    worker["volumeMounts"]
        .as_array_mut()
        .expect("the renderer emits worker volume mounts")
        .push(json!({ "name": "matrix-sentinel", "mountPath": SENTINEL_MOUNT }));

    let env = worker["env"]
        .as_array_mut()
        .expect("the renderer sets worker env");
    for (name, value) in [
        ("DJINN_PROBE_INVOCATION", invocation.clone()),
        ("DJINN_PROBE_FENCE", "1".to_owned()),
        ("DJINN_PROBE_AUTHORITY", "armed".to_owned()),
        ("DJINN_PROBE_DECISION_PATH", PROBE_DECISION_PATH.to_owned()),
        ("DJINN_PROBE_WORKLOAD", PROBE_WORKLOAD.to_owned()),
        ("DJINN_PROBE_CLAMP_SECONDS", "2".to_owned()),
        ("DJINN_PROBE_LIFTED_SECONDS", "2".to_owned()),
    ] {
        env.push(json!({ "name": name, "value": value }));
    }
    invocation
}

fn apply_manifest(manifest: &Value) {
    use std::io::Write as _;
    let text = serde_json::to_string(manifest).expect("the manifest serializes");
    let applied = Command::new("kubectl")
        .args([
            "--context",
            HARNESS_CONTEXT,
            "-n",
            NAMESPACE,
            "apply",
            "-f",
            "-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child
                .stdin
                .as_mut()
                .expect("stdin is piped")
                .write_all(text.as_bytes())?;
            child.wait_with_output()
        })
        .expect("kubectl apply runs");
    assert!(
        applied.status.success(),
        "applying the rendered Job failed: {}",
        String::from_utf8_lossy(&applied.stderr),
    );
}

fn pods_of(task_run_id: &str) -> Vec<(String, String, String)> {
    kubectl_json(&[
        "-n",
        NAMESPACE,
        "get",
        "pods",
        "-l",
        &format!("{LABEL_TASK_RUN_ID}={task_run_id}"),
    ])["items"]
        .as_array()
        .expect("a List has items")
        .iter()
        .map(|item| {
            (
                item["metadata"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                item["metadata"]["uid"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                item["status"]["phase"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            )
        })
        .collect()
}

fn await_running_pod(task_run_id: &str) -> (String, String) {
    for _ in 0..AWAIT_TICKS {
        if let Some((name, uid, _)) = pods_of(task_run_id)
            .into_iter()
            .find(|(_, _, phase)| phase == "Running")
        {
            return (name, uid);
        }
        std::thread::sleep(TICK);
    }
    panic!(
        "no Pod of task-run {task_run_id} reached Running. Pods: {:?}",
        pods_of(task_run_id),
    );
}

fn probe_log(pod: &str) -> String {
    String::from_utf8_lossy(
        &kubectl(&["-n", NAMESPACE, "logs", pod, "-c", WORKER_CONTAINER_NAME]).stdout,
    )
    .into_owned()
}

fn await_probe_record(pod: &str, record: &str) -> String {
    for _ in 0..AWAIT_TICKS {
        let log = probe_log(pod);
        if log.contains(&format!("probe.{record}")) {
            return log;
        }
        if let Some(line) = log.lines().find(|line| line.starts_with("probe.fatal")) {
            panic!("the probe in {pod} failed: {line}");
        }
        std::thread::sleep(TICK);
    }
    panic!(
        "the probe in {pod} never emitted probe.{record}. Log:\n{}",
        probe_log(pod),
    );
}

/// The invocation leaf's `cpu.max`, read from INSIDE the Pod out of the
/// launcher's own delegated cgroup root.
///
/// The kernel's value, read through the same `/sys/fs/cgroup` the launcher
/// writes — never a value this test wrote and never one the probe reported. The
/// leaf directory is mode 0700 and root-owned, hence the launcher container
/// (uid 0) rather than the worker (uid 1000).
fn leaf_cpu_max(pod: &str, invocation: &str) -> String {
    let path = format!("/sys/fs/cgroup/{invocation}/cpu.max");
    let output = kubectl(&[
        "-n",
        NAMESPACE,
        "exec",
        pod,
        "-c",
        LAUNCHER_CONTAINER_NAME,
        "--",
        "cat",
        &path,
    ]);
    assert!(
        output.status.success(),
        "reading {path} in {pod} failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// The quota half of a cgroup v2 `cpu.max` line, in millicores, or `None` when
/// the leaf carries no quota at all.
///
/// `cpu.max` is `"<quota|max> <period>"` — TWO fields. Comparing the whole line
/// against `"max"` never matches, and comparing it against `"max 100000"` would
/// bind this test to the kernel's default period. Only the first field is a
/// statement about who wrote quota.
fn leaf_quota_millicores(raw: &str) -> Option<u64> {
    let mut fields = raw.split_whitespace();
    let quota = fields.next()?;
    let period: u64 = fields.next()?.parse().ok()?;
    if quota == "max" {
        return None;
    }
    let quota: u64 = quota.parse().ok()?;
    if period == 0 {
        return None;
    }
    Some(quota.saturating_mul(1_000) / period)
}

/// The launcher's CPU limit as the kubelet has ACTUATED it, from the ONLY
/// confirmation site: `status.initContainerStatuses[name=cgroup-launcher]`.
fn actuated_launcher_cpu(pod_json: &Value) -> Option<CpuLimit> {
    let raw = pod_json["status"]["initContainerStatuses"]
        .as_array()?
        .iter()
        .find(|entry| entry["name"] == LAUNCHER_CONTAINER_NAME)?["resources"]["limits"]["cpu"]
        .as_str()?
        .to_owned();
    CpuLimit::parse(&raw).ok()
}

/// Whether a `pods/resize` PATCH has ever landed on this Pod.
///
/// Read out of `metadata.managedFields`, which the apiserver writes itself: an
/// entry whose `subresource` is `resize` exists if and only if somebody patched
/// that subresource. It is the apiserver's record of the PATCH, not this test's.
fn resize_patches_recorded(pod_json: &Value) -> usize {
    pod_json["metadata"]["managedFields"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter(|entry| entry["subresource"] == "resize")
                .count()
        })
        .unwrap_or(0)
}

/// One Pod, WITH its `metadata.managedFields`.
///
/// `--show-managed-fields` is not optional decoration. kubectl strips
/// managedFields from `-o json` by default, and without this flag the whole
/// block reads `null` — which made "zero resize PATCHes landed" pass for a Pod
/// that had been resized. Measured on this branch before the flag was added.
fn pod_json(pod: &str) -> Value {
    kubectl_json(&["-n", NAMESPACE, "get", "pod", pod, "--show-managed-fields"])
}

fn sentinel_exists(name: &str) -> bool {
    let nodes = String::from_utf8_lossy(
        &Command::new("kind")
            .args(["get", "nodes", "--name", HARNESS_CLUSTER])
            .output()
            .expect("kind is on PATH")
            .stdout,
    )
    .into_owned();
    nodes.split_whitespace().any(|node| {
        Command::new("docker")
            .args([
                "exec",
                node,
                "test",
                "-e",
                &format!("{SENTINEL_DIR}/{name}"),
            ])
            .output()
            .expect("docker is on PATH")
            .status
            .success()
    })
}

fn delete_cell(live: &LiveCell) {
    eprintln!("--- tearing down cell {} ---", live.cell.name());
    let _ = kubectl(&[
        "-n",
        NAMESPACE,
        "delete",
        "job",
        &live.job_name,
        "--ignore-not-found",
        "--wait=false",
    ]);
}

/// Dispatch one admitted cell and wait until its invocation leaf exists.
fn dispatch(images: &BuiltImages, cell: Cell) -> LiveCell {
    dispatch_as(images, cell, uuid::Uuid::now_v7().to_string())
}

/// [`harness_config`] with the ServiceAccount and PVC names the CHART actually
/// installed on this cluster.
///
/// Resolved from the live cluster rather than hard-coded: the chart's release
/// name prefixes them (`djinn-djinn-taskrun`, not `djinn-taskrun`), and a
/// hard-coded guess fails as `FailedCreate: error looking up service account`
/// minutes into a cell, which reads exactly like a scheduling problem.
fn live_config(image: &str) -> KubernetesConfig {
    let named = |kind: &str, suffix: &str| -> String {
        kubectl_json(&["-n", NAMESPACE, "get", kind])["items"]
            .as_array()
            .expect("a List has items")
            .iter()
            .filter_map(|item| item["metadata"]["name"].as_str())
            .find(|name| name.ends_with(suffix))
            .unwrap_or_else(|| panic!("the chart installs a {kind} ending in {suffix}"))
            .to_owned()
    };
    KubernetesConfig {
        service_account: named("serviceaccounts", "-taskrun"),
        mirror_pvc: named("persistentvolumeclaims", "-mirrors"),
        cache_pvc: named("persistentvolumeclaims", "-cache"),
        projects_pvc: named("persistentvolumeclaims", "-projects"),
        ..harness_config(image)
    }
}

fn dispatch_as(images: &BuiltImages, cell: Cell, task_run_id: String) -> LiveCell {
    let config = live_config(&images.tag(cell.image));
    let (mut manifest, task_run_id) = render_cell_job_as(&config, cell, task_run_id);
    let invocation = attach_probe_and_sentinel(&mut manifest, cell, &task_run_id);
    let job_name = manifest["metadata"]["name"]
        .as_str()
        .expect("the renderer names the Job")
        .to_owned();

    apply_manifest(&manifest);
    let (pod, pod_uid) = await_running_pod(&task_run_id);
    await_probe_record(&pod, "awaiting_decision");

    LiveCell {
        cell,
        task_run_id,
        job_name,
        invocation,
        pod,
        pod_uid,
    }
}

/// **AC3 + AC4.** The whole matrix, live: exactly one quota authority per
/// admitted Pod, and no shell at all for the refused cells.
///
/// One test rather than twelve so the twelve cells share one cluster and one set
/// of images, and so the refused cells can be asserted against the SAME sentinel
/// directory the admitted ones write into. A refused cell whose sentinel
/// mechanism was silently broken would otherwise pass forever.
#[test]
#[ignore = "requires scripts/kind/setup-resize-matrix-cluster.sh up"]
fn the_live_matrix_has_exactly_one_quota_authority_per_admitted_pod() {
    if !live_tests_enabled() {
        return;
    }
    harness_context();
    let images = BuiltImages::ensure();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a tokio runtime");
    let db = runtime
        .block_on(Database::ephemeral())
        .expect("a real test database");
    let modes = LauncherAuthorityModeRepository::new(db.clone());
    let inventory = signed_inventory(&[&images.digest(ImageClass::LegacyNoHandshake)]);

    let mut leaf_cells = 0_usize;
    let mut resize_cells = 0_usize;
    let mut refused_cells = 0_usize;

    for cell in matrix() {
        eprintln!("=== cell {} ===", cell.name());

        // 1. The admission decision, from the production pair against real
        //    PostgreSQL. `Old` has no seam; see the module docs.
        let decision = match cell.mode.authority() {
            None => Ok(AdmissionDecision::Admitted(
                LauncherAuthorityProtocol::LeafV1,
            )),
            Some(authority) => {
                runtime.block_on(force_mode(&modes, authority));
                runtime.block_on(admit(
                    &modes,
                    cell.image,
                    Some(&images.digest(cell.image)),
                    &inventory,
                ))
            }
        };

        match cell.expected {
            Outcome::RefusedBeforeDispatch => {
                refused_cells += 1;
                let rejection = decision.expect_err(&format!(
                    "cell {} must be refused at admission",
                    cell.name(),
                ));

                // Nothing was dispatched, so nothing may exist. A synthetic
                // task-run id stands in for the one that would have been minted.
                let would_have_been = uuid::Uuid::now_v7().to_string();
                let sentinel = sentinel_name(cell, &would_have_been);
                assert!(
                    !sentinel_exists(&sentinel),
                    "cell {} was refused ({rejection}) yet its sentinel exists: a shell ran \
                     before the refusal took effect",
                    cell.name(),
                );
                assert!(
                    pods_of(&would_have_been).is_empty(),
                    "cell {} was refused yet a Pod exists",
                    cell.name(),
                );
                continue;
            }
            Outcome::Admitted(_) => {}
        }

        let admitted = decision
            .unwrap_or_else(|error| panic!("cell {} must be admitted, got {error}", cell.name()));
        assert!(
            matches!(admitted, AdmissionDecision::Admitted(_)),
            "cell {} must resolve an authority, got {admitted:?}",
            cell.name(),
        );

        // 2. Dispatch it for real.
        let live = dispatch(&images, cell);

        // 3. The sentinel is the CONTROL for every refused cell above: it proves
        //    the mechanism whose absence they rely on actually works.
        assert!(
            sentinel_exists(&sentinel_name(cell, &live.task_run_id)),
            "cell {} was admitted and its shell must have run; the sentinel is missing, so \
             every 'the sentinel is absent' assertion in this run is vacuous",
            cell.name(),
        );

        // 4. Observed effect, never a stored column.
        let leaf = leaf_cpu_max(&live.pod, &live.invocation);
        let observed = pod_json(&live.pod);
        assert_eq!(
            observed["metadata"]["uid"].as_str(),
            Some(live.pod_uid.as_str()),
            "the Pod UID moved under us",
        );
        let patches = resize_patches_recorded(&observed);
        let actuated = actuated_launcher_cpu(&observed);

        match cell.expected {
            Outcome::Admitted(Authority::Leaf) => {
                leaf_cells += 1;
                assert!(
                    leaf_quota_millicores(&leaf).is_some(),
                    "cell {}: LEAF authority means the launcher wrote a numeric quota into the \
                     invocation leaf; cpu.max reads {leaf:?}, so NOBODY wrote one",
                    cell.name(),
                );
                assert_eq!(
                    patches,
                    0,
                    "cell {}: a pods/resize PATCH landed on Pod {} alongside the launcher's leaf \
                     quota. That is TWO authorities on one Pod.",
                    cell.name(),
                    live.pod_uid,
                );
                assert_eq!(
                    actuated,
                    None,
                    "cell {}: the launcher carries an actuated CPU limit under leaf authority; \
                     leaf-v1 renders none on purpose and only a resize introduces one",
                    cell.name(),
                );
            }
            Outcome::Admitted(Authority::Resize) => {
                resize_cells += 1;
                let before = actuated.unwrap_or_else(|| {
                    panic!(
                        "cell {}: resize-v2 must render a ceiling the kubelet actuates",
                        cell.name(),
                    )
                });
                assert_eq!(
                    leaf_quota_millicores(&leaf),
                    None,
                    "cell {}: RESIZE authority means the launcher wrote NOTHING into the leaf, \
                     not even `max` as a rewrite; cpu.max reads {leaf:?}",
                    cell.name(),
                );
                assert_eq!(
                    patches,
                    0,
                    "cell {}: a resize PATCH landed before this test issued one",
                    cell.name(),
                );

                // Move it, through the production body and strategic-merge
                // semantics, and confirm from the init-container status only.
                let target = CpuLimit::from_millis(before.millis() / 2);
                issue_resize(&live.pod, target);
                let after = await_actuated(&live.pod, target);
                assert_eq!(
                    after,
                    target,
                    "cell {}: the limit did not move",
                    cell.name()
                );

                let resized = pod_json(&live.pod);
                assert!(
                    resize_patches_recorded(&resized) >= 1,
                    "cell {}: the apiserver recorded no pods/resize PATCH, so the resize \
                     authority never acted. managedFields: {}",
                    cell.name(),
                    resized["metadata"]["managedFields"],
                );
                assert_eq!(
                    leaf_quota_millicores(&leaf_cpu_max(&live.pod, &live.invocation)),
                    None,
                    "cell {}: the launcher wrote leaf quota AFTER the resize; that is a second \
                     authority on the same Pod",
                    cell.name(),
                );
            }
            Outcome::RefusedBeforeDispatch => unreachable!("handled above"),
        }

        delete_cell(&live);
    }

    assert!(
        leaf_cells > 0 && resize_cells > 0 && refused_cells > 0,
        "the live matrix must have exercised all three outcomes; \
         leaf={leaf_cells} resize={resize_cells} refused={refused_cells}",
    );
}

/// Apply the production resize body with strategic-merge semantics against the
/// `resize` subresource.
fn issue_resize(pod: &str, target: CpuLimit) {
    let body = build_resize_patch(target);
    let text = serde_json::to_string(&body).expect("the resize body serializes");
    let output = kubectl(&[
        "-n",
        NAMESPACE,
        "patch",
        "pod",
        pod,
        "--subresource",
        "resize",
        "--type",
        "strategic",
        "--patch",
        &text,
    ]);
    assert!(
        output.status.success(),
        "the strategic resize PATCH failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
}

fn await_actuated(pod: &str, target: CpuLimit) -> CpuLimit {
    for _ in 0..AWAIT_TICKS {
        let observed = pod_json(pod);
        if let Some(limit) = actuated_launcher_cpu(&observed)
            && limit == target
        {
            return limit;
        }
        std::thread::sleep(TICK);
    }
    panic!(
        "the launcher's limit never reached {target} in status.initContainerStatuses. Pod: {}",
        pod_json(pod)["status"],
    );
}

/// **AC6.** The strategic PATCH changes exactly the launcher's CPU limit; an RFC
/// 7386 merge body against the same subresource is REFUSED by the apiserver.
///
/// Both halves are live. The first is the property; the second is the named
/// mutation, and it is the measured behaviour the epic recorded:
/// `spec.initContainers[0].resources.limits: Forbidden: resource limits cannot
/// be removed`, because a merge patch replaces the whole array.
#[test]
#[ignore = "requires scripts/kind/setup-resize-matrix-cluster.sh up"]
fn a_strategic_resize_leaves_every_other_container_byte_identical() {
    if !live_tests_enabled() {
        return;
    }
    harness_context();
    let images = BuiltImages::ensure();
    let cell = Cell {
        image: ImageClass::ResizeV2,
        mode: ServerMode::Activated,
        expected: expectation(ImageClass::ResizeV2, ServerMode::Activated),
    };
    let live = dispatch(&images, cell);

    let before = pod_json(&live.pod);
    let before_spec = before["spec"].clone();
    let ceiling = actuated_launcher_cpu(&before).expect("resize-v2 renders an actuated ceiling");
    let target = CpuLimit::from_millis(ceiling.millis() / 2);

    // The named mutation, run FIRST so a cluster that accepted it could not be
    // mistaken for one where the strategic path merely happened to work.
    let merged = kubectl(&[
        "-n",
        NAMESPACE,
        "patch",
        "pod",
        &live.pod,
        "--subresource",
        "resize",
        "--type",
        "merge",
        "--patch",
        &serde_json::to_string(&build_resize_patch(target)).expect("body serializes"),
    ]);
    assert!(
        !merged.status.success(),
        "an RFC 7386 merge body against pods/resize must be REFUSED: it replaces the whole \
         initContainers array, whose patchMergeKey is `name`. The apiserver accepted it, which \
         means the other init containers were silently destroyed.",
    );

    issue_resize(&live.pod, target);
    await_actuated(&live.pod, target);
    let after = pod_json(&live.pod);

    // Every other init container and every regular container, byte identical.
    let strip = |spec: &Value, list: &str| -> Vec<Value> {
        spec[list]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .filter(|entry| entry["name"] != LAUNCHER_CONTAINER_NAME)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    };
    for list in ["initContainers", "containers"] {
        assert_eq!(
            strip(&before_spec, list),
            strip(&after["spec"], list),
            "the resize changed {list} other than the launcher entry",
        );
    }

    // And the launcher entry itself changed in exactly one place.
    let launcher_of = |spec: &Value| -> Value {
        spec["initContainers"]
            .as_array()
            .expect("initContainers")
            .iter()
            .find(|entry| entry["name"] == LAUNCHER_CONTAINER_NAME)
            .expect("the launcher survives the resize")
            .clone()
    };
    let mut before_launcher = launcher_of(&before_spec);
    let mut after_launcher = launcher_of(&after["spec"]);
    assert_ne!(
        before_launcher["resources"]["limits"]["cpu"], after_launcher["resources"]["limits"]["cpu"],
        "the resize must have moved the launcher's CPU limit",
    );
    before_launcher["resources"]["limits"]["cpu"] = Value::Null;
    after_launcher["resources"]["limits"]["cpu"] = Value::Null;
    assert_eq!(
        before_launcher, after_launcher,
        "the resize touched a second field on the launcher container",
    );

    delete_cell(&live);
}

/// **AC8, live.** The apiserver canonicalises `4000m` to `4`, and a millicore
/// comparison treats them as equal where a string comparison would not.
#[test]
#[ignore = "requires scripts/kind/setup-resize-matrix-cluster.sh up"]
fn the_apiserver_canonicalises_a_ceiling_and_millicores_still_match() {
    if !live_tests_enabled() {
        return;
    }
    harness_context();
    let images = BuiltImages::ensure();
    let cell = Cell {
        image: ImageClass::ResizeV2,
        mode: ServerMode::Activated,
        expected: expectation(ImageClass::ResizeV2, ServerMode::Activated),
    };
    let live = dispatch(&images, cell);

    // Ask for exactly four cores, spelled in millicores.
    let target = CpuLimit::from_millis(4_000);
    issue_resize(&live.pod, target);
    let confirmed = await_actuated(&live.pod, target);
    assert_eq!(confirmed, target);

    // The stored spelling is the apiserver's, not ours.
    let stored = pod_json(&live.pod)["status"]["initContainerStatuses"]
        .as_array()
        .expect("initContainerStatuses")
        .iter()
        .find(|entry| entry["name"] == LAUNCHER_CONTAINER_NAME)
        .expect("the launcher has a status")["resources"]["limits"]["cpu"]
        .as_str()
        .expect("an actuated limit")
        .to_owned();
    assert_eq!(
        stored, "4",
        "the apiserver is expected to canonicalise 4000m to 4; if this changed, the millicore \
         rule is still right but this cell no longer demonstrates why",
    );
    assert_ne!(
        stored,
        target.as_quantity(),
        "the two spellings must DIFFER as strings or this cell proves nothing",
    );
    assert_eq!(
        CpuLimit::parse(&stored).expect("the stored form parses"),
        target,
        "parsed millicores must compare equal; `Quantity` string equality reports \
         `never reported 4000m; last observed Some(4)`",
    );

    delete_cell(&live);
}

/// **AC7, live.** A Pod carrying a MISLEADING, perfectly matching
/// regular-container status and NO init-container status must be treated as
/// unconfirmed.
///
/// The shape is the pre-sidecar render: `cgroup-launcher` as an ordinary
/// container. Kubernetes forbids the same name in both lists, so this — not a
/// duplicate — is the way a misleading match reaches the apiserver, and it is
/// exactly the shape a rollback to a pre-1.29 render would produce.
#[test]
#[ignore = "requires scripts/kind/setup-resize-matrix-cluster.sh up"]
fn a_misleading_regular_container_status_is_not_confirmation() {
    if !live_tests_enabled() {
        return;
    }
    harness_context();
    let images = BuiltImages::ensure();
    let tag = images.tag(ImageClass::LeafV1);
    let name = format!("matrix-misleading-{}", uuid::Uuid::now_v7());

    let target = CpuLimit::from_millis(500);
    let pod = json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": { "name": name, "namespace": NAMESPACE },
        "spec": {
            "restartPolicy": "Never",
            "containers": [{
                // The MISLEADING entry: right name, right limit, wrong list.
                "name": LAUNCHER_CONTAINER_NAME,
                "image": tag,
                "command": ["/bin/sh", "-c", "sleep 600"],
                "resources": { "limits": { "cpu": target.as_quantity(), "memory": "128Mi" },
                               "requests": { "cpu": "50m", "memory": "64Mi" } },
            }],
        }
    });
    apply_manifest(&pod);

    let mut observed = Value::Null;
    for _ in 0..AWAIT_TICKS {
        observed = pod_json(&name);
        if observed["status"]["phase"] == "Running" {
            break;
        }
        std::thread::sleep(TICK);
    }
    assert_eq!(
        observed["status"]["phase"], "Running",
        "the misleading Pod must actually run, or its status lists are both empty and the \
         assertion below is vacuous",
    );

    // The misleading entry really is there and really does match: without this
    // the test would pass against a Pod that simply had no statuses at all.
    let misleading = observed["status"][format!("{}{}", "container", "Statuses").as_str()]
        .as_array()
        .expect("the regular container has a status")
        .iter()
        .find(|entry| entry["name"] == LAUNCHER_CONTAINER_NAME)
        .expect("the misleading entry exists")["resources"]["limits"]["cpu"]
        .as_str()
        .map(|raw| CpuLimit::parse(raw).expect("the misleading limit parses"));
    assert_eq!(
        misleading,
        Some(target),
        "the misleading entry must MATCH the target; a reader that fell back to it would call \
         this Pod confirmed",
    );

    // The only confirmation site says nothing.
    assert_eq!(
        actuated_launcher_cpu(&observed),
        None,
        "there is no init-container status for {LAUNCHER_CONTAINER_NAME}, so this Pod is \
         UNCONFIRMED. Changing the lookup to the regular-container list turns this green.",
    );

    let _ = kubectl(&[
        "-n",
        NAMESPACE,
        "delete",
        "pod",
        &name,
        "--ignore-not-found",
        "--wait=false",
    ]);
}

// ===========================================================================
// AC9: the flips are driven by `ResizeRollout` and gated by the deploy-time
// cutover preflight, against real PostgreSQL and real cluster state.
// ===========================================================================

// ---------------------------------------------------------------------------
// The driver's three injectable seams.
// ---------------------------------------------------------------------------

/// A Pod plane holding **no** live task-run Pods, and counting what the driver
/// attempted against it.
///
/// This is not a fake repository and cannot become one: [`ResizeRollout::new`]
/// builds `BuildPodPermitRepository`, `LauncherAuthorityModeRepository` and
/// `ImageRepository` from the `Database` it is handed and exposes no seam
/// through which a substitute could be passed. Every durable assertion below
/// therefore runs against real PostgreSQL. What this stands in for is the ONE
/// dimension PostgreSQL cannot answer — "is a Pod still alive" — in the half of
/// the suite that has no API server. `ClusterPodPlane` answers it for real.
///
/// The counters exist so "the cutover created nothing" is an observed effect
/// rather than an intention: `creations` moves the instant a Pod is created,
/// whether or not anybody wrote an assertion about ordering.
#[derive(Default)]
struct DrainedPodPlane {
    creations: AtomicU64,
    resizes: AtomicU64,
}

impl DrainedPodPlane {
    fn creations(&self) -> u64 {
        self.creations.load(Ordering::SeqCst)
    }

    fn resizes(&self) -> u64 {
        self.resizes.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl TaskRunPodPlane for DrainedPodPlane {
    async fn create_task_run_pod(&self, _task_run_id: &str, _image_id: &str) -> Result<(), String> {
        self.creations.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn resize_launcher_cpu(
        &self,
        _task_run_id: &str,
        _millicores: u64,
    ) -> Result<(), String> {
        self.resizes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn live_task_run_pods(&self) -> Result<Vec<LiveTaskRunPod>, String> {
        Ok(Vec::new())
    }
}

/// The LIVE task-run Pod census, read off the harness cluster.
///
/// # Why not `KubernetesTaskRunPodPlane`, the production implementation
///
/// Because building one requires a `KubernetesRuntime`, and every constructor
/// for that resolves its client through `kube::Client::try_default()` — which
/// reads the CURRENT kubeconfig context. Every context in a Djinn developer's
/// kubeconfig is a live EKS cluster, and the whole safety property of this
/// harness is that it never consults the current context: it passes
/// `--context kind-djinn-resize-omp4` explicitly on every call and
/// [`harness_context`] refuses that name unless it resolves to a loopback API
/// server. Composing the production plane here would trade a real production
/// hazard for a naming nicety.
///
/// What is preserved is that the census is a REAL apiserver read: the rows
/// below are Pods the kubelet is running, with the `metadata.uid` the resize
/// fence would be taken against, selected by the label the production renderer
/// actually writes ([`LABEL_COMPONENT`] = [`COMPONENT_TASK_RUN_WORKER`], not
/// `app.kubernetes.io/component=task-run`, which matches nothing).
struct ClusterPodPlane;

#[async_trait]
impl TaskRunPodPlane for ClusterPodPlane {
    /// Refuses, always — as [`djinn_server::task_run_resize_rollout::KubernetesTaskRunPodPlane`]
    /// does. The cutover's only dispatch is the probe `pause_admission` issues
    /// to disbelieve its own pause, and a probe that materialised a task-run Pod
    /// on the cluster would be a worse bug than the one it checks for.
    async fn create_task_run_pod(&self, task_run_id: &str, _image_id: &str) -> Result<(), String> {
        Err(format!(
            "the authority cutover never creates task-run Pods; refusing to materialise one for \
             {task_run_id}"
        ))
    }

    async fn resize_launcher_cpu(&self, task_run_id: &str, _millicores: u64) -> Result<(), String> {
        Err(format!(
            "the authority cutover never issues pods/resize; refusing one for {task_run_id}"
        ))
    }

    async fn live_task_run_pods(&self) -> Result<Vec<LiveTaskRunPod>, String> {
        Ok(live_task_run_pods_on_cluster())
    }
}

/// Every task-run Pod the harness cluster still holds, from the apiserver.
///
/// A `Succeeded`/`Failed` Pod owes the cutover nothing — it has no running
/// launcher and no leaf to write — so the census is the set that is genuinely
/// still in flight. Feeding the SAME list to the driver and to the preflight's
/// observation bundle is deliberate: the two cannot disagree about what is
/// alive.
fn live_task_run_pods_on_cluster() -> Vec<LiveTaskRunPod> {
    kubectl_json(&[
        "-n",
        NAMESPACE,
        "get",
        "pods",
        "-l",
        &format!("{LABEL_COMPONENT}={COMPONENT_TASK_RUN_WORKER}"),
    ])["items"]
        .as_array()
        .expect("a List has items")
        .iter()
        .filter(|item| {
            !matches!(
                item["status"]["phase"].as_str().unwrap_or_default(),
                "Succeeded" | "Failed"
            )
        })
        .map(|item| LiveTaskRunPod {
            pod_name: item["metadata"]["name"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            pod_uid: item["metadata"]["uid"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            task_run_id: item["metadata"]["labels"][LABEL_TASK_RUN_ID]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        })
        .collect()
}

/// The registry seam, wired to refuse.
///
/// Every flip below calls `verify_retention(&[])` — retention over an EMPTY
/// retained set. Retention is `eeky`'s property and is proven there against a
/// real HTTP round trip whose response BODY is hashed; a probe that panics is
/// what stops this file quietly acquiring a second, weaker copy of that proof.
struct UnreachedRegistry;

#[async_trait]
impl RegistryProbe for UnreachedRegistry {
    async fn fetch_manifest(&self, repository: &str, reference: &str) -> Result<Vec<u8>, String> {
        panic!(
            "the flip tests retain nothing, so the registry must never be reached; asked for \
             {repository}@{reference}"
        );
    }
}

// ---------------------------------------------------------------------------
// Driving the production cutover.
// ---------------------------------------------------------------------------

/// A cutover driver bound to `db`, with the production admission control.
///
/// `DurableAdmissionControl` writes the real `dispatch_pauses` state and reads
/// it back through `djinn_coordinator`'s OWN refusal predicate, so the pause the
/// cutover believes in is the pause that refuses task dispatch.
fn cutover_rollout(db: &Database, pods: Arc<dyn TaskRunPodPlane>) -> ResizeRollout {
    ResizeRollout::new(
        db.clone(),
        signed_inventory(&[&stub_digest(ImageClass::LegacyNoHandshake)]),
        Arc::new(DurableAdmissionControl::new(
            db.clone(),
            djinn_core::events::EventBus::noop(),
            "li91-cutover",
        )),
        pods,
        Arc::new(UnreachedRegistry),
    )
}

/// A catalog row the probe dispatch can name, written by the production
/// `ImageRepository`.
async fn seed_catalog_image(db: &Database, class: ImageClass) -> Image {
    let images = ImageRepository::new(db.clone());
    // `images.id` is `varchar(36)`, so the class goes in the NAME. A derived id
    // that overflowed would fail here rather than truncate, but there is no
    // reason to make a fixture live on that edge.
    let id = format!("img-{}", uuid::Uuid::now_v7().simple());
    images
        .create(
            &id,
            &format!("resize-matrix-{}", class.as_str()),
            None,
            "{}",
        )
        .await
        .expect("seed a catalog image");
    images
        .mark_ready(
            &id,
            &format!("reg/resize-matrix-{}:{id}", class.as_str()),
            Some(&stub_digest(class)),
            class.declared(),
        )
        .await
        .expect("mark the seeded image ready");
    images
        .get(&id)
        .await
        .expect("read it back")
        .expect("it exists")
}

/// Walk the driver to the point where the drain proof is the next step, and
/// prove it got there by reading the journal rather than by assuming it.
///
/// `pause_admission` does not trust the row it writes: it dispatches `probe`
/// through the same `attempt_dispatch` a task run takes and requires
/// `RefusedByAdmissionPause`. A pause row with no wired refusal blocks the
/// cutover here rather than producing a paused one.
async fn arm_at_the_flip(rollout: &ResizeRollout, probe: DispatchProbe) {
    rollout
        .verify_retention(&[])
        .await
        .expect("retention over an empty retained set is provable");
    rollout
        .pause_admission("li91 authority cutover", &probe)
        .await
        .expect("the production dispatch-pause predicate must refuse the probe");
    assert_eq!(
        rollout.journal(),
        vec![RolloutStep::RetentionVerified, RolloutStep::AdmissionPaused],
        "the driver must be armed at exactly the drain proof",
    );
}

fn probe_for(image: &Image, task_run_id: &str) -> DispatchProbe {
    DispatchProbe {
        task_run_id: task_run_id.to_owned(),
        image: image.clone(),
    }
}

async fn current_mode(modes: &LauncherAuthorityModeRepository) -> LauncherAuthorityProtocol {
    modes
        .read()
        .await
        .expect("read the mode")
        .expect("the singleton row")
        .mode
}

async fn current_epoch(modes: &LauncherAuthorityModeRepository) -> i64 {
    modes
        .read()
        .await
        .expect("read the mode")
        .expect("the singleton row")
        .epoch
}

/// Acquire a permit and drive it into a NONTERMINAL RESIZE state, using only
/// production repository calls.
///
/// `acquire` alone is not enough: `state = 'acquired'` is not one of migration
/// 164's six nonterminal resize states, so
/// `build_pod_permits_resize_nonterminal_idx` does not select it and the
/// driver's row-naming refusal would never fire. `bind_or_refresh_job_uid` then
/// `capture_resize_identity` reach `birth_confirmed`, which it does select — and
/// which carries the `pod_uid` the refusal names.
///
/// Returns the row (so the caller can release it) and the captured Pod UID.
async fn seed_nonterminal_resize_row(
    permits: &BuildPodPermitRepository,
    task_run_id: &str,
) -> (BuildPodPermitRow, String) {
    let row = match permits.acquire(task_run_id, 4).await {
        AcquireBuildPodPermitResult::Acquired { row, .. } => row,
        other => panic!("the fixture must be able to acquire a permit: {other:?}"),
    };
    permits
        .bind_or_refresh_job_uid(
            task_run_id,
            &row.permit_id,
            row.fencing_token,
            &format!("job-uid-{task_run_id}"),
        )
        .await
        .unwrap_or_else(|error| panic!("binding a Job UID must succeed: {error}"));
    let pod_uid = format!("uid-{task_run_id}");
    let captured = permits
        .capture_resize_identity(
            task_run_id,
            &row.permit_id,
            row.fencing_token,
            &BuildPodResizeIdentity {
                pod_namespace: NAMESPACE.into(),
                pod_name: format!("taskrun-{task_run_id}"),
                pod_uid: pod_uid.clone(),
                launcher_container_name: LAUNCHER_CONTAINER_NAME.into(),
                launcher_container_id: "containerd://li91".into(),
                image_digest: stub_digest(ImageClass::ResizeV2),
                observed_launcher_protocol: RESIZE_V2_WIRE.into(),
                effective_launcher_protocol: RESIZE_V2_WIRE.into(),
                admitted_cpu_millicores: 4_000,
            },
        )
        .await
        .expect("capture the resize identity");
    assert!(
        matches!(captured, CaptureBuildPodResizeIdentityResult::Captured(_)),
        "the fixture must actually capture: {captured:?}",
    );
    (row, pod_uid)
}

/// **AC9, durable half.** Both flips are driven by `ResizeRollout` and both are
/// refused — NAMING the offending row — while a nonterminal resize row exists.
///
/// Real PostgreSQL, real repositories, no `Fake`. The row is created through
/// `acquire` / `bind_or_refresh_job_uid` / `capture_resize_identity` — the
/// production calls — so the row the fence reads is the row dispatch writes.
///
/// # The distinguisher this test exists for
///
/// `LauncherAuthorityModeRepository::set_mode` refuses this same seeded row on
/// its own, with `DrainNotEmpty` and a bare per-dimension COUNT. A test that
/// flipped through `set_mode` would therefore be green whether or not the
/// production rollout driver existed. `ResizeRollout::prove_drained` refuses
/// with `NonterminalResizeRows`, carrying `task_run_id`, `state` and `pod_uid`,
/// and only `BuildPodPermitRepository::list_nonterminal_resize` can produce
/// that. Asserting the named rows is what makes deleting the driver visible.
#[tokio::test]
async fn neither_flip_proceeds_with_a_live_permit_row() {
    let db = Database::ephemeral().await.expect("a real test database");
    let modes = LauncherAuthorityModeRepository::new(db.clone());
    let permits = BuildPodPermitRepository::new(db.clone());

    force_mode(&modes, LauncherAuthorityProtocol::LeafV1).await;
    let drained = modes.drain_census().await.expect("read the drain census");
    assert!(
        drained.is_drained(),
        "the fixture must start drained or the refusals below prove nothing: {drained:?}",
    );
    let probe_image = seed_catalog_image(&db, ImageClass::LeafV1).await;

    for (label, target) in [
        ("activation", LauncherAuthorityProtocol::ResizeV2),
        ("rollback", LauncherAuthorityProtocol::LeafV1),
    ] {
        let before = current_mode(&modes).await;
        assert_ne!(
            before, target,
            "{label}: a same-mode replay would move nothing and prove nothing",
        );

        let task_run_id = seed_task_run(&db).await;
        let (row, pod_uid) = seed_nonterminal_resize_row(&permits, &task_run_id).await;

        let census = modes.drain_census().await.expect("read the drain census");
        assert!(
            !census.is_drained(),
            "{label}: one live permit must make the drain non-empty: {census:?}",
        );
        assert_eq!(
            permits
                .list_nonterminal_resize()
                .await
                .expect("read the nonterminal resize rows")
                .len(),
            1,
            "{label}: the fixture must produce a row the nonterminal partial index actually \
             selects, or the driver's refusal below is about nothing",
        );

        // A new driver per flip: every step may run exactly once, which is the
        // point — a cutover is not idempotent by replay.
        let pods = Arc::new(DrainedPodPlane::default());
        let rollout = cutover_rollout(&db, Arc::clone(&pods) as Arc<dyn TaskRunPodPlane>);
        arm_at_the_flip(&rollout, probe_for(&probe_image, &task_run_id)).await;

        // ── the drain proof refuses, NAMING the row ─────────────────────
        let blocked = rollout
            .prove_drained()
            .await
            .expect_err("a nonterminal resize row must block the drain proof");
        let RolloutBlocked::NonterminalResizeRows(rows) = &blocked else {
            panic!(
                "{label}: the driver must refuse naming the rows, got {blocked:?}. \
                 `AuthorityDrainRefused` here would mean the flip was fenced by `set_mode`'s bare \
                 census, which refuses this row whether or not `ResizeRollout` is wired to \
                 anything — the exact vacuity this test exists to remove."
            );
        };
        assert_eq!(rows.len(), 1, "{label}: one seeded row, one named row");
        assert_eq!(
            rows[0].task_run_id, task_run_id,
            "{label}: the refusal must name the task run an operator has to go and find",
        );
        assert_eq!(
            rows[0].state, "BirthConfirmed",
            "{label}: the refusal must name the durable lifecycle state",
        );
        assert_eq!(
            rows[0].pod_uid.as_deref(),
            Some(pod_uid.as_str()),
            "{label}: the refusal must name the Pod UID the resize fence is taken against",
        );

        // A step whose body failed must not be journalled: reserving on entry
        // would let a refused proof satisfy the prerequisite of the flip.
        assert!(
            !rollout.journal().contains(&RolloutStep::DrainProven),
            "{label}: a refused drain proof must not appear in the journal: {:?}",
            rollout.journal(),
        );
        let epoch = current_epoch(&modes).await;
        assert_eq!(
            rollout.flip_authority_mode(epoch, target).await,
            Err(RolloutBlocked::StepOutOfOrder {
                step: RolloutStep::AuthorityModeFlipped,
                missing: RolloutStep::DrainProven,
            }),
            "{label}: an unproven drain must not be flippable",
        );
        assert_eq!(
            current_mode(&modes).await,
            before,
            "{label}: a refused flip must not have moved the mode anyway — an Err returned AFTER \
             the write is exactly the shape this assertion exists to catch",
        );

        // ── drain, and the SAME driver flips ────────────────────────────
        permits
            .release(&task_run_id, &row.permit_id, row.fencing_token, "matrix")
            .await
            .expect("release the permit");
        assert!(
            permits
                .list_nonterminal_resize()
                .await
                .expect("read the nonterminal resize rows")
                .is_empty(),
            "{label}: releasing the permit must clear the nonterminal row",
        );

        rollout
            .prove_drained()
            .await
            .expect("a drained snapshot must prove");
        let flipped = rollout
            .flip_authority_mode(epoch, target)
            .await
            .unwrap_or_else(|error| panic!("{label}: a proven drain must flip: {error:?}"));
        rollout
            .resume_admission()
            .await
            .expect("admission resumes after a confirmed flip");

        assert_eq!(
            rollout.journal(),
            vec![
                RolloutStep::RetentionVerified,
                RolloutStep::AdmissionPaused,
                RolloutStep::DrainProven,
                RolloutStep::AuthorityModeFlipped,
                RolloutStep::AdmissionResumed,
            ],
            "{label}: the observed step sequence is the ordering proof",
        );
        assert_eq!(
            current_mode(&modes).await,
            target,
            "{label}: PostgreSQL must hold the new mode",
        );
        assert!(
            flipped > epoch,
            "{label}: the flip must advance the epoch, got {flipped} from {epoch}",
        );
        assert_eq!(
            (
                pods.creations(),
                pods.resizes(),
                rollout.dispatches_admitted_while_paused()
            ),
            (0, 0, 0),
            "{label}: the cutover must create no Pod and issue no pods/resize PATCH",
        );
    }
}

// ---------------------------------------------------------------------------
// The deploy-time gate: `deploy/preflight/cutover-preflight.sh`.
// ---------------------------------------------------------------------------

/// The chart values that arm the launcher. Without them there is no sidecar for
/// pod resize to move, and `resize-v2` is refused with `launcher-cpu-ceiling` —
/// which is a correct verdict about a deployment that is not ready, and not the
/// question these tests ask.
const ARMED_CHART_VALUES: &[&str] = &[
    "--set",
    "cgroupLauncher.mode=required",
    "--set",
    "cgroupWritable.taskRuns.enabled=true",
    "--set",
    "cgroupWritable.runtimeClass.enabled=true",
];

/// One verdict from the deploy-time gate.
#[derive(Debug)]
struct PreflightVerdict {
    /// 0 the cutover may proceed, 1 blocked, 2 unevaluable. A missing code
    /// (killed by a signal) is neither and is kept distinct from both.
    status: Option<i32>,
    /// The defect classes it printed, as an exact set.
    classes: BTreeSet<String>,
    output: String,
}

impl PreflightVerdict {
    /// Only exit 0 clears. `2` — unevaluable — is deliberately NOT a pass: a
    /// gate that could not evaluate itself has said nothing about the cutover.
    fn cleared(&self) -> bool {
        self.status == Some(0)
    }
}

/// The prebuilt gate binary.
///
/// Deliberately NOT built here. `deploy/preflight/cutover-preflight.sh` builds
/// it itself when `CUTOVER_PREFLIGHT_BIN` is unset, and a `cargo build` launched
/// from inside a running `cargo test` blocks forever on the build-directory lock
/// the outer cargo still holds. The lane prebuilds it; a missing binary is a
/// loud failure naming the command, never a skip.
fn preflight_binary() -> PathBuf {
    let path = std::env::var("CUTOVER_PREFLIGHT_BIN").map_or_else(
        |_| {
            std::env::var("CARGO_TARGET_DIR")
                .map_or_else(|_| repo_root().join("server/target"), PathBuf::from)
                .join("debug/cutover-preflight")
        },
        PathBuf::from,
    );
    assert!(
        path.is_file(),
        "the cutover preflight binary is missing at {}. Build it BEFORE running this suite — \
         `cd server && cargo build -p djinn-k8s --bin cutover-preflight` — because the gate script \
         would otherwise build it from inside this process and deadlock on cargo's build-directory \
         lock. Override with CUTOVER_PREFLIGHT_BIN.",
        path.display(),
    );
    path
}

/// The URL of the ephemeral test database, so the gate reads the SAME durable
/// fence this test seeded.
///
/// Without `DJINN_DATABASE_URL` the gate reports the drain fence UNOBSERVABLE,
/// which is a defect by design — so a test that forgot to pass it would be
/// asserting a blocked verdict it did not cause.
async fn ephemeral_database_url(db: &Database) -> String {
    let name: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(db.pool())
        .await
        .expect("the ephemeral database answers");
    let base = djinn_db::test_database_base_url();
    let trimmed = base.trim_end_matches('/');
    let (scheme, rest) = trimmed
        .split_once("://")
        .expect("the test database URL carries a scheme");
    let authority = rest.split('/').next().unwrap_or(rest);
    format!("{scheme}://{authority}/{name}")
}

/// Run `deploy/preflight/cutover-preflight.sh` — the deploy-time gate itself,
/// as a subprocess.
///
/// This file contains no copy of any of its six checks. `render` optionally
/// substitutes a mutated render through the same `CUTOVER_PREFLIGHT_RENDER`
/// seam the gate's own contract suite uses.
fn cutover_preflight(
    mode: LauncherAuthorityProtocol,
    database_url: &str,
    observations: &Path,
    render: Option<&Path>,
) -> PreflightVerdict {
    let mut command = Command::new("bash");
    command
        .arg(repo_root().join("deploy/preflight/cutover-preflight.sh"))
        .arg(repo_root().join("deploy/helm/djinn"))
        .args(ARMED_CHART_VALUES)
        .current_dir(repo_root())
        .env("CUTOVER_PREFLIGHT_BIN", preflight_binary())
        .env("DJINN_CUTOVER_AUTHORITY_MODE", mode.as_wire())
        .env("DJINN_DATABASE_URL", database_url)
        .env("DJINN_CUTOVER_OBSERVATIONS", observations);
    if let Some(render) = render {
        command.env("CUTOVER_PREFLIGHT_RENDER", render);
    }
    let output = command
        .output()
        .expect("deploy/preflight/cutover-preflight.sh is runnable");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let classes = text
        .lines()
        .filter_map(|line| line.strip_prefix("cutover-preflight: DEFECT "))
        .filter_map(|rest| rest.split_whitespace().next())
        .map(str::to_owned)
        .collect();
    assert_ne!(
        output.status.code(),
        Some(2),
        "the gate could not evaluate itself, which is neither clear nor blocked:\n{text}",
    );
    PreflightVerdict {
        status: output.status.code(),
        classes,
        output: text,
    }
}

/// The cluster/registry facts a render cannot contain, written where the gate
/// expects them.
///
/// Every field is read off the catalog ROW the production `ImageRepository`
/// wrote — including the declaration, which comes from
/// `images.launcher_authority_protocol` rather than from the test's idea of
/// what the class declares. A bundle that restated the declaration would be
/// asking the gate about an image that does not exist.
fn write_observations(path: &Path, catalog: &[&Image], live_pods: &[String]) {
    let images: Vec<Value> = catalog
        .iter()
        .map(|image| {
            json!({
                "pull_ref": image.tag.clone().unwrap_or_else(|| image.id.clone()),
                "declared": image.launcher_authority_protocol.clone(),
                "digest": image.registry_digest.clone(),
            })
        })
        .collect();
    std::fs::write(
        path,
        serde_json::to_vec(&json!({
            "catalog": images,
            "births": [],
            "live_task_run_pods": live_pods,
        }))
        .expect("the observation bundle serializes"),
    )
    .unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
}

/// A LIVE `helm template` of the shipped chart, armed the way the cutover needs.
fn armed_render(destination: &Path) {
    let output = Command::new("helm")
        .args(["template", "djinn-cutover-preflight"])
        .arg(repo_root().join("deploy/helm/djinn"))
        .arg("--is-upgrade")
        .args(ARMED_CHART_VALUES)
        .current_dir(repo_root())
        .output()
        .expect(
            "helm must be on PATH: the gate's whole claim is that it judges a LIVE render of the \
             shipped chart, so a missing helm is a failure and never a skip",
        );
    assert!(
        output.status.success(),
        "helm template failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    std::fs::write(destination, &output.stdout)
        .unwrap_or_else(|error| panic!("write {}: {error}", destination.display()));
}

/// **The mutation AC2 names, embodied.** The armed render with the `pods/resize`
/// grant's RESOURCE — and nothing else — changed to `pods`.
///
/// `pods` is not `pods/resize`, so the grant authorises nothing the lift needs
/// and every resize PATCH would be a 403. Line-scoped rather than a YAML round
/// trip on purpose: a re-serialised manifest differs from the real render in a
/// hundred incidental ways, and the claim under test is that ONE line flips the
/// verdict. Both invariants are asserted here so a refusal can never be
/// attributed to fixture drift.
fn render_with_a_broken_pods_resize_grant(source: &Path, destination: &Path) {
    const ANCHOR: &str = r#"resources: ["pods/resize"]"#;
    let text = std::fs::read_to_string(source)
        .unwrap_or_else(|error| panic!("read {}: {error}", source.display()));
    let anchors = text.lines().filter(|line| line.trim() == ANCHOR).count();
    assert_eq!(
        anchors, 1,
        "the render must carry exactly one pods/resize rule for this mutation to be surgical",
    );
    let mutated: Vec<String> = text
        .lines()
        .map(|line| {
            if line.trim() == ANCHOR {
                line.replace(r#"["pods/resize"]"#, r#"["pods"]"#)
            } else {
                line.to_owned()
            }
        })
        .collect();
    let original: Vec<&str> = text.lines().collect();
    assert_eq!(
        original.len(),
        mutated.len(),
        "the mutation must not change the line count",
    );
    assert_eq!(
        original
            .iter()
            .zip(&mutated)
            .filter(|(before, after)| *before != &**after)
            .count(),
        1,
        "the mutation must differ from the live render in exactly one line",
    );
    std::fs::write(destination, mutated.join("\n") + "\n")
        .unwrap_or_else(|error| panic!("write {}: {error}", destination.display()));
}

/// A scratch directory, namespaced per process so concurrent harnesses cannot
/// read each other's renders.
fn scratch_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("djinn-li91-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    dir
}

/// **AC9 + the cutover preflight.** The flip is gated on
/// `deploy/preflight/cutover-preflight.sh`, and a render mutated in exactly one
/// line refuses it.
///
/// Three cases, and the third is the one that makes the first two mean
/// something:
///
/// 1. A clean gate over a live render of the shipped chart, a drained database
///    and an empty Pod census CLEARS, and the driver flips.
/// 2. The same arguments with the `pods/resize` grant's resource changed to
///    `pods` are refused with exactly `pods-resize-rbac` — so the gate reads
///    the RENDER and not its flags — and the flip never happens: no
///    `AuthorityModeFlipped` in the journal, and PostgreSQL still holds the old
///    mode.
/// 3. A seeded nonterminal resize row makes the gate report `drain-fence` from
///    the production `list_nonterminal_resize` query, and the driver refuses the
///    same row by name. One fact, two independent readers.
///
/// Needs `helm`, `python3` and a prebuilt gate binary; it needs no cluster.
/// `#[ignore]`d for the same reason the crate's own render-bearing proofs are:
/// the ordinary sharded lane has PostgreSQL but not helm.
#[test]
#[ignore = "needs helm, python3 and a prebuilt cutover-preflight binary"]
fn the_cutover_preflight_gates_the_flip_and_one_defect_class_refuses_it() {
    if !live_tests_enabled() {
        return;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a tokio runtime");
    let db = runtime
        .block_on(Database::ephemeral())
        .expect("a real test database");
    let modes = LauncherAuthorityModeRepository::new(db.clone());
    let permits = BuildPodPermitRepository::new(db.clone());
    runtime.block_on(force_mode(&modes, LauncherAuthorityProtocol::LeafV1));

    let url = runtime.block_on(ephemeral_database_url(&db));
    let resize_image = runtime.block_on(seed_catalog_image(&db, ImageClass::ResizeV2));
    let leaf_image = runtime.block_on(seed_catalog_image(&db, ImageClass::LeafV1));
    let scratch = scratch_dir();

    // One bundle per direction. The catalog a cutover prepares is the catalog
    // that may run AFTER it: a declared `resize-v2` row under a `leaf-v1` mode
    // is a genuine `catalog-protocol` defect, and mixing the two would make the
    // exact-class assertions below prove something else.
    let for_resize = scratch.join("observations-resize-v2.json");
    let for_leaf = scratch.join("observations-leaf-v1.json");
    write_observations(&for_resize, &[&resize_image], &[]);
    write_observations(&for_leaf, &[&leaf_image], &[]);

    // ── 1. a clean gate opens, and the driver flips ─────────────────────
    let clear = cutover_preflight(LauncherAuthorityProtocol::ResizeV2, &url, &for_resize, None);
    assert!(
        clear.cleared() && clear.classes.is_empty(),
        "a live render of the armed chart over a drained database must CLEAR the cutover, got \
         {clear:?}",
    );

    let pods = Arc::new(DrainedPodPlane::default());
    let rollout = cutover_rollout(&db, Arc::clone(&pods) as Arc<dyn TaskRunPodPlane>);
    let probe_run = runtime.block_on(seed_task_run(&db));
    runtime.block_on(arm_at_the_flip(
        &rollout,
        probe_for(&resize_image, &probe_run),
    ));
    let epoch = runtime.block_on(current_epoch(&modes));
    let flipped = runtime
        .block_on(gated_flip(
            &rollout,
            &clear,
            epoch,
            LauncherAuthorityProtocol::ResizeV2,
        ))
        .expect("a cleared gate must let the driver flip");
    assert!(
        rollout
            .journal()
            .contains(&RolloutStep::AuthorityModeFlipped),
        "the flip must be journalled: {:?}",
        rollout.journal(),
    );
    assert_eq!(
        runtime.block_on(current_mode(&modes)),
        LauncherAuthorityProtocol::ResizeV2,
    );

    // ── 2. one mutated line refuses the flip ────────────────────────────
    let live = scratch.join("live.yaml");
    let broken = scratch.join("rbac-resource.yaml");
    armed_render(&live);
    render_with_a_broken_pods_resize_grant(&live, &broken);

    let refused = cutover_preflight(
        LauncherAuthorityProtocol::LeafV1,
        &url,
        &for_leaf,
        Some(&broken),
    );
    assert_eq!(
        refused.status,
        Some(1),
        "a broken pods/resize grant must BLOCK the cutover:\n{}",
        refused.output,
    );
    assert_eq!(
        refused.classes,
        ["pods-resize-rbac".to_owned()].into_iter().collect(),
        "exactly one class must fire, and it must be the one the mutation broke — the ARGUMENTS \
         were the healthy ones that cleared above, so the verdict came from the render:\n{}",
        refused.output,
    );

    let rollback_pods = Arc::new(DrainedPodPlane::default());
    let rollback = cutover_rollout(&db, Arc::clone(&rollback_pods) as Arc<dyn TaskRunPodPlane>);
    let rollback_run = runtime.block_on(seed_task_run(&db));
    runtime.block_on(arm_at_the_flip(
        &rollback,
        probe_for(&leaf_image, &rollback_run),
    ));
    let blocked = runtime
        .block_on(gated_flip(
            &rollback,
            &refused,
            flipped,
            LauncherAuthorityProtocol::LeafV1,
        ))
        .expect_err("a blocked gate must refuse the flip");
    assert!(
        blocked.contains("pods-resize-rbac"),
        "the refusal must carry the gate's own class: {blocked}",
    );
    assert!(
        !rollback
            .journal()
            .contains(&RolloutStep::AuthorityModeFlipped),
        "a gate-refused cutover must never journal a flip: {:?}",
        rollback.journal(),
    );
    assert_eq!(
        runtime.block_on(current_mode(&modes)),
        LauncherAuthorityProtocol::ResizeV2,
        "a gate-refused cutover must leave PostgreSQL exactly where it was",
    );

    // ── 3. the drain fence, read twice from one fact ────────────────────
    let task_run_id = runtime.block_on(seed_task_run(&db));
    let (_row, pod_uid) = runtime.block_on(seed_nonterminal_resize_row(&permits, &task_run_id));

    let fenced = cutover_preflight(LauncherAuthorityProtocol::LeafV1, &url, &for_leaf, None);
    assert_eq!(
        fenced.classes,
        ["drain-fence".to_owned()].into_iter().collect(),
        "the seeded row must reach the gate through the production \
         list_nonterminal_resize query:\n{}",
        fenced.output,
    );

    let fence_pods = Arc::new(DrainedPodPlane::default());
    let fence_rollout = cutover_rollout(&db, Arc::clone(&fence_pods) as Arc<dyn TaskRunPodPlane>);
    runtime.block_on(arm_at_the_flip(
        &fence_rollout,
        probe_for(&leaf_image, &task_run_id),
    ));
    let named = runtime
        .block_on(fence_rollout.prove_drained())
        .expect_err("the driver must refuse the same row");
    let RolloutBlocked::NonterminalResizeRows(rows) = &named else {
        panic!("the driver must name the rows the gate counted, got {named:?}");
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].task_run_id, task_run_id);
    assert_eq!(rows[0].pod_uid.as_deref(), Some(pod_uid.as_str()));

    assert_eq!(
        (
            pods.creations(),
            rollback_pods.creations(),
            fence_pods.creations()
        ),
        (0, 0, 0),
        "no cutover in this test may materialise a Pod",
    );
    let _ = std::fs::remove_dir_all(&scratch);
}

/// **The gate itself.** The deploy-time preflight decides, and only then does
/// the rollout driver touch the authority mode.
///
/// Written as one function used by every flip below so the ordering cannot be
/// forgotten at a call site: a `PreflightVerdict` that did not clear returns
/// before `prove_drained` is reached, so a blocked gate leaves the journal — and
/// the durable mode — untouched.
async fn gated_flip(
    rollout: &ResizeRollout,
    verdict: &PreflightVerdict,
    expected_epoch: i64,
    target: LauncherAuthorityProtocol,
) -> Result<i64, String> {
    if !verdict.cleared() {
        return Err(format!(
            "the cutover preflight refused: status={:?} classes={:?}",
            verdict.status, verdict.classes,
        ));
    }
    rollout
        .prove_drained()
        .await
        .map_err(|blocked| format!("the drain proof refused: {blocked}"))?;
    rollout
        .flip_authority_mode(expected_epoch, target)
        .await
        .map_err(|blocked| format!("the flip refused: {blocked}"))
}

/// **AC9, cluster half.** A REAL task-run Pod on the cluster blocks the flip —
/// both through the rollout driver's own Pod census and through the deploy-time
/// preflight — and the flip proceeds once it is gone.
///
/// The live Pod is the dimension PostgreSQL cannot see. `set_mode`'s fence
/// cannot answer it at all: a Pod that outlived its permit is invisible to every
/// count it can take. `ResizeRollout::prove_drained` enumerates the apiserver
/// first and refuses with `LiveTaskRunPods`, naming the Pod, its UID and its
/// task run.
#[test]
#[ignore = "requires scripts/kind/setup-resize-matrix-cluster.sh up"]
fn a_live_pod_on_the_cluster_blocks_the_flip() {
    if !live_tests_enabled() {
        return;
    }
    harness_context();
    let images = BuiltImages::ensure();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a tokio runtime");
    let db = runtime
        .block_on(Database::ephemeral())
        .expect("a real test database");
    let modes = LauncherAuthorityModeRepository::new(db.clone());
    let permits = BuildPodPermitRepository::new(db.clone());
    runtime.block_on(force_mode(&modes, LauncherAuthorityProtocol::LeafV1));

    let url = runtime.block_on(ephemeral_database_url(&db));
    let leaf_image = runtime.block_on(seed_catalog_image(&db, ImageClass::LeafV1));
    // The catalog a forward cutover prepares is the one that may run AFTER it,
    // so the bundle handed to the gate carries the `resize-v2` row and not the
    // `leaf-v1` row the blocking Pod is running.
    let resize_image = runtime.block_on(seed_catalog_image(&db, ImageClass::ResizeV2));
    let scratch = scratch_dir();
    let observations = scratch.join("observations.json");

    // A REAL Pod, under the mode we are about to try to leave.
    let cell = Cell {
        image: ImageClass::LeafV1,
        mode: ServerMode::Preparation,
        expected: expectation(ImageClass::LeafV1, ServerMode::Preparation),
    };
    let task_run_id = runtime.block_on(seed_task_run(&db));
    let live = dispatch_as(&images, cell, task_run_id);

    // The permit that the dispatch path would have taken for it. Acquired here
    // because this harness drives the renderer directly rather than the whole
    // dispatch actor; the ROW is the production one either way.
    let row = match runtime.block_on(permits.acquire(&live.task_run_id, 4)) {
        AcquireBuildPodPermitResult::Acquired { row, .. } => row,
        other => panic!("acquire the live Pod's permit: {other:?}"),
    };

    // The Pod really is alive — otherwise every refusal below is about a stale
    // row rather than about live cluster state.
    let phase = pod_json(&live.pod)["status"]["phase"].clone();
    assert_eq!(
        phase, "Running",
        "the blocking Pod must actually be running, got {phase:?}",
    );

    // ── the driver's own Pod census refuses, naming the Pod ─────────────
    let rollout = cutover_rollout(&db, Arc::new(ClusterPodPlane));
    runtime.block_on(arm_at_the_flip(
        &rollout,
        probe_for(&leaf_image, &live.task_run_id),
    ));
    let blocked = runtime
        .block_on(rollout.prove_drained())
        .expect_err("a live task-run Pod must block the drain proof");
    let RolloutBlocked::LiveTaskRunPods(pods) = &blocked else {
        panic!(
            "the driver must refuse on the Pod census it took from the apiserver, got {blocked:?}. \
             `set_mode`'s fence cannot see a Pod at all, so a refusal of any other shape here \
             would be about a database row."
        );
    };
    let named = pods
        .iter()
        .find(|pod| pod.pod_name == live.pod)
        .unwrap_or_else(|| panic!("the census must name Pod {}: {pods:?}", live.pod));
    assert_eq!(
        named.pod_uid, live.pod_uid,
        "the refusal must carry the immutable UID the resize fence is taken against",
    );
    assert_eq!(named.task_run_id, live.task_run_id);

    // ── and so does the deploy-time gate, from the same census ──────────
    let census: Vec<String> = live_task_run_pods_on_cluster()
        .into_iter()
        .map(|pod| pod.pod_name)
        .collect();
    assert!(
        census.contains(&live.pod),
        "the census fed to the gate must contain the live Pod: {census:?}",
    );
    write_observations(&observations, &[&resize_image], &census);
    let refused = cutover_preflight(
        LauncherAuthorityProtocol::ResizeV2,
        &url,
        &observations,
        None,
    );
    assert_eq!(
        refused.classes,
        ["drain-fence".to_owned()].into_iter().collect(),
        "a live task-run Pod must reach the deploy-time gate as the drain-fence defect, and as \
         that defect ALONE — every other class is render-derived and the render did not \
         change:\n{}",
        refused.output,
    );

    let epoch = runtime.block_on(current_epoch(&modes));
    assert!(
        runtime
            .block_on(gated_flip(
                &rollout,
                &refused,
                epoch,
                LauncherAuthorityProtocol::ResizeV2,
            ))
            .is_err(),
        "neither the gate nor the driver may let this flip through",
    );
    assert!(
        !rollout
            .journal()
            .contains(&RolloutStep::AuthorityModeFlipped),
        "a blocked cutover must never journal a flip: {:?}",
        rollout.journal(),
    );
    assert_eq!(
        runtime.block_on(current_mode(&modes)),
        LauncherAuthorityProtocol::LeafV1,
        "a blocked cutover must leave PostgreSQL exactly where it was",
    );

    // ── the Pod goes, and the same driver flips ─────────────────────────
    delete_cell(&live);
    runtime
        .block_on(permits.release(
            &live.task_run_id,
            &row.permit_id,
            row.fencing_token,
            "matrix",
        ))
        .expect("release the permit");
    for _ in 0..AWAIT_TICKS {
        if live_task_run_pods_on_cluster().is_empty() {
            break;
        }
        std::thread::sleep(TICK);
    }
    assert!(
        live_task_run_pods_on_cluster().is_empty(),
        "the harness must reach an empty Pod census before the flip is expected to succeed",
    );

    write_observations(&observations, &[&resize_image], &[]);
    let cleared = cutover_preflight(
        LauncherAuthorityProtocol::ResizeV2,
        &url,
        &observations,
        None,
    );
    assert!(
        cleared.cleared(),
        "with the Pod gone and the permit released the gate must clear:\n{}",
        cleared.output,
    );
    runtime
        .block_on(gated_flip(
            &rollout,
            &cleared,
            epoch,
            LauncherAuthorityProtocol::ResizeV2,
        ))
        .expect("with the cluster drained, the cutover must proceed");
    assert_eq!(
        rollout.journal(),
        vec![
            RolloutStep::RetentionVerified,
            RolloutStep::AdmissionPaused,
            RolloutStep::DrainProven,
            RolloutStep::AuthorityModeFlipped,
        ],
        "the observed step sequence is the ordering proof",
    );
    assert_eq!(
        runtime.block_on(current_mode(&modes)),
        LauncherAuthorityProtocol::ResizeV2,
    );
    let _ = std::fs::remove_dir_all(&scratch);
}
