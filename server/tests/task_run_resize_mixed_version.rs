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
//! DJINN_TEST_RESIZE_MATRIX=1 cargo test -p djinn-server \
//!     --test task_run_resize_mixed_version -- --ignored --test-threads=1
//! scripts/kind/setup-resize-matrix-cluster.sh down       # ALWAYS, pass or fail
//! scripts/kind/setup-resize-matrix-cluster.sh selfcheck
//! ```

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use djinn_db::launcher_compatibility::{
    AdmissionDecision, AdmissionRejection, InventoryFault, LegacyDigestInventory,
    PreProtocolDigest, admit_with_legacy_inventory,
};
use djinn_db::{
    AcquireBuildPodPermitResult, BuildPodPermitRepository, Database,
    LauncherAuthorityModeRepository, LauncherProtocolAdmission, SetLauncherAuthorityModeResult,
};
use djinn_k8s::config::KubernetesConfig;
use djinn_k8s::job::{LABEL_TASK_RUN_ID, build_task_run_job};
use djinn_k8s::launcher::{
    CgroupLauncherMode, LAUNCHER_CONTAINER_NAME, apply_launcher_authority_protocol,
    render_authority_protocol,
};
use djinn_k8s::pod_resize::{CpuLimit, build_resize_patch};
use djinn_launcher_protocol::{LEAF_V1_WIRE, LauncherAuthorityProtocol, RESIZE_V2_WIRE};
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

/// **AC7, source gate.** No helper in this suite reads the regular-container
/// status list.
///
/// The forbidden token is ASSEMBLED at run time rather than written out, so this
/// gate does not trip on itself. `initContainerStatuses` is not a match: the
/// forbidden spelling has a lower-case `c` that `initContainerStatuses` does not
/// contain at that position.
#[test]
fn no_helper_in_this_suite_reads_the_regular_container_status_list() {
    let forbidden = [
        format!("{}{}", "container", "Statuses"),
        format!("{}_{}", "container", "statuses"),
    ];
    let body = std::fs::read_to_string(this_file()).expect("this test file is readable");
    assert!(
        body.contains("initContainerStatuses"),
        "the suite must actually name the ONLY confirmation site, or this gate guards nothing",
    );
    for token in forbidden {
        assert!(
            !body.contains(&token),
            "`{token}` appears in this suite. Confirmation for the native sidecar comes ONLY from \
             status.initContainerStatuses[name={LAUNCHER_CONTAINER_NAME}]; the regular-container \
             list can carry a matching, stale or misleading entry and reading it is the defect \
             this gate exists to prevent",
        );
    }
}

/// **AC10, hermetic half.** The harness script exists, is disposable, refuses
/// every sibling's target, pins its context, and cannot have its floor walked
/// back to the measured-false 1.29.
#[test]
fn the_kind_harness_is_disposable_and_isolated() {
    let script = repo_root().join("scripts/kind/setup-resize-matrix-cluster.sh");
    let body = std::fs::read_to_string(&script)
        .unwrap_or_else(|error| panic!("{} must exist: {error}", script.display()));

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
        body.contains("trap cleanup_on_failure EXIT"),
        "the failure path must tear the cluster down; without the trap a failed `up` leaves a \
         cluster behind and the host fills up",
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
        !body.contains("MIN_K8S_MINOR=29"),
        "the 1.29 floor was measured false and corrected in #2818; it must not come back",
    );
    assert!(
        body.contains("eks") || body.contains("EKS"),
        "the script must say out loud why a non-kind server is refused",
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
// AC9: the flips are fenced, against real PostgreSQL and real cluster state.
// ===========================================================================

/// **AC9, durable half.** Activation refuses to proceed while a live task-run
/// permit exists, and rollback refuses until every nonterminal resize row is
/// gone.
///
/// Real PostgreSQL, real repositories, no `Fake`. The permit is created through
/// `BuildPodPermitRepository::acquire` — the production admission call — so the
/// row the fence counts is the row dispatch actually writes.
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

    let task_run_id = seed_task_run(&db).await;
    let acquired = permits.acquire(&task_run_id, 4).await;
    let row = match acquired {
        AcquireBuildPodPermitResult::Acquired { row, .. } => row,
        other => panic!("the fixture must be able to acquire a permit: {other:?}"),
    };

    let census = modes.drain_census().await.expect("read the drain census");
    assert!(
        !census.is_drained(),
        "one live permit must make the drain non-empty: {census:?}",
    );

    // Activation: refused.
    let epoch = modes
        .read()
        .await
        .expect("read the mode")
        .expect("the singleton row")
        .epoch;
    let activation = modes
        .set_mode(epoch, LauncherAuthorityProtocol::ResizeV2)
        .await;
    assert!(
        matches!(
            activation,
            SetLauncherAuthorityModeResult::DrainNotEmpty { .. }
        ),
        "activation must refuse to proceed with a live Pod's permit outstanding, got {activation:?}",
    );
    assert_eq!(
        modes
            .read()
            .await
            .expect("read the mode")
            .expect("the singleton row")
            .mode,
        LauncherAuthorityProtocol::LeafV1,
        "a refused activation must not have flipped the mode anyway: an Err returned AFTER the \
         flip is exactly the shape this assertion exists to catch",
    );

    // Drain, then activation succeeds.
    permits
        .release(&task_run_id, &row.permit_id, row.fencing_token, "matrix")
        .await
        .expect("release the permit");
    let activation = modes
        .set_mode(epoch, LauncherAuthorityProtocol::ResizeV2)
        .await;
    assert!(
        matches!(activation, SetLauncherAuthorityModeResult::Flipped { .. }),
        "with the drain empty, activation must proceed: {activation:?}",
    );

    // Rollback: refused again while a permit is live.
    let rollback_task = seed_task_run(&db).await;
    let rollback_row = match permits.acquire(&rollback_task, 4).await {
        AcquireBuildPodPermitResult::Acquired { row, .. } => row,
        other => panic!("acquire a second permit: {other:?}"),
    };
    let epoch = modes
        .read()
        .await
        .expect("read the mode")
        .expect("the singleton row")
        .epoch;
    let rollback = modes
        .set_mode(epoch, LauncherAuthorityProtocol::LeafV1)
        .await;
    assert!(
        matches!(
            rollback,
            SetLauncherAuthorityModeResult::DrainNotEmpty { .. }
        ),
        "rollback must not start until every resize-v2 Pod is gone, got {rollback:?}",
    );
    assert_eq!(
        modes
            .read()
            .await
            .expect("read the mode")
            .expect("the singleton row")
            .mode,
        LauncherAuthorityProtocol::ResizeV2,
        "a refused rollback must leave the mode where it was",
    );

    permits
        .release(
            &rollback_task,
            &rollback_row.permit_id,
            rollback_row.fencing_token,
            "matrix",
        )
        .await
        .expect("release the second permit");
    let rollback = modes
        .set_mode(epoch, LauncherAuthorityProtocol::LeafV1)
        .await;
    assert!(
        matches!(rollback, SetLauncherAuthorityModeResult::Flipped { .. }),
        "with the drain empty, rollback must proceed: {rollback:?}",
    );
}

/// **AC9, cluster half.** A live task-run Pod on the cluster blocks activation,
/// and the catalog selection is restored before rollback completes.
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

    let epoch = runtime
        .block_on(modes.read())
        .expect("read the mode")
        .expect("the singleton row")
        .epoch;
    let refused = runtime.block_on(modes.set_mode(epoch, LauncherAuthorityProtocol::ResizeV2));
    assert!(
        matches!(
            refused,
            SetLauncherAuthorityModeResult::DrainNotEmpty { .. }
        ),
        "activation must refuse while Pod {} is alive on the cluster, got {refused:?}",
        live.pod_uid,
    );

    // The Pod really is alive — otherwise the refusal above is about a stale row
    // rather than about live cluster state.
    let phase = pod_json(&live.pod)["status"]["phase"].clone();
    assert_eq!(
        phase, "Running",
        "the blocking Pod must actually be running, got {phase:?}",
    );

    delete_cell(&live);
    runtime
        .block_on(permits.release(
            &live.task_run_id,
            &row.permit_id,
            row.fencing_token,
            "matrix",
        ))
        .expect("release the permit");

    let allowed = runtime.block_on(modes.set_mode(epoch, LauncherAuthorityProtocol::ResizeV2));
    assert!(
        matches!(allowed, SetLauncherAuthorityModeResult::Flipped { .. }),
        "with the Pod gone and the permit released, activation must proceed: {allowed:?}",
    );
}
