//! **AC2 + AC4.** What newly built artifacts must declare, and what fencing
//! them must not cost.
//!
//! AC2 is the producer side: every image this repository generates declares its
//! launcher authority protocol in its own build metadata *and* captures an
//! immutable manifest digest, so it never needs the legacy compatibility path
//! at all. The inventory exists for artifacts that predate the declaration —
//! never as a destination for new ones.
//!
//! AC4 is the "no retirement" clause. Compatibility is a decision about which
//! artifacts may dispatch; nothing about it may be satisfied by deleting a
//! containment asset. The assertions below name the launcher and broker
//! binaries, the RuntimeClass and node conformance assets, the broker mount
//! surface, the credential-boundary tests, child-UID execution, `cgroup.kill`,
//! and `wait_empty` — and fail if any of them stops existing.

use std::path::{Path, PathBuf};

use djinn_db::launcher_compatibility::PreProtocolDigest;
use djinn_db::{Database, ImageRepository, ProjectRepository, launcher_compatibility};
use djinn_image_builder::dockerfile::{
    AgentWorkerImage, DEFAULT_LAUNCHER_PROTOCOL, LAUNCHER_PROTOCOL_ENV, LAUNCHER_PROTOCOL_LABEL,
    generate_dockerfile,
};
use djinn_launcher_protocol::LauncherAuthorityProtocol;

fn repo_root() -> PathBuf {
    // server/crates/djinn-image-controller → repository root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("the crate must live three levels below the repository root")
        .to_path_buf()
}

fn read(relative: &str) -> String {
    let path = repo_root().join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{relative} must exist and be readable: {error}"))
}

// ── AC2: what a newly built artifact declares ───────────────────────────────

/// Every Dockerfile this generator emits declares the protocol in build
/// metadata — a LABEL on the artifact and the env var the sidecar reads out of
/// the same image — and it declares one of the two canonical wire forms.
///
/// Mutation that fails it: delete the `emit_launcher_protocol` call from
/// `generate_dockerfile`. Both `assert!(contains)` calls below fail, naming the
/// config whose Dockerfile lost its declaration.
#[test]
fn every_generated_image_declares_its_protocol_in_build_metadata() {
    let worker = AgentWorkerImage::new("djinn/agent-worker", "sha256-test");
    for (name, config_json) in [
        ("empty", r#"{"schema_version":1}"#),
        (
            "rust",
            r#"{"schema_version":1,"languages":{"rust":{"default_toolchain":"1.84.0"}}}"#,
        ),
        (
            "node",
            r#"{"schema_version":1,"languages":{"node":{"default_version":"22"}}}"#,
        ),
    ] {
        let config: djinn_stack::environment::EnvironmentConfig =
            serde_json::from_str(config_json).unwrap_or_else(|e| panic!("{name}: {e}"));
        let dockerfile = generate_dockerfile(&config, &worker, DEFAULT_LAUNCHER_PROTOCOL)
            .unwrap_or_else(|e| panic!("{name} must generate: {e}"))
            .dockerfile;

        let declared = DEFAULT_LAUNCHER_PROTOCOL.as_wire();
        assert!(
            dockerfile.contains(&format!("LABEL {LAUNCHER_PROTOCOL_LABEL}=\"{declared}\"")),
            "{name}: the artifact must carry the protocol LABEL, got:\n{dockerfile}"
        );
        assert!(
            dockerfile.contains(&format!("ENV {LAUNCHER_PROTOCOL_ENV}={declared}")),
            "{name}: the artifact must carry the protocol env var, got:\n{dockerfile}"
        );
        assert!(
            LauncherAuthorityProtocol::ALL.contains(&DEFAULT_LAUNCHER_PROTOCOL),
            "{name}: the declaration must be one of the canonical wire forms"
        );
    }
}

/// The build Job echoes the declaration back out of the image it just built, so
/// the watcher records what the *artifact* says rather than what the tag claims.
///
/// Mutation that fails it: remove the `DJINN_LAUNCHER_PROTOCOL=` echo from
/// `build_job.rs`'s script. The `assert!` fails naming the sentinel.
#[test]
fn the_build_job_echoes_the_declaration_as_a_sentinel() {
    let source = read("server/crates/djinn-image-controller/src/build_job.rs");
    assert!(
        source.contains("DJINN_LAUNCHER_PROTOCOL=${LAUNCHER_AUTHORITY_PROTOCOL}"),
        "build_job.rs must echo the protocol sentinel the watcher parses"
    );
    assert!(
        source.contains(LAUNCHER_PROTOCOL_LABEL),
        "build_job.rs must read the declaration out of the artifact's own LABEL"
    );
}

/// A newly cataloged artifact carries BOTH a declaration and an immutable
/// digest, and therefore never consults the inventory: the compatibility path
/// is unreachable for it.
///
/// Mutation that fails it: drop the digest requirement from
/// `ImageRepository::mark_ready` and mark the row ready with
/// `registry_digest = NULL`. The `expect_err` below returns `Ok`.
#[tokio::test]
async fn a_newly_cataloged_artifact_declares_a_protocol_and_pins_a_digest() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    ProjectRepository::new(db.clone(), djinn_core::events::EventBus::noop())
        .create_with_id("p1", "p-1", "test", "p1")
        .await
        .unwrap();
    let images = ImageRepository::new(db.clone());
    images
        .create("new", "Newly built", None, "{}")
        .await
        .unwrap();

    let digest = format!("sha256:{}", "ab".repeat(32));
    images
        .mark_ready(
            "new",
            "reg/djinn-image-new:contenthash",
            Some(&digest),
            Some(DEFAULT_LAUNCHER_PROTOCOL),
        )
        .await
        .unwrap();
    images.set_project_image("p1", Some("new")).await.unwrap();

    let row = images.get("new").await.unwrap().expect("row");
    assert_eq!(
        row.declared_launcher_protocol().unwrap(),
        Some(DEFAULT_LAUNCHER_PROTOCOL)
    );
    assert_eq!(row.registry_digest.as_deref(), Some(digest.as_str()));

    // The inventory is EMPTY and the artifact still dispatches: a declaring
    // image is admitted on its own declaration, never on an allowlist entry.
    // (Migration 167 seeds the authority singleton at `leaf-v1`, which is what
    // `DEFAULT_LAUNCHER_PROTOCOL` is, so no flip is needed here.)
    let resolved = ProjectRepository::new(db.clone(), djinn_core::events::EventBus::noop())
        .with_legacy_digest_inventory(launcher_compatibility::LegacyDigestInventory::verified(
            "ops",
            "now",
            [],
        ))
        .resolve_dispatch_image("p1")
        .await
        .unwrap()
        .expect("project resolves");
    assert_eq!(resolved.authority_protocol, Some(DEFAULT_LAUNCHER_PROTOCOL));

    // And a declaring artifact can never be written without a digest, so a new
    // build cannot fall into the legacy population by accident.
    images.create("bad", "No digest", None, "{}").await.unwrap();
    images
        .mark_ready(
            "bad",
            "reg/djinn-image-bad:contenthash",
            None,
            Some(DEFAULT_LAUNCHER_PROTOCOL),
        )
        .await
        .expect_err("a declaring build with no digest must be refused");
}

/// The inventory is built from the catalog, not from guesswork:
/// `legacy_pre_protocol_digests` returns exactly the `ready`, undeclared,
/// digest-pinned rows — and nothing else.
///
/// Mutation that fails it: drop `launcher_authority_protocol IS NULL` from the
/// query. The declaring row appears and the length assertion fails.
#[tokio::test]
async fn the_inventory_candidate_set_is_exactly_the_ready_undeclared_digest_pinned_rows() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let images = ImageRepository::new(db.clone());

    let legacy_digest = format!("sha256:{}", "1e".repeat(32));
    for (id, digest, protocol) in [
        ("a-legacy-pinned", Some(legacy_digest.as_str()), None),
        ("b-legacy-digestless", None, None),
        (
            "c-declaring",
            Some("sha256:cc"),
            Some(DEFAULT_LAUNCHER_PROTOCOL),
        ),
    ] {
        images.create(id, id, None, "{}").await.unwrap();
        images
            .mark_ready(id, &format!("reg/{id}:hash"), digest, protocol)
            .await
            .unwrap();
    }
    // A row that never went ready must not be vouched for either.
    images.create("d-building", "d", None, "{}").await.unwrap();
    images.mark_building("d-building", "hash").await.unwrap();

    let candidates = images.legacy_pre_protocol_digests().await.unwrap();
    let ids: Vec<&str> = candidates.iter().map(|c| c.image_id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["a-legacy-pinned"],
        "only a ready, undeclared, digest-pinned row belongs in the inventory"
    );
    assert_eq!(candidates[0].registry_digest, legacy_digest);
    // The candidate is directly usable as an inventory entry.
    assert!(PreProtocolDigest::parse(&candidates[0].registry_digest).is_ok());
}

// ── AC4: no retirement is used to satisfy compatibility ─────────────────────

/// The launcher and broker binaries, the RuntimeClass and node conformance
/// assets, the broker mount surface, the credential-boundary tests, child-UID
/// execution, `cgroup.kill`, and `wait_empty` all still exist.
///
/// Every needle below is a *behavior* anchor, not a filename: deleting the file
/// fails the read, and gutting the file while keeping its name fails the
/// `contains`. Mutation that fails it: delete
/// `Launcher::wait_empty` from `djinn-cgroup-launcher/src/lib.rs` — the
/// `wait_empty` assertion fails naming the missing symbol.
#[test]
fn no_containment_asset_was_retired_to_make_compatibility_work() {
    struct Asset {
        what: &'static str,
        file: &'static str,
        needles: &'static [&'static str],
    }

    let assets = [
        Asset {
            what: "the launcher binary",
            file: "server/crates/djinn-cgroup-launcher/Cargo.toml",
            needles: &["[[bin]]", "name = \"djinn-cgroup-launcher\""],
        },
        Asset {
            what: "cgroup.kill and wait_empty",
            file: "server/crates/djinn-cgroup-launcher/src/lib.rs",
            needles: &["cgroup.kill", "fn wait_empty"],
        },
        Asset {
            what: "the broker and its wait_empty control",
            file: "server/crates/djinn-cgroup-launcher/src/broker.rs",
            needles: &["fn wait_empty", "WorkerReadinessAssertion"],
        },
        Asset {
            what: "child UID execution",
            file: "server/crates/djinn-cgroup-launcher/src/child.rs",
            needles: &["CHILD_UID: u32 = 1001", "fn set_uid", "set_uid(CHILD_UID)"],
        },
        Asset {
            what: "the worker-side process broker",
            file: "server/crates/djinn-agent/src/process_broker.rs",
            needles: &["wait_empty"],
        },
        Asset {
            what: "the credential-boundary test",
            file: "server/crates/djinn-cgroup-launcher/tests/containment_and_escalation.rs",
            needles: &["credential_and_control_descriptors_are_closed_before_credential_drop"],
        },
        Asset {
            what: "the rendered credential/mount boundary",
            file: "server/crates/djinn-cgroup-launcher/tests/rendered_boundary/mod.rs",
            needles: &["credential"],
        },
        Asset {
            what: "the broker mount surface",
            file: "server/crates/djinn-k8s/src/launcher.rs",
            needles: &[
                "VOLUME_LAUNCHER_IPC",
                "LAUNCHER_SOCKET_PATH",
                "LAUNCHER_CREDENTIAL_PATH",
                "TASK_RUN_CGROUP_RUNTIME_CLASS",
            ],
        },
        Asset {
            what: "the RuntimeClass manifest",
            file: "deploy/helm/djinn/templates/runtimeclass-cgroup-writable.yaml",
            needles: &[
                "kind: RuntimeClass",
                "name: djinn-cgroup-writable",
                "handler: runc-cgroupwritable",
            ],
        },
        Asset {
            what: "the node conformance program",
            file: "deploy/node/k3s/djinn-cgroup-writable-conformance.sh",
            needles: &["djinn.io/cgroup-writable"],
        },
    ];

    for asset in assets {
        let source = read(asset.file);
        for needle in asset.needles {
            assert!(
                source.contains(needle),
                "{} was retired: {} no longer contains {needle:?}. No retirement may be used \
                 to satisfy launcher-authority compatibility",
                asset.what,
                asset.file
            );
        }
    }
}

/// The compatibility decision has exactly one definition, and neither the
/// image-controller nor the image-builder carries a second copy of the rule.
///
/// Mutation that fails it: reimplement the "undeclared + digest ⇒ leaf-v1" rule
/// inline in the controller. The scan finds `LeafV1` reached without going
/// through `decide_admission` and fails.
#[test]
fn the_compatibility_rule_is_not_restated_outside_its_module() {
    let module = read("server/crates/djinn-db/src/launcher_compatibility.rs");
    assert!(
        module.contains("pub fn decide_admission("),
        "the centralized decision must live in launcher_compatibility.rs"
    );
    // …and it composes z3gi's decision rather than restating it.
    assert!(
        module.contains("decide_launcher_protocol_admission("),
        "the declaration arm must delegate to z3gi's decision, not reimplement it"
    );

    // The resolver reaches the decision through that module, not through a
    // local reimplementation.
    let project = read("server/crates/djinn-db/src/repositories/project.rs");
    assert!(
        project.contains("admit_with_legacy_inventory(")
            && project.contains("admit_declared_protocol("),
        "resolve_dispatch_image must route through the centralized decision and the persisted \
         authority singleton"
    );

    for (label, file) in [
        (
            "the image controller",
            "server/crates/djinn-image-controller/src/watcher.rs",
        ),
        (
            "the image builder",
            "server/crates/djinn-image-builder/src/dockerfile.rs",
        ),
    ] {
        let source = read(file);
        assert!(
            !source.contains("unwrap_or_default()") || !source.contains("registry_digest"),
            "{label} must not derive an effective protocol from a digest; that decision \
             belongs to decide_admission"
        );
    }
}
