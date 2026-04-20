//! [`ImageBuildWatcher`] — watches build `Job`s to terminal state.
//!
//! Phase 3 PR 5.5.  PR 5 shipped [`crate::ImageController`], which fires
//! per-project `@devcontainers/cli` build Jobs but never watches their
//! outcome — `projects.image_status` stays pinned to `"building"` until
//! this watcher flips it.  Without this, `KubernetesRuntime::prepare`
//! rejects every task with `DevcontainerMissing`.
//!
//! # Flow
//!
//! `ImageBuildWatcher::spawn` returns a detached [`JoinHandle<()>`] that
//! runs a [`kube::runtime::watcher`] over the `Job`s carrying
//! `djinn.app/build=true`.  For every `Event::Apply` /
//! `Event::InitApply` we inspect `.status`:
//!
//! - `.status.succeeded >= 1`: mark `projects.image_status = ready`,
//!   populate `image_tag` + `image_hash` from labels, emit
//!   `project.image.ready`.
//! - `.status.failed >= 1`: mark `projects.image_status = failed`,
//!   populate `image_last_error` with a stable diagnostic string
//!   (`"build Job <name> failed; see kubectl logs job/<name>"`), emit
//!   `project.image.build_failed`.
//!
//! `Delete` / `Init` / `InitDone` events are ignored — the build Job
//! carries `ttl_seconds_after_finished` so the record disappears after a
//! grace window and the transition we care about has already been
//! persisted.
//!
//! Label correlators come straight from PR 5's [`crate::build_job`]:
//! `djinn.app/project-id`, `djinn.app/image-hash`.
//!
//! # Graceful shutdown
//!
//! The spawned task observes the [`CancellationToken`] threaded in from
//! `AppState::cancel()`.  On cancel it drops the watcher stream and
//! exits cleanly.
//!
//! # In-memory idempotency
//!
//! The watcher keeps a small `HashSet<String>` of
//! `(project_id, hash_prefix)` keys it has already flipped.  Job events
//! re-fire when the stream restarts; without this guard every restart
//! re-emits `project.image.ready` and re-writes the DB row for already-
//! completed builds.  The set bounds itself at [`DEDUPE_CAP`] entries —
//! ample for realistic project counts, trivial to rebuild on restart.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_db::{Database, ProjectImage, ProjectImageStatus, ProjectRepository};
use futures::StreamExt;
use k8s_openapi::api::batch::v1::Job;
use kube::api::Api;
use kube::runtime::watcher;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::build_job::{LABEL_BUILD, LABEL_IMAGE_HASH, LABEL_PROJECT_ID};
use crate::config::ImageControllerConfig;
use crate::controller::format_image_tag;

/// Soft cap on the in-memory "already reconciled" set. Prevents the
/// watcher from growing unbounded across very long uptimes.  When the
/// cap is hit the oldest entries are dropped wholesale — a restart
/// re-establishes them on first Apply.
const DEDUPE_CAP: usize = 1024;

/// Backoff between watcher restarts when the stream errors out.  The
/// upstream `kube::runtime::watcher` handles reconnects internally; this
/// value only gates how fast we re-enter the match loop after a hard
/// break.
const POST_ERROR_SLEEP: Duration = Duration::from_secs(2);

/// Background task that watches image-build Jobs to completion.
///
/// Construct via [`Self::spawn`]; the returned [`JoinHandle`] lets the
/// server's shutdown path `abort()` + `await` the task.
pub struct ImageBuildWatcher;

impl ImageBuildWatcher {
    /// Spawn the watcher task and return its [`JoinHandle`].
    ///
    /// The task:
    /// - lists+watches `Job`s in `config.namespace` with
    ///   `label_selector=djinn.app/build=true`;
    /// - for each `Apply`/`InitApply` event inspects `.status`;
    /// - writes DB + emits events on terminal states;
    /// - exits cleanly when `cancel` is triggered.
    pub fn spawn(
        client: kube::Client,
        config: ImageControllerConfig,
        db: Database,
        event_bus: EventBus,
        cancel: CancellationToken,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            run_loop(client, config, db, event_bus, cancel).await;
        })
    }
}

async fn run_loop(
    client: kube::Client,
    config: ImageControllerConfig,
    db: Database,
    event_bus: EventBus,
    cancel: CancellationToken,
) {
    let jobs: Api<Job> = Api::namespaced(client, &config.namespace);
    let watch_cfg = watcher::Config::default().labels(&format!("{}=true", LABEL_BUILD));

    let mut seen: HashSet<String> = HashSet::new();
    let repo = ProjectRepository::new(db.clone(), event_bus.clone());

    info!(
        namespace = %config.namespace,
        label = %LABEL_BUILD,
        "image_build_watcher: starting"
    );

    'outer: loop {
        let mut stream = watcher::watcher(jobs.clone(), watch_cfg.clone()).boxed();

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    info!("image_build_watcher: cancellation observed; exiting");
                    return;
                }
                next = stream.next() => {
                    match next {
                        Some(Ok(ev)) => {
                            handle_event(&config, &repo, &event_bus, &mut seen, ev).await;
                        }
                        Some(Err(e)) => {
                            warn!(error = %e, "image_build_watcher: stream error; reconnecting");
                            tokio::time::sleep(POST_ERROR_SLEEP).await;
                            continue 'outer;
                        }
                        None => {
                            debug!("image_build_watcher: stream ended; reconnecting");
                            continue 'outer;
                        }
                    }
                }
            }
        }
    }
}

/// Exposed for integration tests — drives the same transition logic the
/// watcher's inner loop calls, without having to stand up a live
/// `kube::Client`. Production code calls the private in-module version.
///
/// `seen` is the caller-owned dedupe set so tests can assert the
/// idempotency guard.
#[doc(hidden)]
pub async fn __test_handle_event(
    config: &ImageControllerConfig,
    db: &Database,
    event_bus: &EventBus,
    seen: &mut HashSet<String>,
    ev: watcher::Event<Job>,
) {
    let repo = ProjectRepository::new(db.clone(), event_bus.clone());
    handle_event(config, &repo, event_bus, seen, ev).await;
}

async fn handle_event(
    config: &ImageControllerConfig,
    repo: &ProjectRepository,
    event_bus: &EventBus,
    seen: &mut HashSet<String>,
    ev: watcher::Event<Job>,
) {
    let job = match ev {
        watcher::Event::Apply(j) | watcher::Event::InitApply(j) => j,
        watcher::Event::Delete(_) | watcher::Event::Init | watcher::Event::InitDone => return,
    };

    let Some(outcome) = classify(&job) else {
        // Still running / no status yet — wait for the next Apply.
        return;
    };

    let labels = job
        .metadata
        .labels
        .as_ref()
        .cloned()
        .unwrap_or_default();
    let Some(project_id) = labels.get(LABEL_PROJECT_ID).cloned() else {
        warn!(
            job = ?job.metadata.name,
            "image_build_watcher: Job missing djinn.app/project-id label; skipping"
        );
        return;
    };
    let Some(hash_prefix) = labels.get(LABEL_IMAGE_HASH).cloned() else {
        warn!(
            job = ?job.metadata.name,
            project_id,
            "image_build_watcher: Job missing djinn.app/image-hash label; skipping"
        );
        return;
    };
    let job_name = job
        .metadata
        .name
        .clone()
        .unwrap_or_else(|| "<unknown>".into());

    let dedupe_key = format!("{project_id}:{hash_prefix}:{}", outcome.kind_str());
    if seen.contains(&dedupe_key) {
        debug!(
            project_id,
            hash = %hash_prefix,
            outcome = outcome.kind_str(),
            "image_build_watcher: already reconciled; skipping"
        );
        return;
    }

    match outcome {
        JobOutcome::Succeeded => {
            apply_success(config, repo, event_bus, &project_id, &hash_prefix, &job_name, &labels)
                .await;
        }
        JobOutcome::Failed => {
            apply_failure(repo, event_bus, &project_id, &hash_prefix, &job_name).await;
        }
    }

    if seen.len() >= DEDUPE_CAP {
        seen.clear();
    }
    seen.insert(dedupe_key);
}

async fn apply_success(
    config: &ImageControllerConfig,
    repo: &ProjectRepository,
    event_bus: &EventBus,
    project_id: &str,
    hash_prefix: &str,
    job_name: &str,
    labels: &BTreeMap<String, String>,
) {
    // Recover the original full hash from the DB (labels only carry the
    // prefix). The controller wrote the full hash in `submit_build_job`;
    // if the row is gone or the hash doesn't match, fall back to the
    // prefix rather than drop the transition.
    let full_hash = match repo.get_project_image(project_id).await {
        Ok(Some(existing)) => existing.hash.unwrap_or_else(|| hash_prefix.to_string()),
        _ => hash_prefix.to_string(),
    };

    let image_tag = format_image_tag(&config.registry_host, project_id, hash_prefix);
    let image = ProjectImage {
        tag: Some(image_tag.clone()),
        hash: Some(full_hash.clone()),
        status: ProjectImageStatus::READY.to_string(),
        last_error: None,
    };

    if let Err(e) = repo.set_project_image(project_id, &image).await {
        warn!(
            project_id,
            hash = %hash_prefix,
            job = %job_name,
            error = %e,
            "image_build_watcher: set_project_image(Ready) failed"
        );
        return;
    }

    info!(
        project_id,
        hash = %hash_prefix,
        job = %job_name,
        image_tag = %image_tag,
        "image_build_watcher: build Job succeeded — flipped projects.image_status to ready"
    );

    event_bus.send(image_status_event(
        "ready",
        project_id,
        Some(&image_tag),
        Some(hash_prefix),
        None,
        labels.get(LABEL_BUILD).map(String::as_str),
        job_name,
    ));
}

async fn apply_failure(
    repo: &ProjectRepository,
    event_bus: &EventBus,
    project_id: &str,
    hash_prefix: &str,
    job_name: &str,
) {
    let last_error = format!(
        "build Job {job_name} failed; see kubectl logs job/{job_name}"
    );

    let previous = repo.get_project_image(project_id).await.ok().flatten();
    let image = ProjectImage {
        // Preserve the previously-ready tag if any so the runtime can
        // still dispatch against the last-known-good image while the
        // user investigates. The UI banner surfaces `last_error`.
        tag: previous.as_ref().and_then(|p| p.tag.clone()),
        // Keep the hash we attempted to build — matches how PR 5 writes it.
        hash: Some(
            previous
                .as_ref()
                .and_then(|p| p.hash.clone())
                .unwrap_or_else(|| hash_prefix.to_string()),
        ),
        status: ProjectImageStatus::FAILED.to_string(),
        last_error: Some(last_error.clone()),
    };

    if let Err(e) = repo.set_project_image(project_id, &image).await {
        warn!(
            project_id,
            hash = %hash_prefix,
            job = %job_name,
            error = %e,
            "image_build_watcher: set_project_image(Failed) failed"
        );
        return;
    }

    warn!(
        project_id,
        hash = %hash_prefix,
        job = %job_name,
        "image_build_watcher: build Job failed — flipped projects.image_status to failed"
    );

    event_bus.send(image_status_event(
        "build_failed",
        project_id,
        None,
        Some(hash_prefix),
        Some(&last_error),
        None,
        job_name,
    ));
}

#[derive(Clone, Copy, Debug)]
enum JobOutcome {
    Succeeded,
    Failed,
}

impl JobOutcome {
    fn kind_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }
}

/// Inspect a Job's `.status` and return a terminal outcome, or `None`
/// if it is still running (no counts set yet).
fn classify(job: &Job) -> Option<JobOutcome> {
    let status = job.status.as_ref()?;
    if status.succeeded.unwrap_or(0) >= 1 {
        return Some(JobOutcome::Succeeded);
    }
    if status.failed.unwrap_or(0) >= 1 {
        return Some(JobOutcome::Failed);
    }
    None
}

/// Build the `DjinnEventEnvelope` the UI (PR 6) subscribes to.
fn image_status_event(
    action_suffix: &'static str,
    project_id: &str,
    image_tag: Option<&str>,
    hash_prefix: Option<&str>,
    error: Option<&str>,
    _unused_label: Option<&str>,
    job_name: &str,
) -> DjinnEventEnvelope {
    // Synthetic envelope (no first-party helper on `DjinnEventEnvelope`
    // for image events yet — PR 6 adds a dedicated helper).  Shape is
    // stable so the UI subscriber can pattern-match on
    // `entity_type = "project_image"` + `action = "ready"|"build_failed"`.
    let action: &'static str = match action_suffix {
        "ready" => "ready",
        "build_failed" => "build_failed",
        _ => "updated",
    };
    DjinnEventEnvelope {
        entity_type: "project_image",
        action,
        payload: serde_json::json!({
            "project_id": project_id,
            "image_tag": image_tag,
            "image_hash": hash_prefix,
            "last_error": error,
            "job": job_name,
        }),
        id: None,
        project_id: Some(project_id.to_string()),
        from_sync: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::batch::v1::JobStatus;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    fn job_with_status(
        name: &str,
        project_id: &str,
        hash: &str,
        succeeded: Option<i32>,
        failed: Option<i32>,
    ) -> Job {
        let mut labels = BTreeMap::new();
        labels.insert(LABEL_BUILD.into(), "true".into());
        labels.insert(LABEL_PROJECT_ID.into(), project_id.into());
        labels.insert(LABEL_IMAGE_HASH.into(), hash.into());
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

    #[test]
    fn classify_reads_succeeded_first() {
        let job = job_with_status("j", "p", "h", Some(1), None);
        assert!(matches!(classify(&job), Some(JobOutcome::Succeeded)));
    }

    #[test]
    fn classify_reads_failed_when_succeeded_is_zero() {
        let job = job_with_status("j", "p", "h", None, Some(2));
        assert!(matches!(classify(&job), Some(JobOutcome::Failed)));
    }

    #[test]
    fn classify_returns_none_while_running() {
        let job = job_with_status("j", "p", "h", None, None);
        assert!(classify(&job).is_none());
    }

    #[test]
    fn classify_returns_none_when_status_missing() {
        let mut job = job_with_status("j", "p", "h", None, None);
        job.status = None;
        assert!(classify(&job).is_none());
    }

    #[test]
    fn image_status_event_ready_shape() {
        let envelope = image_status_event(
            "ready",
            "proj-abc",
            Some("reg:5000/djinn-project-proj-abc:1a2b3c4d5e6f"),
            Some("1a2b3c4d5e6f"),
            None,
            None,
            "djinn-build-proj-abc-1a2b3c4d5e6f",
        );
        assert_eq!(envelope.entity_type, "project_image");
        assert_eq!(envelope.action, "ready");
        assert_eq!(envelope.project_id.as_deref(), Some("proj-abc"));
        assert_eq!(
            envelope.payload.get("image_tag").and_then(|v| v.as_str()),
            Some("reg:5000/djinn-project-proj-abc:1a2b3c4d5e6f")
        );
        assert_eq!(
            envelope.payload.get("image_hash").and_then(|v| v.as_str()),
            Some("1a2b3c4d5e6f")
        );
    }

    #[test]
    fn image_status_event_build_failed_carries_last_error() {
        let envelope = image_status_event(
            "build_failed",
            "proj-xyz",
            None,
            Some("deadbeef"),
            Some("build Job djinn-build-proj-xyz-deadbeef failed; see kubectl logs job/djinn-build-proj-xyz-deadbeef"),
            None,
            "djinn-build-proj-xyz-deadbeef",
        );
        assert_eq!(envelope.action, "build_failed");
        assert!(
            envelope
                .payload
                .get("last_error")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .contains("kubectl logs")
        );
    }
}
