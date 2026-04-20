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

    // The volume set includes the mirror PVC (read-only) and the
    // registry-auth Secret.
    let volumes = pod.volumes.as_ref().unwrap();
    assert!(volumes.iter().any(|v| v.persistent_volume_claim.is_some()));
    assert!(volumes.iter().any(|v| v.secret.is_some()));
}
