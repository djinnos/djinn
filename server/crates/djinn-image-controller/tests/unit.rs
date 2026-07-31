//! Unit coverage for the image-controller crate.
//!
//! The cluster-backed path (`ImageController::enqueue` against a live
//! apiserver) is out of scope for this PR — it belongs to a follow-up
//! kind_smoke test. Instead these tests exercise the pure-function
//! building blocks: the Job manifest builder (labels, envs, volumes)
//! and the image-build watcher's state transitions.

use std::collections::BTreeMap;

use djinn_image_builder::{AgentWorkerImage, BuildContext, generate_dockerfile};
use djinn_image_controller::ImageControllerConfig;
use djinn_image_controller::build_job::{
    BuildSubject, COMPONENT_IMAGE_BUILD, LABEL_BUILD, LABEL_COMPONENT, LABEL_IMAGE_HASH,
    LABEL_PROJECT_ID, build_image_build_job,
};
use djinn_image_controller::controller::format_image_tag;
use djinn_image_controller::watcher::__test_handle_event;
use djinn_stack::environment::EnvironmentConfig;

fn test_build_context() -> BuildContext {
    let mut cfg = EnvironmentConfig::empty();
    cfg.schema_version = djinn_stack::environment::SCHEMA_VERSION;
    generate_dockerfile(&cfg, &AgentWorkerImage::new("djinn/agent-runtime", "dev"))
        .expect("generate")
}

#[test]
fn build_job_labels_and_envs_match_plan() {
    let cfg = ImageControllerConfig::for_testing();
    let tag = format_image_tag(&cfg.registry_host, "proj-abc", "1a2b3c4d5e6f");
    let ctx = test_build_context();
    let job = build_image_build_job(
        &cfg,
        &BuildSubject::project("proj-abc"),
        "1a2b3c4d5e6f",
        &tag,
        &ctx,
    );

    let labels = job.metadata.labels.as_ref().expect("labels present");
    assert_eq!(labels.get(LABEL_COMPONENT).unwrap(), COMPONENT_IMAGE_BUILD);
    assert_eq!(labels.get(LABEL_BUILD).unwrap(), "true");
    assert_eq!(labels.get(LABEL_PROJECT_ID).unwrap(), "proj-abc");
    assert_eq!(labels.get(LABEL_IMAGE_HASH).unwrap(), "1a2b3c4d5e6f");

    let template_labels = job
        .spec
        .as_ref()
        .unwrap()
        .template
        .metadata
        .as_ref()
        .and_then(|m| m.labels.as_ref())
        .expect("template labels");
    assert_eq!(template_labels.get(LABEL_PROJECT_ID).unwrap(), "proj-abc");

    let pod = job.spec.as_ref().unwrap().template.spec.as_ref().unwrap();
    assert_eq!(pod.containers.len(), 1);
    let env: BTreeMap<&str, &str> = pod.containers[0]
        .env
        .as_ref()
        .unwrap()
        .iter()
        .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
        .collect();
    // Post-P5: the builder Pod talks to buildkitd via BUILDCTL_ADDR and
    // publishes to IMAGE_TAG. DOCKER_HOST is gone — `docker buildx` is not
    // used anymore.
    assert_eq!(env.get("IMAGE_TAG").copied(), Some(tag.as_str()));
    assert_eq!(
        env.get("BUILDCTL_ADDR").copied(),
        Some(cfg.buildkitd_host.as_str())
    );
    assert!(
        !env.contains_key("DOCKER_HOST"),
        "DOCKER_HOST must be absent — buildctl talks to buildkitd directly"
    );

    // Volumes: build-context ConfigMap, writable docker-config emptyDir,
    // registry-auth Secret. No workspace emptyDir (no clone), no
    // build-token Secret (no GitHub auth needed).
    let volumes = pod.volumes.as_ref().unwrap();
    assert!(
        volumes.iter().any(|v| v.config_map.is_some()),
        "build-context must be a ConfigMap volume"
    );
    assert!(
        volumes.iter().any(|v| v.secret.is_some()),
        "registry-auth must be a Secret volume"
    );
    assert!(
        !volumes.iter().any(|v| v.name == "workspace"),
        "no workspace emptyDir — the Dockerfile generator is self-contained"
    );
}

/// An image build is an UPSTREAM DEPENDENCY of a task-run, so it must stay out
/// of the shared ClusterQueue: a task-run holding a slot while the image build
/// it is waiting on is queued behind it is a priority-inversion deadlock the
/// current ledger cannot produce. Kueue cutover S1 therefore arms task-run, warm
/// and SCIP only.
///
/// Parameterized over every combination of the two chart flags (`kueue.enabled`
/// / `kueue.armed`, reaching this process only as `DJINN_KUEUE_*` env vars,
/// since `ImageControllerConfig` deliberately has no Kueue field). Asserting the
/// one default combination would pass even if `build_job.rs` started calling the
/// shared `djinn_k8s` label helper.
#[test]
fn image_build_job_does_not_opt_into_kueue_build_object_admission() {
    const KUEUE_BUILD_OBJECT_LABEL: &str = "djinn.io/kueue-build-object";
    const KUEUE_QUEUE_NAME_LABEL: &str = "kueue.x-k8s.io/queue-name";
    const ARMED: &str = "DJINN_KUEUE_ARMED";
    const ENABLED: &str = "DJINN_KUEUE_ENABLED";

    let restore_armed = std::env::var(ARMED).ok();
    let restore_enabled = std::env::var(ENABLED).ok();

    let context = test_build_context();
    for enabled in ["false", "true"] {
        for armed in ["false", "true"] {
            // SAFETY: single-threaded within this test; no other test in this
            // binary reads DJINN_KUEUE_*, and the pre-existing values are
            // restored below.
            unsafe {
                std::env::set_var(ENABLED, enabled);
                std::env::set_var(ARMED, armed);
            }

            // `from_env`, not `for_testing`: a config constructor that ignored
            // the environment would make the parameterization vacuous.
            let config = ImageControllerConfig::from_env();
            let job = build_image_build_job(
                &config,
                &BuildSubject::project("proj-kueue-contract"),
                "1a2b3c4d5e6f",
                "registry.example/djinn-project-proj-kueue-contract:1a2b3c4d5e6f",
                &context,
            );
            let flags = format!("kueue.enabled={enabled} kueue.armed={armed}");

            for (location, labels) in [
                ("Job metadata", job.metadata.labels.as_ref()),
                (
                    "Pod-template metadata",
                    job.spec
                        .as_ref()
                        .and_then(|spec| spec.template.metadata.as_ref())
                        .and_then(|metadata| metadata.labels.as_ref()),
                ),
            ] {
                let labels: &BTreeMap<String, String> =
                    labels.expect("image-build Job renders labels at both locations");
                assert!(
                    !labels.contains_key(KUEUE_BUILD_OBJECT_LABEL),
                    "image-build {location} must remain outside Kueue build-object admission at {flags}",
                );
                assert!(
                    !labels.contains_key(KUEUE_QUEUE_NAME_LABEL),
                    "image-build {location} must name no LocalQueue at {flags}",
                );
            }

            // A suspended Job that no ClusterQueue will ever admit hangs forever.
            assert_eq!(
                job.spec.as_ref().and_then(|spec| spec.suspend),
                None,
                "image-build Job must never be created suspended at {flags}",
            );
        }
    }

    // SAFETY: see above; restores the ambient environment for the rest of the run.
    unsafe {
        match restore_armed {
            Some(value) => std::env::set_var(ARMED, value),
            None => std::env::remove_var(ARMED),
        }
        match restore_enabled {
            Some(value) => std::env::set_var(ENABLED, value),
            None => std::env::remove_var(ENABLED),
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 3 PR 5.5: image-build Job watcher transition coverage.
// ---------------------------------------------------------------------------

mod watcher_transitions {
    use std::collections::{BTreeMap, HashSet};
    use std::sync::{Arc, Mutex};

    use djinn_core::events::{DjinnEventEnvelope, EventBus};
    use djinn_db::{Database, ProjectImage, ProjectImageStatus, ProjectRepository};
    use k8s_openapi::api::batch::v1::{Job, JobStatus};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
    use kube::runtime::watcher;

    use djinn_image_controller::ImageControllerConfig;
    use djinn_image_controller::build_job::{LABEL_BUILD, LABEL_IMAGE_HASH, LABEL_PROJECT_ID};

    use super::__test_handle_event;

    fn capturing_bus() -> (EventBus, Arc<Mutex<Vec<DjinnEventEnvelope>>>) {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let bus = EventBus::new({
            let captured = captured.clone();
            move |ev| captured.lock().unwrap().push(ev)
        });
        (bus, captured)
    }

    fn fake_job(
        name: &str,
        project_id: &str,
        hash_prefix: &str,
        succeeded: Option<i32>,
        failed: Option<i32>,
    ) -> Job {
        let mut labels = BTreeMap::new();
        labels.insert(LABEL_BUILD.into(), "true".into());
        labels.insert(LABEL_PROJECT_ID.into(), project_id.into());
        labels.insert(LABEL_IMAGE_HASH.into(), hash_prefix.into());
        Job {
            metadata: ObjectMeta {
                name: Some(name.into()),
                labels: Some(labels),
                ..ObjectMeta::default()
            },
            status: Some(JobStatus {
                succeeded,
                failed,
                ..JobStatus::default()
            }),
            ..Job::default()
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn succeeded_event_flips_status_to_ready_and_emits_event() {
        let cfg = ImageControllerConfig::for_testing();
        let db = Database::open_in_memory().expect("open_in_memory");
        let repo_seed = ProjectRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let project = repo_seed
            .create("demo", "test", "demo")
            .await
            .expect("create project");

        let full_hash = "1a2b3c4d5e6f7081928374655e4d3c2b1a0f1e2d";
        repo_seed
            .set_project_image(
                &project.id,
                &ProjectImage {
                    tag: None,
                    hash: Some(full_hash.to_string()),
                    status: ProjectImageStatus::BUILDING.to_string(),
                    last_error: None,
                },
            )
            .await
            .expect("seed building row");

        let (bus, captured) = capturing_bus();
        let mut seen = HashSet::new();
        let hash_prefix = &full_hash[..12];

        let job = fake_job(
            &format!("djinn-build-{}-{hash_prefix}", project.id),
            &project.id,
            hash_prefix,
            Some(1),
            None,
        );

        __test_handle_event(
            &cfg,
            &db,
            &bus,
            &mut seen,
            watcher::Event::Apply(job.clone()),
        )
        .await;

        let row = repo_seed
            .get_project_image(&project.id)
            .await
            .expect("get_project_image")
            .expect("row present");
        assert_eq!(row.status, ProjectImageStatus::READY);
        assert_eq!(row.hash.as_deref(), Some(full_hash));
        assert_eq!(row.last_error, None);
        let tag = row.tag.as_deref().expect("tag present");
        assert!(
            tag.ends_with(&format!(":{hash_prefix}")),
            "tag {tag} should end with hash prefix"
        );
        assert!(
            tag.contains(&format!("djinn-project-{}", project.id)),
            "tag {tag} should reference the project slug"
        );

        {
            let events = captured.lock().unwrap();
            assert_eq!(events.len(), 1, "one envelope expected");
            assert_eq!(events[0].entity_type, "project_image");
            assert_eq!(events[0].action, "ready");
            assert_eq!(events[0].project_id.as_deref(), Some(project.id.as_str()));
            assert_eq!(
                events[0].payload.get("image_hash").and_then(|v| v.as_str()),
                Some(hash_prefix)
            );
        }

        __test_handle_event(&cfg, &db, &bus, &mut seen, watcher::Event::Apply(job)).await;
        assert_eq!(
            captured.lock().unwrap().len(),
            1,
            "dedupe set must suppress the repeat Apply"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_event_flips_status_and_records_last_error() {
        let cfg = ImageControllerConfig::for_testing();
        let db = Database::open_in_memory().expect("open_in_memory");
        let repo_seed = ProjectRepository::new(db.clone(), EventBus::noop());
        let project = repo_seed
            .create("demo-fail", "test", "demo-fail")
            .await
            .expect("create project");
        repo_seed
            .set_project_image(
                &project.id,
                &ProjectImage {
                    tag: None,
                    hash: Some("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string()),
                    status: ProjectImageStatus::BUILDING.to_string(),
                    last_error: None,
                },
            )
            .await
            .expect("seed building row");

        let (bus, captured) = capturing_bus();
        let mut seen = HashSet::new();
        let job_name = format!("djinn-build-{}-deadbeefdead", project.id);
        let job = fake_job(&job_name, &project.id, "deadbeefdead", None, Some(1));

        __test_handle_event(&cfg, &db, &bus, &mut seen, watcher::Event::InitApply(job)).await;

        let row = repo_seed
            .get_project_image(&project.id)
            .await
            .expect("get_project_image")
            .expect("row present");
        assert_eq!(row.status, ProjectImageStatus::FAILED);
        let err = row.last_error.as_deref().expect("last_error populated");
        assert!(
            err.contains(&job_name),
            "error '{err}' should cite job name"
        );
        assert!(
            err.contains("kubectl logs"),
            "error '{err}' should hint logs"
        );

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].action, "build_failed");
        assert_eq!(
            events[0]
                .payload
                .get("last_error")
                .and_then(|v| v.as_str())
                .unwrap_or_default(),
            err
        );
    }
}
