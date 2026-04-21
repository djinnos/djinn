//! [`ImageController`] — per-project devcontainer-image reconciler.
//!
//! Runs inside the `djinn-server` process. Invoked from
//! `mirror_fetcher::fetch_one` after every successful fetch + stack
//! detection. Best-effort: failures log + return Ok so a broken cluster
//! never breaks the mirror fetch tick.
//!
//! ## Flow
//!
//! 1. Bail out early if `stack.manifest_signals.has_devcontainer == false`
//!    — the UI banner handles onboarding.
//! 2. Compute `sha256(devcontainer.json [+ devcontainer-lock.json])` from
//!    the bare mirror at `HEAD` via [`crate::hash::compute_devcontainer_hash`].
//! 3. Compare against `projects.image_hash`. If unchanged, log + return.
//! 4. Else: acquire the in-flight guard (skip duplicate enqueues), acquire
//!    a semaphore permit (cap cluster-wide build concurrency), and create
//!    the build Job via `kube::Api::<Job>::create`.
//! 5. Write `projects.image_status = "building"` + `image_hash = <new>` so
//!    `KubernetesRuntime::prepare` can see the state.
//!
//! **Known limitation.** This PR does *not* watch the Job through
//! completion — status stays `"building"` until a follow-up reconcile
//! loop (planned for a later PR per plan §5.5 step 6/7) observes the
//! Job's terminal state and flips the column to `"ready"` / `"failed"`.
//! The Job carries `djinn.app/project-id` + `djinn.app/image-hash` labels
//! so that future loop can correlate without an in-process side channel.

use std::collections::HashSet;
use std::sync::Arc;

use djinn_db::{Database, ProjectImage, ProjectImageStatus, ProjectRepository};
use djinn_stack::Stack;
use k8s_openapi::api::batch::v1::Job;
use kube::api::{Api, PostParams};
use tokio::sync::{Mutex, Semaphore};
use tracing::{debug, info, warn};

use crate::build_job::{build_image_build_job, sanitize_id};
use crate::config::ImageControllerConfig;
use crate::hash::compute_devcontainer_hash;
use djinn_workspace::mirror_path_for;

/// How many hex characters of the devcontainer hash to include in the
/// image tag and Job name suffix. 12 is enough to be globally unique
/// for realistic project counts and short enough for DNS label budgets.
const HASH_TAG_PREFIX_LEN: usize = 12;

/// Errors the controller surfaces from [`ImageController::enqueue`].
///
/// Most of these degrade to a `warn!` log at the call site — the
/// mirror-fetcher swallows them so a broken build pipeline never breaks
/// the mirror fetch tick.
#[derive(Debug, thiserror::Error)]
pub enum ImageControllerError {
    #[error("image hash computation failed: {0}")]
    Hash(#[source] anyhow::Error),
    #[error("db error: {0}")]
    Db(#[from] djinn_db::Error),
    #[error("kubernetes api error: {0}")]
    Kube(#[from] kube::Error),
}

type Result<T> = std::result::Result<T, ImageControllerError>;

/// Controller handle. Cheaply cloneable via the outer `Arc`.
///
/// The `Database` clone is small; `Semaphore` + `Mutex<HashSet>` are
/// the bookkeeping that enforce single-flight per project AND capped
/// cluster-wide concurrency.
pub struct ImageController {
    client: kube::Client,
    config: ImageControllerConfig,
    db: Database,
    semaphore: Arc<Semaphore>,
    in_flight: Arc<Mutex<HashSet<String>>>,
}

impl ImageController {
    /// Construct a controller bound to an already-built `kube::Client`.
    ///
    /// The server boot path calls `kube::Client::try_default()` and wires
    /// the resulting client here. In dev environments without a cluster
    /// the server skips controller construction entirely — the mirror
    /// fetcher sees `None` and silently skips the enqueue step.
    pub fn new(client: kube::Client, config: ImageControllerConfig, db: Database) -> Self {
        let cap = config.max_concurrent.max(1);
        Self {
            client,
            config,
            db,
            semaphore: Arc::new(Semaphore::new(cap)),
            in_flight: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Snapshot the active config (tests + introspection).
    pub fn config(&self) -> &ImageControllerConfig {
        &self.config
    }

    /// Reconcile one project's image state.
    ///
    /// Returns `Ok(())` on all expected states (no devcontainer, unchanged
    /// hash, duplicate enqueue coalesced, Job created). Reserves
    /// `ImageControllerError` for hash / DB / cluster faults — the caller
    /// is expected to warn-log and proceed.
    pub async fn enqueue(&self, project_id: &str, stack: &Stack) -> Result<()> {
        // Fast-path: no committed devcontainer → nothing to build. The
        // onboarding banner in PR 6 drives the user to create one.
        if !stack.manifest_signals.has_devcontainer {
            debug!(
                project_id,
                "image_controller: skipping — stack reports no devcontainer"
            );
            return Ok(());
        }

        let mirror_path = self.resolve_mirror_path(project_id);
        let Some(new_hash) = compute_devcontainer_hash(&mirror_path)
            .map_err(ImageControllerError::Hash)?
        else {
            debug!(
                project_id,
                mirror = %mirror_path.display(),
                "image_controller: stack flagged devcontainer but mirror HEAD has none — skipping"
            );
            return Ok(());
        };

        let repo = ProjectRepository::new(self.db.clone(), djinn_core::events::EventBus::noop());
        let current = repo.get_project_image(project_id).await?;
        let current_hash = current.as_ref().and_then(|r| r.hash.clone());
        if current_hash.as_deref() == Some(new_hash.as_str()) {
            debug!(
                project_id,
                hash = %short_hash(&new_hash),
                "image_controller: devcontainer hash unchanged — skipping rebuild"
            );
            return Ok(());
        }

        // In-flight guard: swallow duplicate enqueues for the same project
        // without stalling callers behind a held semaphore permit.
        {
            let mut guard = self.in_flight.lock().await;
            if !guard.insert(project_id.to_string()) {
                debug!(
                    project_id,
                    "image_controller: build already in flight — coalescing"
                );
                return Ok(());
            }
        }

        // From here on we must always drop the in-flight entry before
        // returning. Wrap the body so the result is returned unchanged.
        let outcome = self
            .submit_build_job(&repo, project_id, &new_hash, current.as_ref())
            .await;

        self.in_flight.lock().await.remove(project_id);
        outcome
    }

    async fn submit_build_job(
        &self,
        repo: &ProjectRepository,
        project_id: &str,
        new_hash: &str,
        previous: Option<&ProjectImage>,
    ) -> Result<()> {
        let permit = match self.semaphore.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => {
                warn!(
                    project_id,
                    "image_controller: semaphore closed — dropping enqueue"
                );
                return Ok(());
            }
        };

        let hash_prefix = &new_hash[..HASH_TAG_PREFIX_LEN.min(new_hash.len())];
        let image_tag = format_image_tag(&self.config.registry_host, project_id, hash_prefix);
        let job = build_image_build_job(&self.config, project_id, hash_prefix, &image_tag);

        let jobs: Api<Job> = Api::namespaced(self.client.clone(), &self.config.namespace);
        let created = jobs.create(&PostParams::default(), &job).await?;
        info!(
            project_id,
            hash = %hash_prefix,
            job = %created.metadata.name.as_deref().unwrap_or_default(),
            namespace = %self.config.namespace,
            "image_controller: build Job created"
        );
        drop(permit);

        // Flip state to `building` + stash the new hash so subsequent
        // ticks compare correctly. We intentionally keep the previous
        // `image_tag` around (if any) so `KubernetesRuntime::prepare`
        // doesn't fail-hard in the window between "building started" and
        // "first build finished". A follow-up reconcile flips to
        // `ready` + overwrites `image_tag` once the Job terminates.
        let image = ProjectImage {
            tag: previous.and_then(|p| p.tag.clone()),
            hash: Some(new_hash.to_string()),
            status: ProjectImageStatus::BUILDING.into(),
            last_error: None,
        };
        repo.set_project_image(project_id, &image).await?;
        Ok(())
    }

    fn resolve_mirror_path(&self, project_id: &str) -> std::path::PathBuf {
        mirror_path_for(project_id)
    }
}

/// Build the content-addressable image tag.
///
/// Shape: `<registry>/djinn-project-<sanitized_id>:<hash_prefix>` so
/// every build produces a distinct, immutable image reference the
/// runtime can pin to.
pub fn format_image_tag(registry: &str, project_id: &str, hash_prefix: &str) -> String {
    format!(
        "{}/djinn-project-{}:{}",
        registry.trim_end_matches('/'),
        sanitize_id(project_id),
        hash_prefix
    )
}

fn short_hash(hash: &str) -> &str {
    &hash[..HASH_TAG_PREFIX_LEN.min(hash.len())]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_image_tag_matches_registry_and_project() {
        let tag = format_image_tag("reg.example:5000", "proj-abc", "1a2b3c4d5e6f");
        assert_eq!(tag, "reg.example:5000/djinn-project-proj-abc:1a2b3c4d5e6f");
    }

    #[test]
    fn format_image_tag_sanitizes_project_id() {
        let tag = format_image_tag("r:5000", "Weird/ID_1", "deadbeefcafe");
        assert_eq!(tag, "r:5000/djinn-project-weird-id-1:deadbeefcafe");
    }
}
