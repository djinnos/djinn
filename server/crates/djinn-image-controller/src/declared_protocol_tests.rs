//! End-to-end coverage for the **configurable** launcher authority protocol:
//! from the deployment's environment to the catalog row.
//!
//! A sibling file behind the `#[path]` convention `watcher_protocol_tests.rs`
//! already uses, and a child module of `watcher` so the private sentinel parser
//! and the `classify_ready` decision are reachable without widening their
//! visibility for production code.
//!
//! **This file names no Kubernetes type.** The `k8s` capability boundary keeps
//! that vocabulary inside `djinn-k8s`, and the image-controller's exceptions to
//! it are an inventory being shrunk under `epic/fztz` — not a list to append to
//! for a new test's convenience. Everything below is provable without a `Job`:
//! the declaration is decided by `BuildContext::verify_declaration`, the
//! builder's shell is `render_builder_script`, and the two are bound to one
//! spelling by `LAUNCHER_PROTOCOL_JOB_ENV` and `PROTOCOL_SENTINEL`. The single
//! assertion that genuinely needs the rendered Job — that its container env
//! carries the verified declaration — lives in `build_job.rs`'s own test
//! module, inside the file the inventory already covers.
//!
//! Nothing here asserts on a constant. Each step consumes the output of the
//! previous one:
//!
//! ```text
//! DJINN_IMAGE_LAUNCHER_AUTHORITY_PROTOCOL=resize-v2
//!   └─ ImageControllerConfig::from_env
//!        └─ controller::render_build_context      (the composition site)
//!             ├─ Dockerfile LABEL / ENV           → the artifact's metadata
//!             ├─ BuildContext::environment_hash   → the cache key + image tag
//!             └─ BuildContext::verify_declaration → what the build Job reports
//!                  └─ render_builder_script's own echo, expanded
//!                       └─ parse_build_metadata → classify_ready
//!                            └─ ImageRepository::mark_ready → images row
//! ```

use djinn_db::{Database, ImageRepository};
use djinn_image_builder::{
    DEFAULT_LAUNCHER_PROTOCOL, LAUNCHER_PROTOCOL_ENV, LAUNCHER_PROTOCOL_LABEL,
};
use djinn_launcher_protocol::LauncherAuthorityProtocol;
use djinn_stack::environment::EnvironmentConfig;

use super::*;
use crate::build_job::{
    BuildSubject, LAUNCHER_PROTOCOL_JOB_ENV, build_image_build_job, render_builder_script,
};
use crate::config::test_env::with_protocol_env;
use crate::controller::render_build_context;

const WORKER_REF: &str = "registry.example/djinn-agent-runtime:sha256-e2e";

fn image_config() -> EnvironmentConfig {
    let mut cfg = EnvironmentConfig::empty();
    cfg.schema_version = djinn_stack::environment::SCHEMA_VERSION;
    cfg
}

/// A controller config as a deployment that set (or did not set)
/// `DJINN_IMAGE_LAUNCHER_AUTHORITY_PROTOCOL` would boot with. Built through
/// `from_env` rather than by assigning the field, so the env plumbing is part
/// of what every assertion below depends on.
fn deployment(selection: Option<&str>) -> ImageControllerConfig {
    let mut config = with_protocol_env(selection, ImageControllerConfig::from_env);
    config.agent_worker_image = WORKER_REF.to_string();
    config.build_version = "9.9.9".to_string();
    config
}

/// The build Pod's log, produced by evaluating the **builder's own script**
/// against the value the build Job puts in its environment.
///
/// Both halves are real: the `echo` line comes out of `render_builder_script`,
/// and `declared` is what `build_image_build_job` calls `verify_declaration`
/// for. The expansion done here is the one the Pod's shell does. If the script
/// stopped expanding the variable the Job sets — a rename on one side only —
/// the substitution would leave `${…}` in place, the sentinel would fail to
/// parse, and `classify_ready` would refuse.
fn build_log(
    config: &ImageControllerConfig,
    declared: LauncherAuthorityProtocol,
    digest: &str,
) -> String {
    let script = render_builder_script(config, "img-e2e");
    let echo = script
        .lines()
        .find(|line| line.contains(PROTOCOL_SENTINEL))
        .unwrap_or_else(|| panic!("the build script must report the declaration:\n{script}"))
        .trim()
        .to_string();
    let rendered = echo.trim_start_matches("echo ").trim_matches('"').replace(
        &format!("${{{LAUNCHER_PROTOCOL_JOB_ENV}}}"),
        declared.as_wire(),
    );
    format!("#12 exporting to image\n{DIGEST_SENTINEL}{digest}\n{rendered}\n")
}

fn declaration_in(dockerfile: &str, prefix: &str) -> String {
    dockerfile
        .lines()
        .find_map(|line| line.trim_end().strip_prefix(prefix))
        .unwrap_or_else(|| panic!("{prefix} missing from:\n{dockerfile}"))
        .trim_matches('"')
        .to_string()
}

/// **AC1 + AC4.** A deployment configures a protocol; the built artifact
/// declares it in its own OCI metadata, and the catalog row the build produces
/// records exactly what that artifact declares — carried there by the build
/// script's own sentinel, not restated by the test.
///
/// MUTATION (AC1): make `render_build_context` pass
/// `DEFAULT_LAUNCHER_PROTOCOL` instead of `config.declared_launcher_protocol`
/// — i.e. reinstate the hardcoded declaration. The LABEL assertion fails with
/// `leaf-v1`, and so does the catalog assertion, because every step downstream
/// carries what was built.
///
/// MUTATION (AC4): make `verify_declaration` return `Ok(self.launcher_protocol)`
/// unconditionally — the reported protocol stops being the one the artifact
/// carries, and the final assertion (row == the Dockerfile's own LABEL) fails.
#[tokio::test]
async fn a_configured_protocol_reaches_the_artifact_and_the_catalog_row() {
    for protocol in LauncherAuthorityProtocol::ALL {
        let config = deployment(Some(protocol.as_wire()));
        assert_eq!(config.declared_launcher_protocol, protocol);

        // 1. What the artifact itself will carry.
        let context = render_build_context(&config, &image_config()).expect("renders");
        let label = declaration_in(
            &context.dockerfile,
            &format!("LABEL {LAUNCHER_PROTOCOL_LABEL}="),
        );
        let env = declaration_in(
            &context.dockerfile,
            &format!("ENV {LAUNCHER_PROTOCOL_ENV}="),
        );
        assert_eq!(
            label,
            protocol.as_wire(),
            "the configured protocol must reach the artifact's own build metadata"
        );
        assert_eq!(env, label, "the sidecar reads the same declaration");

        // 2. What the build Job reports about it — the checked value, which is
        //    also what its container env carries (asserted against the rendered
        //    Job in `build_job.rs`'s own test module, inside the k8s boundary).
        let declared = context
            .verify_declaration()
            .expect("a freshly rendered context agrees with itself");

        // 3. The build Pod's log, from the builder's own script.
        let digest = format!("sha256:{}", "ab".repeat(32));
        let metadata = parse_build_metadata(&build_log(&config, declared, &digest));
        let ReadyOutcome::Ready {
            digest: seen_digest,
            protocol: reported,
        } = classify_ready("img-e2e", "djinn-build-img-e2e", &metadata)
        else {
            panic!("a declaring build with a digest must be admitted: {metadata:?}");
        };
        assert_eq!(seen_digest.as_deref(), Some(digest.as_str()));

        // 4. What the catalog ends up holding.
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let images = ImageRepository::new(db.clone());
        images.create("img-e2e", "e2e", None, "{}").await.unwrap();
        images
            .mark_ready(
                "img-e2e",
                "reg.example/djinn-image-img-e2e:0123456789ab",
                seen_digest.as_deref(),
                reported,
            )
            .await
            .unwrap();

        let row = images.get("img-e2e").await.unwrap().expect("row");
        assert_eq!(
            row.declared_launcher_protocol().unwrap(),
            Some(protocol),
            "the catalog must record the configured protocol"
        );
        assert_eq!(
            row.launcher_authority_protocol.as_deref(),
            Some(label.as_str()),
            "the catalog row and the artifact's OCI label must be the same string"
        );
    }
}

/// **AC2.** Flipping the deployment's selection invalidates the cache: the
/// environment hash moves, so the image tag moves, so the controller's
/// "hash unchanged and ready — skipping" arm cannot be taken and no cached
/// artifact can be served under the new declaration.
///
/// MUTATION: drop `launcher_protocol` from `compute_environment_hash`. The
/// hashes match, the tags collide, and both assertions fail.
#[test]
fn flipping_the_deployment_selection_invalidates_the_cached_image() {
    let cfg = image_config();
    let leaf = deployment(Some(LauncherAuthorityProtocol::LeafV1.as_wire()));
    let resize = deployment(Some(LauncherAuthorityProtocol::ResizeV2.as_wire()));

    let leaf_hash = render_build_context(&leaf, &cfg).unwrap().environment_hash(
        &cfg,
        &leaf.agent_worker_image,
        &leaf.build_version,
    );
    let resize_hash = render_build_context(&resize, &cfg)
        .unwrap()
        .environment_hash(&cfg, &resize.agent_worker_image, &resize.build_version);

    assert_ne!(
        leaf_hash, resize_hash,
        "a protocol flip must not resolve to the cached image's hash"
    );
    assert_ne!(
        crate::controller::format_catalog_image_tag("reg.example", "img-e2e", &leaf_hash[..12]),
        crate::controller::format_catalog_image_tag("reg.example", "img-e2e", &resize_hash[..12]),
        "the two declarations must not share a content-addressed tag"
    );

    // And an unconfigured deployment keeps the hash it had before the knob
    // existed, so upgrading rebuilds nothing.
    let unset = deployment(None);
    assert_eq!(unset.declared_launcher_protocol, DEFAULT_LAUNCHER_PROTOCOL);
    let unset_hash = render_build_context(&unset, &cfg)
        .unwrap()
        .environment_hash(&cfg, &unset.agent_worker_image, &unset.build_version);
    assert_eq!(
        unset_hash, leaf_hash,
        "configuring nothing must be identical to configuring the default"
    );
}

/// **AC4, forced.** An artifact whose label and catalog row would disagree
/// cannot be built: the call that renders the build Job refuses.
///
/// This is the failure the whole design exists to make impossible — a
/// `leaf-v1` image catalogued as `resize-v2` means the launcher writes leaf
/// `cpu.max` while the plane believes Kubernetes owns quota.
///
/// MUTATION: delete the `verify_declaration()?` call from
/// `build_image_build_job`. It returns `Ok` and this test's `expect` panics.
#[test]
fn a_build_whose_label_and_catalog_row_would_disagree_is_refused() {
    let config = deployment(None);
    let mut context = render_build_context(&config, &image_config()).unwrap();
    // The Dockerfile still declares leaf-v1; only what would be reported to the
    // catalog is changed.
    context.launcher_protocol = LauncherAuthorityProtocol::ResizeV2;

    let error = build_image_build_job(
        &config,
        &BuildSubject::image("img-e2e"),
        "0123456789ab",
        "reg.example/djinn-image-img-e2e:0123456789ab",
        &context,
    )
    .expect_err("a disagreeing context must never become a build");

    assert!(
        matches!(
            error,
            djinn_image_builder::DeclarationError::Disagree {
                dockerfile_says: LauncherAuthorityProtocol::LeafV1,
                context_says: LauncherAuthorityProtocol::ResizeV2,
            }
        ),
        "unexpected refusal: {error}"
    );
}

/// The emitter and the parser of the protocol sentinel share one spelling, and
/// the script expands exactly the variable the build Job sets.
///
/// This is what lets the chain above be proven without a rendered Job: the Job
/// sets `LAUNCHER_PROTOCOL_JOB_ENV`, the script expands
/// `LAUNCHER_PROTOCOL_JOB_ENV`, and the watcher parses `PROTOCOL_SENTINEL` —
/// each spelled once.
///
/// MUTATION: give `render_builder_script` its own literal `${PROTOCOL}` instead
/// of interpolating `LAUNCHER_PROTOCOL_JOB_ENV`. The Pod would print an
/// unexpanded `${PROTOCOL}` that the watcher refuses; this fails first, naming
/// the script.
#[test]
fn the_script_expands_the_variable_the_build_job_sets() {
    let script = render_builder_script(&deployment(None), "img-e2e");
    assert!(
        script.contains(&format!(
            "echo \"{PROTOCOL_SENTINEL}${{{LAUNCHER_PROTOCOL_JOB_ENV}}}\""
        )),
        "the script must echo the sentinel by expanding the Job's own env var:\n{script}"
    );
    // …and nothing derives the declaration from the mutable image name.
    let protocol_line = script
        .lines()
        .find(|line| line.contains(PROTOCOL_SENTINEL))
        .expect("sentinel line");
    assert!(
        !protocol_line.contains("IMAGE_TAG"),
        "the declaration must never come from the tag: {protocol_line}"
    );
}
