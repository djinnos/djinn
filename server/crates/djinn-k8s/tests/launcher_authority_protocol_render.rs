//! The catalog protocol reaches the pod, or no pod is created (task fi2k).
//!
//! #2823 made `resize-v2` reachable from the shipped launcher binary by reading
//! `DJINN_LAUNCHER_AUTHORITY_PROTOCOL` in its `main.rs`. Nothing emitted that
//! variable, so every production launcher fell through to `leaf-v1` regardless
//! of what the image declared — a merged, tested feature that was inert in
//! production. These tests assert the two halves of the connection:
//!
//! * **AC1** — the rendered Job carries the resolved image's protocol on the
//!   `cgroup-launcher` init container (and on the worker, which asserts the same
//!   value to the broker at READY).
//! * **AC4** — an image that declares no protocol AND carries no immutable
//!   digest is refused before any Kubernetes object is POSTed.
//!
//! AC4 runs through the REAL `KubernetesRuntime::prepare` against a recording
//! fake apiserver, because "fails closed" is a claim about the number of Jobs
//! created, not about a function returning `Err`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use djinn_cgroup_launcher::LauncherAuthorityProtocol;
use djinn_core::events::EventBus;
use djinn_core::models::TaskRunTrigger;
use djinn_db::Database;
use djinn_db::repositories::image::ImageRepository;
use djinn_db::repositories::project::ProjectRepository;
use djinn_k8s::config::KubernetesConfig;
use djinn_k8s::launcher::{
    AUTHORITY_PROTOCOL_ENV, CgroupLauncherMode, LAUNCHER_CONTAINER_NAME, RenderValidationError,
    apply_launcher_authority_protocol, render_authority_protocol,
};
use djinn_k8s::runtime::KubernetesRuntime;
use djinn_runtime::spec::{SupervisorFlow, TaskRunSpec};
use djinn_runtime::{ResolvedCredentials, SessionRuntime};
use djinn_supervisor::ConnectionRegistry;
use k8s_openapi::api::batch::v1::Job;

/// A canonical immutable manifest digest: `sha256:` + 64 lowercase hex.
///
/// The dispatch-path fixtures below must use a well-formed digest because
/// `resolve_dispatch_image` now runs task vf7a's admission compatibility fence,
/// which compares digests exactly. `render_authority_protocol`'s own unit
/// assertions are unaffected — it takes the digest as an opaque presence check.
const CANONICAL_DIGEST: &str =
    "sha256:7822b7de0000000000000000000000000000000000000000000000000000cafe";

/// Read one container's environment value, if present.
fn env_of(job: &Job, container: &str, name: &str) -> Option<String> {
    let pod = job.spec.as_ref()?.template.spec.as_ref()?;
    let all = pod
        .init_containers
        .iter()
        .flatten()
        .chain(pod.containers.iter());
    all.filter(|candidate| candidate.name == container)
        .flat_map(|candidate| candidate.env.iter().flatten())
        .find(|entry| entry.name == name)
        .and_then(|entry| entry.value.clone())
}

fn task_run_job(config: &KubernetesConfig) -> Job {
    djinn_k8s::job::build_task_run_job(
        config,
        &uuid::Uuid::nil(),
        "project-1",
        "djinn-task-run-secret",
        "registry.example/proj:tag",
        &[],
        None,
        false,
        None,
    )
}

/// AC1. A `resize-v2` catalog image renders `resize-v2`; a `leaf-v1` image
/// renders `leaf-v1`. Both land on `spec.initContainers[name=cgroup-launcher]`.
///
/// Non-vacuity, as stated by the AC: hardcode `leaf-v1` in
/// `apply_launcher_authority_protocol` (or in `render_authority_protocol`) and
/// the `resize-v2` case below fails, naming the value it actually rendered.
#[test]
fn the_rendered_launcher_sidecar_carries_the_images_declared_protocol() {
    let config = KubernetesConfig::for_testing();
    assert!(
        config.cgroup_launcher_mode.renders_sidecar(),
        "this test is about the armed render"
    );

    for protocol in LauncherAuthorityProtocol::ALL {
        // A declaring image is digest-pinned (migration 166), and the render
        // resolves the declaration it made.
        let resolved = render_authority_protocol(Some(protocol), Some("sha256:abc"))
            .expect("a declared protocol resolves");
        assert_eq!(resolved, protocol);

        let mut job = task_run_job(&config);
        assert_eq!(
            env_of(&job, LAUNCHER_CONTAINER_NAME, AUTHORITY_PROTOCOL_ENV),
            None,
            "the base render must not already carry a protocol, or this test would \
             pass without the propagation"
        );

        apply_launcher_authority_protocol(&mut job, config.cgroup_launcher_mode, resolved)
            .expect("the armed render has a launcher sidecar");

        assert_eq!(
            env_of(&job, LAUNCHER_CONTAINER_NAME, AUTHORITY_PROTOCOL_ENV).as_deref(),
            Some(protocol.as_wire()),
            "the {protocol} sidecar must be told it is running {protocol}"
        );
        // The worker asserts the same value to the broker at READY; if only the
        // sidecar were told, every resize-v2 pod would fail its own handshake.
        assert_eq!(
            env_of(&job, "worker", AUTHORITY_PROTOCOL_ENV).as_deref(),
            Some(protocol.as_wire()),
            "the worker must assert the same protocol the launcher runs"
        );

        // It is a container environment ENTRY, exactly once — a second,
        // shadowed definition would be a coin flip at container start.
        let pod = job.spec.as_ref().unwrap().template.spec.as_ref().unwrap();
        for container in pod
            .init_containers
            .iter()
            .flatten()
            .chain(pod.containers.iter())
        {
            let occurrences = container
                .env
                .iter()
                .flatten()
                .filter(|entry| entry.name == AUTHORITY_PROTOCOL_ENV)
                .count();
            assert!(
                occurrences <= 1,
                "{} carries {occurrences} copies of {AUTHORITY_PROTOCOL_ENV}",
                container.name
            );
        }

        // Re-applying is idempotent, not additive.
        apply_launcher_authority_protocol(&mut job, config.cgroup_launcher_mode, resolved)
            .expect("re-apply");
        assert_eq!(
            env_of(&job, LAUNCHER_CONTAINER_NAME, AUTHORITY_PROTOCOL_ENV).as_deref(),
            Some(protocol.as_wire())
        );
    }
}

/// An undeclared but digest-pinned image — every image built before the
/// declaration existed — renders `leaf-v1`, the behavior it has always had.
/// An image with neither is refused.
#[test]
fn an_undeclared_image_renders_leaf_v1_only_when_it_is_digest_pinned() {
    assert_eq!(
        render_authority_protocol(None, Some("sha256:abc")).unwrap(),
        LauncherAuthorityProtocol::LeafV1
    );
    for absent in [None, Some(""), Some("   ")] {
        let refusal = render_authority_protocol(None, absent)
            .expect_err("an image with no declaration and no digest must be refused");
        assert!(matches!(
            refusal,
            RenderValidationError::UndeclarableAuthorityProtocol
        ));
    }
    // A declaration still wins over a missing digest: migration 166 makes that
    // row impossible, and if it ever existed the declaration is the stronger
    // statement.
    assert_eq!(
        render_authority_protocol(Some(LauncherAuthorityProtocol::ResizeV2), None).unwrap(),
        LauncherAuthorityProtocol::ResizeV2
    );
}

/// A disabled launcher renders no sidecar, so there is no protocol to agree
/// about and nothing to write.
#[test]
fn a_disabled_launcher_render_is_left_alone() {
    let config = KubernetesConfig::for_testing();
    let mut job = task_run_job(&config);
    apply_launcher_authority_protocol(
        &mut job,
        CgroupLauncherMode::Disabled,
        LauncherAuthorityProtocol::ResizeV2,
    )
    .expect("a disabled render has nothing to configure");
    assert_eq!(
        env_of(&job, LAUNCHER_CONTAINER_NAME, AUTHORITY_PROTOCOL_ENV),
        None
    );

    // But an ARMED render whose sidecar is missing is an error, never a
    // silent skip: the launcher would boot with no selection at all.
    let mut sidecarless = task_run_job(&config);
    sidecarless
        .spec
        .as_mut()
        .unwrap()
        .template
        .spec
        .as_mut()
        .unwrap()
        .init_containers = None;
    let refusal = apply_launcher_authority_protocol(
        &mut sidecarless,
        CgroupLauncherMode::Required,
        LauncherAuthorityProtocol::ResizeV2,
    )
    .expect_err("an armed render with no launcher container must be refused");
    assert!(matches!(
        refusal,
        RenderValidationError::MissingLauncherSidecar { .. }
    ));
}

/// AC4. An image with neither a declaration nor a digest produces **zero**
/// Kubernetes objects: the refusal happens inside `prepare`, before the Secret
/// POST and long before the Job POST.
///
/// Non-vacuity: make `render_authority_protocol` fall through to
/// `Ok(LauncherAuthorityProtocol::LeafV1)` when both inputs are absent and this
/// test fails on the `POST:` assertion, listing the objects that were created.
#[tokio::test]
async fn a_protocol_less_undigested_image_creates_zero_kubernetes_objects() {
    // Sanity: the SAME flow with a digest-pinned image reaches the POSTs, so
    // the zero below is caused by the refusal and not by an inert harness.
    //
    // The digest is canonical (`sha256:` + 64 lowercase hex): task vf7a's
    // admission compatibility fence compares digests exactly, so a placeholder
    // like `sha256:abc` is refused as a non-digest before the render is reached.
    let (created, digest_pinned) = prepare_with_image(
        Some(CANONICAL_DIGEST),
        None,
        LauncherAuthorityProtocol::LeafV1,
    )
    .await;
    assert!(
        digest_pinned.is_ok(),
        "a digest-pinned image must still dispatch: {digest_pinned:?}"
    );
    assert!(
        created.iter().any(|event| event == "POST:Job"),
        "the harness must be able to reach the Job POST, got {created:?}"
    );

    let (created, refused) =
        prepare_with_image(None, None, LauncherAuthorityProtocol::LeafV1).await;
    // The count first: this is the claim. A render that falls through to a
    // default is caught HERE, naming the objects it created.
    assert!(
        created.is_empty(),
        "an undeclarable image must create NOTHING, got {created:?}"
    );
    let error = refused.expect_err("an undeclarable image must not dispatch");
    let rendered = error.to_string();
    assert!(
        rendered.contains("registry digest") || rendered.contains("launcher authority protocol"),
        "the refusal must name what is missing, got: {rendered}"
    );

    // And a declaring image dispatches, carrying its declaration.
    let (created, declared) = prepare_with_image(
        Some(CANONICAL_DIGEST),
        Some(LauncherAuthorityProtocol::ResizeV2),
        LauncherAuthorityProtocol::ResizeV2,
    )
    .await;
    assert!(
        declared.is_ok(),
        "a declaring image dispatches: {declared:?}"
    );
    assert!(created.iter().any(|event| event == "POST:Job"));
}

/// Drive `KubernetesRuntime::prepare` against a recording fake apiserver with a
/// seeded catalog image, returning every POST it made and the outcome.
async fn prepare_with_image(
    digest: Option<&str>,
    protocol: Option<LauncherAuthorityProtocol>,
    authority: LauncherAuthorityProtocol,
) -> (Vec<String>, Result<(), String>) {
    use http::Response;
    use kube::client::Body;
    use tower::service_fn;

    let created: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let events = created.clone();
    let client = kube::Client::new(
        service_fn(move |request: http::Request<Body>| {
            let events = events.clone();
            async move {
                let path = request.uri().path().to_string();
                let (event, body) = if path.contains("/secrets") {
                    (
                        "POST:Secret",
                        serde_json::json!({
                            "apiVersion": "v1", "kind": "Secret",
                            "metadata": {"name": "task-secret"}
                        }),
                    )
                } else {
                    (
                        "POST:Job",
                        serde_json::json!({
                            "apiVersion": "batch/v1", "kind": "Job",
                            "metadata": {"name": "task-job", "uid": "job-uid"}
                        }),
                    )
                };
                if request.method() == http::Method::POST {
                    events.lock().unwrap().push(event.into());
                }
                Ok::<_, std::io::Error>(
                    Response::builder()
                        .status(200)
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string().into_bytes()))
                        .unwrap(),
                )
            }
        }),
        "djinn",
    );

    let db = Database::open_in_memory().expect("in-memory database");
    db.ensure_initialized().await.expect("migrations");
    if authority == LauncherAuthorityProtocol::LeafV1 {
        djinn_db::test_support::seed_legacy_launcher_authority_for_test(&db).await;
    }
    // Bounded to `projects.id`'s varchar(36); the digest is only summarized.
    let project_id = format!(
        "proj-{}-{}",
        if digest.is_some() {
            "digested"
        } else {
            "nodigest"
        },
        protocol.map(|p| p.as_wire()).unwrap_or("undeclared")
    );
    ProjectRepository::new(db.clone(), EventBus::noop())
        .create_with_id(&project_id, &project_id, "test", &project_id)
        .await
        .expect("seed project");
    let images = ImageRepository::new(db.clone());
    let image_id = format!("img-{project_id}");
    images
        .create(&image_id, &image_id, None, "{}")
        .await
        .expect("seed image");
    images
        .mark_ready(
            &image_id,
            "registry.example/djinn-image-test:hash",
            digest,
            protocol,
        )
        .await
        .expect("mark the seeded image ready");
    images
        .set_project_image(&project_id, Some(&image_id))
        .await
        .expect("select the catalog image");

    // `resolve_dispatch_image` now admits against the DURABLE authority
    // singleton (migration 167) before `prepare` renders anything (task vf7a),
    // so a declaring fixture only dispatches when the server actually runs the
    // protocol it declares. Align the singleton with the fixture's declaration;
    // the mismatch case has its own tests in `djinn-db`.
    if let Some(protocol) = protocol {
        let modes = djinn_db::LauncherAuthorityModeRepository::new(db.clone());
        let epoch = modes
            .read()
            .await
            .expect("read the authority singleton")
            .expect("migration 167 seeds it")
            .epoch;
        modes.set_mode(epoch, protocol).await;
    }

    let runtime = KubernetesRuntime::from_client_with_db(
        client,
        KubernetesConfig::for_testing(),
        Arc::new(ConnectionRegistry::new()),
        db,
    );
    let spec = TaskRunSpec {
        task_run_id: uuid::Uuid::now_v7().to_string(),
        task_attempt_id: None,
        task_id: "task-protocol-render".into(),
        project_id: project_id.clone(),
        trigger: TaskRunTrigger::NewTask,
        base_branch: "main".into(),
        task_branch: "task/protocol-render".into(),
        flow: SupervisorFlow::NewTask,
        model_id_per_role: HashMap::new(),
        read_source_project_ids: Vec::new(),
        knowledge_injection: djinn_core::models::KnowledgeInjectionConfig::default(),
        github_owner: None,
        github_install_token: None,
        commit_author_name: None,
        commit_author_email: None,
        resume_lifecycle_metadata: None,
        is_evidence_spike: false,
    };
    let outcome = runtime
        .prepare(&spec, &ResolvedCredentials::default())
        .await
        .map(|_| ())
        .map_err(|error| error.to_string());
    let observed = created.lock().unwrap().clone();
    (observed, outcome)
}
