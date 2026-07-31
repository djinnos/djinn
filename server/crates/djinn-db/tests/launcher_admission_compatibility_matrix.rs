//! **AC1 + AC3.** The mixed-artifact admission matrix.
//!
//! Three artifact shapes — a real no-handshake legacy build, an explicit
//! `leaf-v1` build, and an explicit `resize-v2` build — resolved through the
//! **real** [`ImageRepository`] and [`ProjectRepository`] against **both**
//! server authority modes and every state the pre-protocol digest inventory can
//! be in.
//!
//! Nothing here stubs the decision. Each row writes an `images` row through
//! `mark_ready` (so migration 166's CHECK and the repository's own digest guard
//! both apply), flips the durable `launcher_authority_mode` singleton through
//! z3gi's own drain fence (migration 167), selects the image from a `projects`
//! row, and reads it back through `resolve_dispatch_image` — the seam
//! `djinn_k8s::runtime::prepare` calls before it builds a Job. An "admitted"
//! cell therefore proves the whole path, and a "rejected" cell proves the
//! refusal lands before any Kubernetes object exists — long before a shell
//! runs.
//!
//! Before this task both halves of that path were inert: z3gi's
//! `decide_launcher_protocol_admission` had no dispatch caller, and #2836's
//! render read the artifact's declaration with nothing consulting the persisted
//! authority. `resolve_dispatch_image` is where they meet.
//!
//! The invariant every admitted cell asserts is **exactly one** effective quota
//! authority: the admitted protocol equals the server mode, and (when the
//! artifact declared anything at all) equals the declaration too. Two
//! components writing one leaf's `cpu.max` is the failure this matrix exists to
//! make impossible.

use djinn_db::launcher_compatibility::{
    AdmissionDecision, AdmissionRejection, InventoryFault, LegacyDigestInventory,
    PreProtocolDigest, decide_admission,
};
use djinn_db::repositories::launcher_authority_mode::{
    LauncherAuthorityModeRepository, LauncherProtocolAdmission, SetLauncherAuthorityModeResult,
};
use djinn_db::{Database, ImageRepository, ProjectRepository};
use djinn_launcher_protocol::LauncherAuthorityProtocol;

/// Canonical `sha256:` + 64 lowercase hex. `seed` distinguishes artifacts.
fn digest(seed: &str) -> String {
    let mut hex = seed.to_owned();
    hex.truncate(8);
    format!("sha256:{hex}{}", "0".repeat(64 - hex.len()))
}

fn inventoried(seed: &str) -> LegacyDigestInventory {
    LegacyDigestInventory::verified(
        "platform-ops",
        "2026-07-30T00:00:00Z",
        [PreProtocolDigest::parse(&digest(seed)).unwrap()],
    )
}

/// One catalog artifact, written through the real repositories.
struct Artifact {
    label: &'static str,
    declared: Option<LauncherAuthorityProtocol>,
    digest: Option<String>,
}

impl Artifact {
    /// The exact shape a live deployment already holds: `ready`, no
    /// declaration, pinned to an immutable digest.
    fn legacy_no_handshake() -> Self {
        Self {
            label: "legacy no-handshake (digest-pinned)",
            declared: None,
            digest: Some(digest("1eeac9de")),
        }
    }

    /// The other legacy shape migration 166 deliberately keeps legal: `ready`,
    /// no declaration, `registry_digest IS NULL`.
    fn legacy_no_handshake_undigested() -> Self {
        Self {
            label: "legacy no-handshake (no digest)",
            declared: None,
            digest: None,
        }
    }

    fn declaring(protocol: LauncherAuthorityProtocol) -> Self {
        Self {
            label: match protocol {
                LauncherAuthorityProtocol::LeafV1 => "explicit leaf-v1",
                LauncherAuthorityProtocol::ResizeV2 => "explicit resize-v2",
            },
            declared: Some(protocol),
            digest: Some(digest("dec1a2ed")),
        }
    }
}

/// Seed a project selecting one artifact, and hand back a resolver bound to
/// `mode` + `inventory`.
async fn resolve(
    artifact: &Artifact,
    mode: LauncherAuthorityProtocol,
    inventory: LegacyDigestInventory,
) -> djinn_db::Result<Option<djinn_db::DispatchImage>> {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    ProjectRepository::new(db.clone(), djinn_core::events::EventBus::noop())
        .create_with_id("p1", "p-1", "test", "p1")
        .await
        .unwrap();
    let images = ImageRepository::new(db.clone());
    images.create("i1", "Fixture", None, "{}").await.unwrap();
    images
        .mark_ready(
            "i1",
            "reg/djinn-image-i1:contenthash",
            artifact.digest.as_deref(),
            artifact.declared,
        )
        .await
        .expect("the fixture must be writable through the real repository");
    images.set_project_image("p1", Some("i1")).await.unwrap();

    // The authority mode is the DURABLE singleton migration 167 seeds, flipped
    // through z3gi's own drain fence — not a value this test hands the
    // resolver. A fresh database has no live permits, so the fence lets the
    // flip through and the resolution reads a mode that is really stored.
    let modes = LauncherAuthorityModeRepository::new(db.clone());
    let epoch = modes
        .read()
        .await
        .unwrap()
        .expect("migration 167 seeds the singleton")
        .epoch;
    assert!(
        matches!(
            modes.set_mode(epoch, mode).await,
            SetLauncherAuthorityModeResult::Flipped { .. }
                | SetLauncherAuthorityModeResult::Unchanged { .. }
        ),
        "the authority singleton must be settable to {mode}"
    );

    ProjectRepository::new(db.clone(), djinn_core::events::EventBus::noop())
        .with_legacy_digest_inventory(inventory)
        .resolve_dispatch_image("p1")
        .await
}

/// **AC3.** Every admitted combination yields exactly one effective quota
/// authority, and it is the server's mode.
///
/// Mutation that fails it: make `decide_admission` return
/// `Ok(AdmissionDecision::Admitted(declared))` for a declaration that does not
/// equal `mode` (i.e. drop the `declared == mode` guard). The `resize-v2`
/// declaration under `leaf-v1` then resolves, and the `assert_eq!` on
/// `admitted == mode` fails naming both protocols.
#[tokio::test]
async fn every_admitted_combination_has_exactly_one_effective_authority() {
    let mut admitted_cells = 0;
    for mode in LauncherAuthorityProtocol::ALL {
        for artifact in [
            Artifact::legacy_no_handshake(),
            Artifact::legacy_no_handshake_undigested(),
            Artifact::declaring(LauncherAuthorityProtocol::LeafV1),
            Artifact::declaring(LauncherAuthorityProtocol::ResizeV2),
        ] {
            // The inventory vouches for the legacy artifact, so the only thing
            // that can reject it in this sweep is the mode.
            let inventory = inventoried("1eeac9de");
            let resolved = resolve(&artifact, mode, inventory).await;

            let Ok(Some(image)) = resolved else {
                continue; // rejected; the rejection sweeps below cover why
            };
            let Some(admitted) = image.authority_protocol else {
                // Undeclarable: no digest, no declaration. The render refuses
                // it; there is no authority to be one-of.
                assert!(
                    image.digest.is_none() && image.declared_authority_protocol.is_none(),
                    "{}: only an unidentified artifact may resolve without an authority",
                    artifact.label
                );
                continue;
            };

            admitted_cells += 1;
            assert_eq!(
                admitted, mode,
                "{} admitted under mode {mode} must run under {mode} and nothing else",
                artifact.label
            );
            if let Some(declared) = image.declared_authority_protocol {
                assert_eq!(
                    admitted, declared,
                    "{}: an admitted declaration is never reinterpreted",
                    artifact.label
                );
            }
        }
    }
    assert_eq!(
        admitted_cells, 3,
        "the matrix must actually admit something: leaf-v1 mode admits the legacy and the \
         leaf-v1 artifacts, resize-v2 mode admits the resize-v2 artifact"
    );
}

/// **AC1, the compatibility grant.** A missing protocol becomes `leaf-v1` only
/// for an exact inventoried digest under `leaf-v1` mode.
///
/// Mutation that fails it: make `LegacyDigestInventory::vouches_for` return
/// `Ok(true)` for the `Verified` arm regardless of membership. The
/// "uninventoried" assertion below then resolves instead of erroring.
#[tokio::test]
async fn a_missing_protocol_is_admitted_only_for_an_exact_inventoried_digest() {
    let artifact = Artifact::legacy_no_handshake();

    let admitted = resolve(
        &artifact,
        LauncherAuthorityProtocol::LeafV1,
        inventoried("1eeac9de"),
    )
    .await
    .expect("an inventoried legacy digest is admitted")
    .expect("project resolves");
    assert_eq!(
        admitted.authority_protocol,
        Some(LauncherAuthorityProtocol::LeafV1)
    );
    assert_eq!(admitted.declared_authority_protocol, None);

    // A different digest, correctly formed, that the inventory does not list.
    let rejected = resolve(
        &artifact,
        LauncherAuthorityProtocol::LeafV1,
        inventoried("d1ffe2ed"),
    )
    .await
    .expect_err("an uninventoried digest must not dispatch");
    let rendered = rejected.to_string();
    assert!(
        rendered.contains("not listed in the pre-protocol digest inventory"),
        "the refusal must say the digest is not vouched for, got: {rendered}"
    );
    assert!(
        rendered.contains(&digest("1eeac9de")),
        "the refusal must name the exact digest an operator has to add, got: {rendered}"
    );
}

/// **AC1, the resize-v2 half.** No missing-protocol artifact survives
/// `resize-v2`, inventoried or not.
///
/// Mutation that fails it: delete the `mode != LeafV1` arm in
/// `decide_admission`. Both `expect_err`s below then return `Ok`.
#[tokio::test]
async fn no_missing_protocol_artifact_survives_resize_v2() {
    for inventory in [
        inventoried("1eeac9de"), // even vouched for
        LegacyDigestInventory::Unconfigured,
    ] {
        let error = resolve(
            &Artifact::legacy_no_handshake(),
            LauncherAuthorityProtocol::ResizeV2,
            inventory,
        )
        .await
        .expect_err("a no-handshake artifact must never be admitted under resize-v2");
        assert!(
            error
                .to_string()
                .contains("declares no launcher authority protocol"),
            "got: {error}"
        );
    }
}

/// **AC1, the mutable-tag half.** An artifact whose only identity is a tag is
/// never admitted, and neither is one whose digest is not canonical.
///
/// Note the split, which is migration 166's and #2836's, not a new one:
///
/// * a digest that *exists* but is not canonical is a positive claim that fails
///   → the resolve errors;
/// * `registry_digest IS NULL` is the shape migration 166 deliberately keeps
///   legal so an upgrade cannot strand a live catalog → it resolves with **no
///   authority**, and `render_authority_protocol` refuses to build the Job.
///
/// Either way nothing dispatches. Mutation that fails it: make
/// `PreProtocolDigest::parse` accept any string starting with `sha256:` — the
/// malformed sweep then resolves.
#[tokio::test]
async fn a_mutable_tag_without_a_verified_digest_never_dispatches() {
    // Not canonical: short, uppercase, wrong algorithm, and a bare tag.
    for bad in [
        "sha256:abc",
        "SHA256:7822B7DE0000000000000000000000000000000000000000000000000000CAFE",
        "sha512:7822b7de0000000000000000000000000000000000000000000000000000cafe",
        "reg/djinn-image-i1:contenthash",
    ] {
        let artifact = Artifact {
            label: "malformed digest",
            declared: None,
            digest: Some(bad.to_owned()),
        };
        let error = resolve(
            &artifact,
            LauncherAuthorityProtocol::LeafV1,
            LegacyDigestInventory::Unconfigured,
        )
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("is not a canonical immutable digest"),
            "{bad:?} must be refused as a non-canonical digest, got: {error}"
        );
    }

    // The digestless legacy row: resolvable, but carrying no authority.
    let undigested = resolve(
        &Artifact::legacy_no_handshake_undigested(),
        LauncherAuthorityProtocol::LeafV1,
        LegacyDigestInventory::Unconfigured,
    )
    .await
    .expect("migration 166 keeps this row legal, so the resolve must not error")
    .expect("project resolves");
    assert_eq!(
        undigested.authority_protocol, None,
        "an unidentified artifact must reach the render with no authority, so the render \
         refuses rather than the resolver inventing one"
    );
    assert!(
        undigested.pull_ref().is_some(),
        "#2831's anti-wedge guard: the row itself stays resolvable"
    );
}

/// **AC1, the explicit-mismatch half.** A declaration the server does not run
/// is refused, in both directions.
///
/// Mutation that fails it: replace the `declared == mode` comparison in
/// `decide_admission` with `true`. Both directions then resolve.
#[tokio::test]
async fn an_explicit_protocol_mismatch_is_refused_in_both_directions() {
    for mode in LauncherAuthorityProtocol::ALL {
        for declared in LauncherAuthorityProtocol::ALL {
            if declared == mode {
                continue;
            }
            let error = resolve(
                &Artifact::declaring(declared),
                mode,
                LegacyDigestInventory::Unconfigured,
            )
            .await
            .expect_err("a declaration that disagrees with the server must not dispatch");
            let rendered = error.to_string();
            assert!(
                rendered.contains(declared.as_wire()) && rendered.contains(mode.as_wire()),
                "the refusal must name both protocols, got: {rendered}"
            );
        }
    }
}

/// **AC2, the fail-closed half.** An inventory that fails its own validation
/// rejects every legacy artifact — it never falls back to the unarmed rule.
///
/// Mutation that fails it: make `LegacyDigestInventory::vouches_for` return
/// `Ok(true)` for the `Unusable` arm (i.e. treat a broken inventory like an
/// absent one). The first assertion then resolves.
#[tokio::test]
async fn an_unusable_inventory_rejects_rather_than_falling_back() {
    let error = resolve(
        &Artifact::legacy_no_handshake(),
        LauncherAuthorityProtocol::LeafV1,
        LegacyDigestInventory::Unusable(InventoryFault::SignatureInvalid),
    )
    .await
    .expect_err("a legacy artifact must not be admitted on an untrusted inventory");
    assert!(
        error.to_string().contains("cannot be trusted"),
        "got: {error}"
    );

    // A DECLARING artifact does not consult the inventory at all, so a broken
    // inventory must not take the whole catalog down with it.
    let declaring = resolve(
        &Artifact::declaring(LauncherAuthorityProtocol::LeafV1),
        LauncherAuthorityProtocol::LeafV1,
        LegacyDigestInventory::Unusable(InventoryFault::SignatureInvalid),
    )
    .await
    .expect("a declaring artifact is admitted on its own declaration")
    .expect("project resolves");
    assert_eq!(
        declaring.authority_protocol,
        Some(LauncherAuthorityProtocol::LeafV1)
    );
}

/// The deliberate, tested behavior change: arming the inventory is what makes a
/// previously-dispatchable undeclared artifact stop dispatching.
///
/// This is the one case where the fence takes something away, so it is stated
/// explicitly rather than left as a surprise. Before an inventory exists the
/// membership question has no configured answer and is not asked — which is
/// exactly what keeps a live catalog of pre-#2831 digest-pinned images
/// dispatching across the upgrade. Configuring an inventory arms it.
#[tokio::test]
async fn arming_the_inventory_is_what_narrows_an_undeclared_artifact() {
    let artifact = Artifact::legacy_no_handshake();

    let unarmed = resolve(
        &artifact,
        LauncherAuthorityProtocol::LeafV1,
        LegacyDigestInventory::Unconfigured,
    )
    .await
    .expect("with no inventory configured the pre-existing rule stands")
    .expect("project resolves");
    assert_eq!(
        unarmed.authority_protocol,
        Some(LauncherAuthorityProtocol::LeafV1)
    );

    // The same artifact, once an inventory exists that does not list it.
    assert!(
        resolve(
            &artifact,
            LauncherAuthorityProtocol::LeafV1,
            inventoried("d1ffe2ed"),
        )
        .await
        .is_err(),
        "an armed inventory that omits this digest must stop it dispatching"
    );
}

/// The pure decision and the repository seam agree cell for cell.
///
/// The matrix above proves the decision through the database. This proves the
/// database path is not a second implementation: for every combination, the
/// admitted protocol the repository reports is exactly what
/// [`decide_admission`] returns for the same inputs.
#[tokio::test]
async fn the_repository_seam_reports_exactly_what_the_decision_returns() {
    for mode in LauncherAuthorityProtocol::ALL {
        for inventory in [
            LegacyDigestInventory::Unconfigured,
            inventoried("1eeac9de"),
            inventoried("d1ffe2ed"),
            LegacyDigestInventory::Unusable(InventoryFault::SignatureInvalid),
        ] {
            for artifact in [
                Artifact::legacy_no_handshake(),
                Artifact::legacy_no_handshake_undigested(),
                Artifact::declaring(LauncherAuthorityProtocol::LeafV1),
                Artifact::declaring(LauncherAuthorityProtocol::ResizeV2),
            ] {
                let expected = decide_admission(
                    mode,
                    artifact.declared,
                    artifact.digest.as_deref(),
                    &inventory,
                );
                let observed = resolve(&artifact, mode, inventory.clone()).await;
                match (expected, observed) {
                    (Ok(AdmissionDecision::Admitted(protocol)), Ok(Some(image))) => {
                        assert_eq!(
                            image.authority_protocol,
                            Some(protocol),
                            "{} under {mode}",
                            artifact.label
                        );
                    }
                    (Ok(AdmissionDecision::Undeclarable), Ok(Some(image))) => {
                        assert_eq!(
                            image.authority_protocol, None,
                            "{} under {mode}",
                            artifact.label
                        );
                    }
                    (Err(rejection), Err(error)) => {
                        assert!(
                            error.to_string().contains(&rejection.to_string()),
                            "{} under {mode}: the repository must surface the decision's own \
                             reason, got {error}",
                            artifact.label
                        );
                    }
                    (expected, observed) => panic!(
                        "{} under {mode}: the decision and the repository disagree \
                         ({expected:?} vs {observed:?})",
                        artifact.label
                    ),
                }
            }
        }
    }
}

/// Non-vacuity for the rejection assertions above: the shapes they reject are
/// rejected by the *decision*, not by an unwritable fixture.
#[test]
fn the_rejection_reasons_are_distinct_and_none_is_a_catch_all() {
    let leaf = LauncherAuthorityProtocol::LeafV1;
    let resize = LauncherAuthorityProtocol::ResizeV2;
    let listed = digest("1eeac9de");
    let unlisted = digest("d1ffe2ed");
    let inventory = inventoried("1eeac9de");

    assert!(matches!(
        decide_admission(leaf, Some(resize), Some(&listed), &inventory),
        Err(AdmissionRejection::Declared(
            LauncherProtocolAdmission::ProtocolMismatch { .. }
        ))
    ));
    assert!(matches!(
        decide_admission(resize, None, Some(&listed), &inventory),
        Err(AdmissionRejection::MissingDeclarationUnderMode { .. })
    ));
    assert!(matches!(
        decide_admission(leaf, None, Some("sha256:abc"), &inventory),
        Err(AdmissionRejection::MalformedDigest(_))
    ));
    assert!(matches!(
        decide_admission(leaf, None, Some(&unlisted), &inventory),
        Err(AdmissionRejection::UninventoriedDigest(_))
    ));
    assert!(matches!(
        decide_admission(
            leaf,
            None,
            Some(&listed),
            &LegacyDigestInventory::Unusable(InventoryFault::SignatureInvalid)
        ),
        Err(AdmissionRejection::InventoryUnusable(_))
    ));
    // …and the one combination that is admitted really is admitted, so the
    // five above are not all failing for the same reason.
    assert_eq!(
        decide_admission(leaf, None, Some(&listed), &inventory),
        Ok(AdmissionDecision::Admitted(leaf))
    );
}

/// The authority is durable state, not a parameter: with the singleton gone,
/// even a perfectly inventoried legacy artifact stops dispatching.
///
/// This is the one input the caller cannot supply, so it is the one that proves
/// the resolver really reads z3gi's row rather than defaulting to `leaf-v1`.
/// Mutation that fails it: make `admit_declared_protocol`'s `Ok(None)` arm
/// return `Admitted { mode: LauncherAuthorityProtocol::default() }`. The
/// `expect_err` below returns `Ok`.
#[tokio::test]
async fn an_unreadable_authority_singleton_refuses_every_dispatch() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    ProjectRepository::new(db.clone(), djinn_core::events::EventBus::noop())
        .create_with_id("p1", "p-1", "test", "p1")
        .await
        .unwrap();
    let images = ImageRepository::new(db.clone());
    images.create("i1", "Fixture", None, "{}").await.unwrap();
    images
        .mark_ready(
            "i1",
            "reg/djinn-image-i1:contenthash",
            Some(&digest("1eeac9de")),
            None,
        )
        .await
        .unwrap();
    images.set_project_image("p1", Some("i1")).await.unwrap();

    // Sanity: the same artifact dispatches while the singleton is present, so
    // the refusal below is caused by its absence and not by the fixture.
    let inventory = inventoried("1eeac9de");
    assert_eq!(
        ProjectRepository::new(db.clone(), djinn_core::events::EventBus::noop())
            .with_legacy_digest_inventory(inventory.clone())
            .resolve_dispatch_image("p1")
            .await
            .unwrap()
            .expect("project resolves")
            .authority_protocol,
        Some(LauncherAuthorityProtocol::LeafV1)
    );

    sqlx::query("DELETE FROM launcher_authority_mode")
        .execute(db.pool())
        .await
        .unwrap();
    assert!(
        LauncherAuthorityModeRepository::new(db.clone())
            .read()
            .await
            .unwrap()
            .is_none()
    );

    let error = ProjectRepository::new(db.clone(), djinn_core::events::EventBus::noop())
        .with_legacy_digest_inventory(inventory)
        .resolve_dispatch_image("p1")
        .await
        .expect_err("an unreadable authority must refuse, never default");
    assert!(
        error.to_string().contains("could not be read"),
        "got: {error}"
    );

    // The same shape, decided purely.
    assert_eq!(
        djinn_db::admit_with_legacy_inventory(
            LauncherProtocolAdmission::AuthorityUnavailable,
            Some(&digest("1eeac9de")),
            &inventoried("1eeac9de"),
        ),
        Err(AdmissionRejection::Declared(
            LauncherProtocolAdmission::AuthorityUnavailable
        ))
    );
}
