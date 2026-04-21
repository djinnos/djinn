//! Unit coverage for the image-controller crate.
//!
//! The cluster-backed path (`ImageController::enqueue` against a live
//! apiserver) is out of scope for this PR — it belongs to a follow-up
//! kind_smoke test. Instead these tests exercise the pure-function
//! building blocks: the Job manifest builder (labels, envs, volumes)
//! and the devcontainer hash (deterministic, flips on file change).

use std::fs;
use std::path::Path;

use djinn_image_controller::build_job::{
    COMPONENT_IMAGE_BUILD, LABEL_BUILD, LABEL_COMPONENT, LABEL_IMAGE_HASH, LABEL_PROJECT_ID,
    build_image_build_job,
};
use djinn_image_controller::controller::format_image_tag;
use djinn_image_controller::hash::{
    DEVCONTAINER_LOCK_PATH, DEVCONTAINER_PATH, compute_devcontainer_hash,
};
use djinn_image_controller::ImageControllerConfig;
use djinn_image_controller::watcher::__test_handle_event;

/// Init a bare git mirror containing `files` at `HEAD`. Returns the
/// TempDir (drop = cleanup) and the absolute path to the mirror.
fn bare_mirror_with(files: &[(&str, &[u8])]) -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let repo = git2::Repository::init(work.path()).unwrap();
    {
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "tester").unwrap();
        cfg.set_str("user.email", "tester@example.com").unwrap();
    }
    for (rel, bytes) in files {
        let full = work.path().join(rel);
        if let Some(p) = full.parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(&full, bytes).unwrap();
    }
    let mut index = repo.index().unwrap();
    for (rel, _) in files {
        index.add_path(Path::new(rel)).unwrap();
    }
    index.write().unwrap();
    let oid = index.write_tree().unwrap();
    let tree = repo.find_tree(oid).unwrap();
    let sig = repo.signature().unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "seed", &tree, &[])
        .unwrap();

    let bare = tmp.path().join("mirror.git");
    git2::build::RepoBuilder::new()
        .bare(true)
        .clone(&format!("file://{}", work.path().display()), &bare)
        .unwrap();
    (tmp, bare)
}

#[test]
fn hash_is_deterministic_for_same_contents() {
    let (_keep_a, mirror_a) = bare_mirror_with(&[(DEVCONTAINER_PATH, b"{\"name\":\"demo\"}")]);
    let (_keep_b, mirror_b) = bare_mirror_with(&[(DEVCONTAINER_PATH, b"{\"name\":\"demo\"}")]);
    let a = compute_devcontainer_hash(&mirror_a).unwrap().unwrap();
    let b = compute_devcontainer_hash(&mirror_b).unwrap().unwrap();
    assert_eq!(
        a, b,
        "identical devcontainer.json content must hash the same across mirrors"
    );
}

#[test]
fn hash_flips_when_lockfile_is_added() {
    let (_a, ma) = bare_mirror_with(&[(DEVCONTAINER_PATH, b"{}")]);
    let (_b, mb) = bare_mirror_with(&[
        (DEVCONTAINER_PATH, b"{}"),
        (DEVCONTAINER_LOCK_PATH, b"{}"),
    ]);
    let a = compute_devcontainer_hash(&ma).unwrap().unwrap();
    let b = compute_devcontainer_hash(&mb).unwrap().unwrap();
    assert_ne!(a, b, "adding the lockfile must move the hash");
}

#[test]
fn missing_devcontainer_is_none() {
    let (_keep, bare) = bare_mirror_with(&[("README.md", b"hi")]);
    assert!(compute_devcontainer_hash(&bare).unwrap().is_none());
}

#[test]
fn build_job_labels_and_envs_match_plan() {
    let cfg = ImageControllerConfig::for_testing();
    let tag = format_image_tag(&cfg.registry_host, "proj-abc", "1a2b3c4d5e6f");
    let job = build_image_build_job(&cfg, "proj-abc", "1a2b3c4d5e6f", &tag);

    // Labels carry the correlators the future reconcile loop relies on.
    let labels = job.metadata.labels.as_ref().expect("labels present");
    assert_eq!(labels.get(LABEL_COMPONENT).unwrap(), COMPONENT_IMAGE_BUILD);
    assert_eq!(labels.get(LABEL_BUILD).unwrap(), "true");
    assert_eq!(labels.get(LABEL_PROJECT_ID).unwrap(), "proj-abc");
    assert_eq!(labels.get(LABEL_IMAGE_HASH).unwrap(), "1a2b3c4d5e6f");

    // Pod template mirrors the same labels (so watching by label picks
    // up the Pod, not just the Job).
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

    // The container env exposes IMAGE_TAG + DOCKER_HOST — the builder
    // script pivots entirely on these.
    let pod = job
        .spec
        .as_ref()
        .unwrap()
        .template
        .spec
        .as_ref()
        .unwrap();
    assert_eq!(pod.containers.len(), 1);
    let env: std::collections::BTreeMap<&str, &str> = pod.containers[0]
        .env
        .as_ref()
        .unwrap()
        .iter()
        .map(|e| (e.name.as_str(), e.value.as_deref().unwrap_or_default()))
        .collect();
    assert_eq!(env.get("IMAGE_TAG").copied(), Some(tag.as_str()));
    assert_eq!(
        env.get("DOCKER_HOST").copied(),
        Some(cfg.buildkitd_host.as_str())
    );

    // Volumes carry a writable workspace (emptyDir) plus the registry-auth
    // and build-token Secrets. The mirror PVC was dropped in 89f054af2
    // (build Job now clones upstream directly) so no PVC is expected.
    let volumes = pod.volumes.as_ref().unwrap();
    assert!(volumes.iter().any(|v| v.empty_dir.is_some()));
    assert!(volumes.iter().any(|v| v.secret.is_some()));
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

    use djinn_image_controller::build_job::{LABEL_BUILD, LABEL_IMAGE_HASH, LABEL_PROJECT_ID};
    use djinn_image_controller::ImageControllerConfig;

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

    /// `succeeded=1` flips `projects.image_status` to `ready`, populates
    /// `image_tag` from the registry + hash-prefix, and emits a
    /// `project_image/ready` envelope on the bus.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn succeeded_event_flips_status_to_ready_and_emits_event() {
        let cfg = ImageControllerConfig::for_testing();
        let db = Database::open_in_memory().expect("open_in_memory");
        let repo_seed =
            ProjectRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let project = repo_seed
            .create("demo", "/tmp/demo")
            .await
            .expect("create project");

        // Simulate the pre-flight write the controller does on enqueue:
        // status=building + the full hash stashed. The watcher must pick
        // up the full hash from here when writing Ready.
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

        __test_handle_event(&cfg, &db, &bus, &mut seen, watcher::Event::Apply(job.clone()))
            .await;

        // DB row should now be Ready, tag populated, hash preserved.
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

        // Event envelope shape the UI subscribes to.
        {
            let events = captured.lock().unwrap();
            assert_eq!(events.len(), 1, "one envelope expected");
            assert_eq!(events[0].entity_type, "project_image");
            assert_eq!(events[0].action, "ready");
            assert_eq!(events[0].project_id.as_deref(), Some(project.id.as_str()));
            assert_eq!(
                events[0]
                    .payload
                    .get("image_hash")
                    .and_then(|v| v.as_str()),
                Some(hash_prefix)
            );
        }

        // Second re-fire with identical outcome should be coalesced by
        // the in-memory dedupe set — no additional DB write, no second
        // event emission.
        __test_handle_event(&cfg, &db, &bus, &mut seen, watcher::Event::Apply(job)).await;
        assert_eq!(
            captured.lock().unwrap().len(),
            1,
            "dedupe set must suppress the repeat Apply"
        );
    }

    /// `failed>=1` flips `projects.image_status` to `failed` and stores a
    /// diagnostic `image_last_error` referencing `kubectl logs`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_event_flips_status_and_records_last_error() {
        let cfg = ImageControllerConfig::for_testing();
        let db = Database::open_in_memory().expect("open_in_memory");
        let repo_seed = ProjectRepository::new(db.clone(), EventBus::noop());
        let project = repo_seed
            .create("demo-fail", "/tmp/demo-fail")
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
        assert!(err.contains(&job_name), "error '{err}' should cite job name");
        assert!(err.contains("kubectl logs"), "error '{err}' should hint logs");

        // The envelope for failures carries action=build_failed.
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
