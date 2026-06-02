//! [`ImageController`] — per-project image reconciler.
//!
//! Runs inside the `djinn-server` process, invoked from
//! `mirror_fetcher::fetch_one` after every successful fetch + stack
//! detection. Best-effort: failures log + return Ok so a broken
//! cluster never breaks the mirror fetch tick.
//!
//! ## Flow (post-P5)
//!
//! 1. Read `projects.environment_config` from Dolt. If the row is still
//!    the migration-10 default (`'{}'`) or has `schema_version = 0`,
//!    skip — the boot reseed hook will seed it on the next server
//!    boot; until then there's nothing to build.
//! 2. Compute `djinn_image_builder::compute_environment_hash(&cfg,
//!    &agent_worker_ref)`. This covers config edits, installer-script
//!    edits, and worker-binary rebuilds — every input that could
//!    change the resulting image invalidates the hash.
//! 3. Compare against `projects.image_hash`. Skip if unchanged and the
//!    previous build reached `ready`.
//! 4. Take the per-project in-flight guard, then check the cluster-wide
//!    concurrency cap: count the live (pending + running) build Jobs the
//!    controller owns and defer this project to a later reconcile tick if
//!    the count is already at `max_concurrent`. Otherwise upsert the
//!    per-build ConfigMap carrying the generated Dockerfile + scripts and
//!    create the build Job.
//! 5. Write `projects.image_status = building` + `image_hash = <new>`.
//!
//! **What's gone.** No more `.devcontainer/devcontainer.json` read
//! path. No GitHub shallow-clone in the build Pod (Dockerfile
//! generator is self-contained). No per-project clone-URL Secret
//! (build-token Secret is deleted).

use std::collections::HashSet;
use std::sync::Arc;

use djinn_db::{Database, ProjectImage, ProjectImageStatus, ProjectRepository};
use djinn_image_builder::{
    AgentWorkerImage, BuildContext, compute_environment_hash, generate_dockerfile,
};
use djinn_stack::environment::EnvironmentConfig;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::ConfigMap;
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams, PostParams};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::build_job::{
    LABEL_BUILD, build_context_config_map_name, build_image_build_context_config_map,
    build_image_build_job, build_job_owner_reference, sanitize_id,
};
use crate::config::ImageControllerConfig;

/// How many hex characters of the environment hash to include in the
/// image tag and Job name suffix. 12 is enough to be globally unique
/// for realistic project counts and short enough for DNS label budgets.
const HASH_TAG_PREFIX_LEN: usize = 12;

/// Errors the controller surfaces from [`ImageController::enqueue`].
#[derive(Debug, thiserror::Error)]
pub enum ImageControllerError {
    #[error("db error: {0}")]
    Db(#[from] djinn_db::Error),
    #[error("kubernetes api error: {0}")]
    Kube(#[from] kube::Error),
    #[error("parse environment_config for project {project_id}: {reason}")]
    ConfigParse { project_id: String, reason: String },
    #[error("environment_config invalid for project {project_id}: {source}")]
    ConfigInvalid {
        project_id: String,
        #[source]
        source: djinn_stack::environment::EnvironmentConfigError,
    },
    #[error("dockerfile generation failed for project {project_id}: {source}")]
    Dockerfile {
        project_id: String,
        #[source]
        source: djinn_image_builder::DockerfileError,
    },
}

type Result<T> = std::result::Result<T, ImageControllerError>;

pub struct ImageController {
    client: kube::Client,
    config: ImageControllerConfig,
    db: Database,
    /// Per-project coalescing guard. Prevents two overlapping `enqueue`
    /// calls for the *same* project (e.g. a mirror-fetch tick racing the
    /// watcher-driven re-reconcile) from both creating a Job. This is an
    /// in-process dedupe, NOT the concurrency cap — that's enforced
    /// cluster-wide by [`ImageController::count_active_build_jobs`].
    in_flight: Arc<Mutex<HashSet<String>>>,
}

impl ImageController {
    pub fn new(client: kube::Client, config: ImageControllerConfig, db: Database) -> Self {
        Self {
            client,
            config,
            db,
            in_flight: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub fn config(&self) -> &ImageControllerConfig {
        &self.config
    }

    /// Reconcile one project's image state.
    pub async fn enqueue(&self, project_id: &str) -> Result<()> {
        let repo = ProjectRepository::new(self.db.clone(), djinn_core::events::EventBus::noop());

        let raw = match repo.get_environment_config(project_id).await? {
            Some(s) => s,
            None => {
                debug!(
                    project_id,
                    "image_controller: project row missing — skipping"
                );
                return Ok(());
            }
        };

        // The migration-10 default is the "needs reseed" sentinel — the
        // P5 boot reseed hook handles those on next server boot. The
        // controller is a no-op for un-seeded rows; once the hook runs
        // every project has a real config and the next tick builds.
        if raw.trim() == "{}" || raw.trim().is_empty() {
            debug!(
                project_id,
                "image_controller: environment_config empty (pre-reseed) — skipping"
            );
            return Ok(());
        }

        let cfg: EnvironmentConfig =
            serde_json::from_str(&raw).map_err(|e| ImageControllerError::ConfigParse {
                project_id: project_id.to_string(),
                reason: e.to_string(),
            })?;

        if cfg.schema_version == 0 {
            debug!(
                project_id,
                "image_controller: environment_config schema_version=0 (reseed pending) — skipping"
            );
            return Ok(());
        }

        cfg.validate()
            .map_err(|source| ImageControllerError::ConfigInvalid {
                project_id: project_id.to_string(),
                source,
            })?;

        let agent_worker_ref = self.config.agent_worker_image.clone();
        let new_hash = compute_environment_hash(&cfg, &agent_worker_ref);

        let current = repo.get_project_image(project_id).await?;
        let current_hash = current.as_ref().and_then(|r| r.hash.clone());
        let current_status = current.as_ref().map(|r| r.status.as_str()).unwrap_or("");
        if current_hash.as_deref() == Some(new_hash.as_str())
            && current_status == ProjectImageStatus::READY
        {
            debug!(
                project_id,
                hash = %short_hash(&new_hash),
                "image_controller: environment hash unchanged and image ready — skipping"
            );
            return Ok(());
        }
        debug!(
            project_id,
            hash = %short_hash(&new_hash),
            status = %current_status,
            "image_controller: enqueueing build (hash mismatch or image not ready)"
        );

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

        // Cluster-wide concurrency cap. Count the build Jobs we own that
        // are still running/pending (label-selected, terminal Jobs
        // excluded) — *excluding* this project's own Jobs so re-reconciling
        // an already-dispatched project (status flips, Failed-Job cleanup)
        // is never starved. If we're already at the cap, DEFER: skip
        // creating a new Job and leave the project's image state untouched
        // so the next reconcile tick re-evaluates it once a slot frees.
        // This is the real guard against the mass-rebuild herd that can
        // starve the single shared buildkitd into a liveness-kill crash
        // loop. The previous Job already exists / already terminal paths
        // inside `submit_build_job` stay reachable because we exclude this
        // project from the count.
        let cap = self.config.max_concurrent;
        let active = self
            .count_active_build_jobs(Some(&sanitize_id(project_id)))
            .await
            .unwrap_or_else(|e| {
                // A failed list shouldn't wedge the whole pipeline; log and
                // treat as "no other builds active" so we don't deadlock on
                // a transient API error. The per-project in-flight guard +
                // hash check still prevent duplicate work.
                warn!(
                    project_id,
                    error = %e,
                    "image_controller: failed to count active build Jobs — proceeding without cap this tick"
                );
                0
            });
        if build_slots_available(cap, active) == 0 {
            debug!(
                project_id,
                active,
                cap = cap.max(1),
                "image_controller: at max concurrent builds — deferring to a later reconcile tick"
            );
            self.in_flight.lock().await.remove(project_id);
            return Ok(());
        }

        let outcome = self
            .submit_build_job(&repo, project_id, &cfg, &new_hash, current.as_ref())
            .await;

        self.in_flight.lock().await.remove(project_id);
        outcome
    }

    /// Count build Jobs the controller owns that are still in flight
    /// (running or pending — i.e. not yet Succeeded or Failed) in the
    /// controller's namespace.
    ///
    /// Selected by the [`LABEL_BUILD`] label every build Job carries.
    /// When `exclude_project` is `Some`, Jobs whose
    /// `djinn.app/project-id` label matches are NOT counted — the caller
    /// uses this to avoid counting the project it's about to (re)reconcile
    /// against its own cap.
    async fn count_active_build_jobs(&self, exclude_project: Option<&str>) -> Result<usize> {
        let jobs: Api<Job> = Api::namespaced(self.client.clone(), &self.config.namespace);
        let lp = ListParams::default().labels(&format!("{LABEL_BUILD}=true"));
        let list = jobs.list(&lp).await?;
        Ok(count_active_jobs(&list.items, exclude_project))
    }

    async fn submit_build_job(
        &self,
        repo: &ProjectRepository,
        project_id: &str,
        cfg: &EnvironmentConfig,
        new_hash: &str,
        previous: Option<&ProjectImage>,
    ) -> Result<()> {
        let hash_prefix = &new_hash[..HASH_TAG_PREFIX_LEN.min(new_hash.len())];
        let image_tag = format_image_tag(&self.config.registry_host, project_id, hash_prefix);

        let (repo_ref, tag_ref) = split_image_ref(&self.config.agent_worker_image);
        let agent_worker_image = AgentWorkerImage::new(repo_ref, tag_ref);

        // 1. Generate the Dockerfile + script bundle.
        let build_context = generate_dockerfile(cfg, &agent_worker_image).map_err(|source| {
            ImageControllerError::Dockerfile {
                project_id: project_id.to_string(),
                source,
            }
        })?;

        // 2. Create the build-context ConfigMap *before* the Job so the
        // Pod's volume mount is satisfiable at startup. The CM is
        // per-build (per hash) so two different hashes never share
        // content.
        self.upsert_build_context_cm(project_id, hash_prefix, &build_context)
            .await?;

        // 3. Create the Job.
        let job = build_image_build_job(
            &self.config,
            project_id,
            hash_prefix,
            &image_tag,
            &build_context,
        );
        let jobs: Api<Job> = Api::namespaced(self.client.clone(), &self.config.namespace);
        let job_name = job
            .metadata
            .name
            .clone()
            .unwrap_or_else(|| format!("djinn-build-{project_id}-{hash_prefix}"));

        let existing = jobs.get_opt(&job_name).await?;
        let existing = match existing {
            Some(ref job) if is_job_failed(job) => {
                // Prior Job reached a terminal Failed state — typically
                // an early crash that exhausted `backoffLimit` (we've
                // seen this on cold-start races against buildkitd).
                // Leaving a Failed Job in place wedges the controller
                // permanently because every subsequent reconcile hits
                // the "Job already exists" branch and never retries.
                // Delete it with Background propagation so the Job
                // object goes away immediately; the next reconcile
                // tick will find no Job and create a fresh one with
                // buildkitd now warm.
                info!(
                    project_id,
                    hash = %hash_prefix,
                    job = %job_name,
                    namespace = %self.config.namespace,
                    "image_controller: build Job in Failed state — deleting so the next reconcile recreates it"
                );
                if let Err(e) = jobs.delete(&job_name, &DeleteParams::background()).await {
                    warn!(
                        project_id,
                        hash = %hash_prefix,
                        job = %job_name,
                        error = %e,
                        "image_controller: failed to delete Failed build Job — will retry on next reconcile"
                    );
                }
                return Ok(());
            }
            other => other,
        };

        let created_job = match existing {
            Some(existing) => {
                // If the existing Job already Succeeded, the watcher's
                // state-change event already flipped status to Ready —
                // and falling through to set status=Building below would
                // overwrite that, never to be re-flipped (watcher only
                // fires on Job state transitions, which already happened).
                // That's the "status stuck at building" loop we
                // reproduced when a retrigger nulls hash AFTER a Job
                // completed. Detect it here and reconcile status to
                // Ready inline.
                if is_job_succeeded(&existing) {
                    info!(
                        project_id,
                        hash = %hash_prefix,
                        job = %job_name,
                        namespace = %self.config.namespace,
                        "image_controller: build Job already Succeeded — reconciling status to Ready (watcher already fired)"
                    );
                    let image = ProjectImage {
                        tag: Some(image_tag.clone()),
                        hash: Some(new_hash.to_string()),
                        status: ProjectImageStatus::READY.into(),
                        last_error: None,
                    };
                    repo.set_project_image(project_id, &image).await?;
                    return Ok(());
                }
                info!(
                    project_id,
                    hash = %hash_prefix,
                    job = %job_name,
                    namespace = %self.config.namespace,
                    "image_controller: build Job already exists — leaving it running"
                );
                existing
            }
            None => {
                let created = jobs.create(&PostParams::default(), &job).await?;
                info!(
                    project_id,
                    hash = %hash_prefix,
                    job = %created.metadata.name.as_deref().unwrap_or_default(),
                    namespace = %self.config.namespace,
                    "image_controller: build Job created"
                );
                created
            }
        };

        // 4. Back-fill OwnerReference on the CM so it GCs with the Job.
        if let Some(owner) = build_job_owner_reference(&created_job) {
            self.set_context_cm_owner(project_id, hash_prefix, owner)
                .await?;
        }

        let image = ProjectImage {
            tag: previous.and_then(|p| p.tag.clone()),
            hash: Some(new_hash.to_string()),
            status: ProjectImageStatus::BUILDING.into(),
            last_error: None,
        };
        repo.set_project_image(project_id, &image).await?;
        Ok(())
    }

    /// Persist a fresh `EnvironmentConfig` for a project and update the
    /// runtime ConfigMap warm/task-run Pods mount. Invoked by the
    /// `project_environment_config_set` MCP tool.
    ///
    /// Order matters: the runtime CM is upserted *before* the DB write.
    /// The DB write nulls `image_hash`, which triggers a rebuild on the
    /// next mirror-fetch tick; warm Pods scheduled in the interim read
    /// the pre-update image plus the new CM, which is still closer to
    /// "what the user intended" than stale-both.
    ///
    /// The caller has already validated `cfg.validate()` — if they
    /// haven't, the DB write succeeds and the stored JSON is invalid;
    /// the next `enqueue` will surface the error.
    pub async fn apply_environment_config(
        &self,
        project_id: &str,
        cfg: &djinn_stack::environment::EnvironmentConfig,
    ) -> Result<()> {
        let json = serde_json::to_string(cfg).map_err(|e| ImageControllerError::ConfigParse {
            project_id: project_id.to_string(),
            reason: format!("serialise: {e}"),
        })?;

        // Upsert the runtime ConfigMap first. If this fails, abort
        // before touching the DB — the caller sees a clear error.
        self.upsert_runtime_env_cm(project_id, &json).await?;

        let repo = ProjectRepository::new(self.db.clone(), djinn_core::events::EventBus::noop());
        repo.set_environment_config(project_id, &json).await?;
        info!(
            project_id,
            "apply_environment_config: DB + runtime ConfigMap updated; next tick will rebuild"
        );
        Ok(())
    }

    async fn upsert_runtime_env_cm(
        &self,
        project_id: &str,
        environment_config_json: &str,
    ) -> Result<()> {
        let cms: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.config.namespace);
        let cm = djinn_k8s::build_env_config_config_map(
            &self.config.namespace,
            project_id,
            environment_config_json,
        );
        let name = djinn_k8s::env_config_config_map_name(project_id);
        let params = PatchParams::apply("djinn-image-controller").force();
        cms.patch(&name, &params, &Patch::Apply(&cm)).await?;
        debug!(
            project_id,
            cm = %name,
            "apply_environment_config: runtime env-config CM applied"
        );
        Ok(())
    }

    async fn upsert_build_context_cm(
        &self,
        project_id: &str,
        hash_prefix: &str,
        ctx: &BuildContext,
    ) -> Result<()> {
        let cms: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.config.namespace);
        let cm = build_image_build_context_config_map(&self.config, project_id, hash_prefix, ctx);
        let name = build_context_config_map_name(project_id, hash_prefix);
        let params = PatchParams::apply("djinn-image-controller").force();
        cms.patch(&name, &params, &Patch::Apply(&cm)).await?;
        debug!(
            project_id,
            hash = %hash_prefix,
            cm = %name,
            "image_controller: build-context ConfigMap applied"
        );
        Ok(())
    }

    async fn set_context_cm_owner(
        &self,
        project_id: &str,
        hash_prefix: &str,
        owner: k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference,
    ) -> Result<()> {
        let cms: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.config.namespace);
        let name = build_context_config_map_name(project_id, hash_prefix);
        let patch = serde_json::json!({
            "metadata": { "ownerReferences": [owner] }
        });
        let params = PatchParams::default();
        cms.patch(&name, &params, &Patch::Merge(&patch)).await?;
        Ok(())
    }
}

/// Split an image ref of the form `repo:tag` or `repo@sha256:...` into
/// its (repository, reference) parts. Falls back to `(whole, "latest")`
/// if the caller passed a bare repo with no tag.
fn split_image_ref(full: &str) -> (String, String) {
    // Colon after the last `/` — otherwise `:` inside a registry's
    // `host:port` prefix misparses.
    let last_slash = full.rfind('/').unwrap_or(0);
    let search = &full[last_slash..];
    if let Some(rel_colon) = search.find('@') {
        let colon = last_slash + rel_colon;
        return (full[..colon].to_string(), full[colon + 1..].to_string());
    }
    if let Some(rel_colon) = search.find(':') {
        let colon = last_slash + rel_colon;
        return (full[..colon].to_string(), full[colon + 1..].to_string());
    }
    (full.to_string(), "latest".to_string())
}

/// Build the content-addressable image tag.
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

/// Returns true when the Job has reached a terminal Failed state —
/// either `.status.failed >= 1` (a pod exhausted the Job's backoff
/// limit) or a `{type: "Failed", status: "True"}` condition is set.
///
/// A Failed Job never recovers on its own; the controller must tear
/// it down before a fresh reconcile can create a replacement.
fn is_job_failed(job: &Job) -> bool {
    let Some(status) = job.status.as_ref() else {
        return false;
    };
    if status.failed.unwrap_or(0) >= 1 {
        return true;
    }
    status
        .conditions
        .as_ref()
        .map(|conds| {
            conds
                .iter()
                .any(|c| c.type_ == "Failed" && c.status == "True")
        })
        .unwrap_or(false)
}

/// Whether a build Job has Succeeded (≥1 successful Pod or Complete
/// condition true). Mirror of [`is_job_failed`] used by the reconcile
/// path to detect "already-built" Jobs and avoid the
/// ready→building→ready loop the watcher can't recover from on its own
/// (watcher only fires on Job state transitions, which already happened).
fn is_job_succeeded(job: &Job) -> bool {
    let Some(status) = job.status.as_ref() else {
        return false;
    };
    if status.succeeded.unwrap_or(0) >= 1 {
        return true;
    }
    status
        .conditions
        .as_ref()
        .map(|conds| {
            conds
                .iter()
                .any(|c| c.type_ == "Complete" && c.status == "True")
        })
        .unwrap_or(false)
}

/// Whether a build Job is still in flight — neither Succeeded nor
/// Failed. Used by [`ImageController::count_active_build_jobs`] to size
/// the concurrency cap against the *real* cluster state (pending +
/// running Jobs), not the rate of Job creation. A Job with no status yet
/// (just created, controller hasn't populated `.status`) counts as
/// active.
fn is_job_active(job: &Job) -> bool {
    !is_job_succeeded(job) && !is_job_failed(job)
}

/// Count the active (in-flight) build Jobs in `jobs`, optionally
/// excluding any whose `djinn.app/project-id` label equals
/// `exclude_project`. Pure so the cap arithmetic is unit-testable
/// without a live API server.
fn count_active_jobs(jobs: &[Job], exclude_project: Option<&str>) -> usize {
    jobs.iter()
        .filter(|job| {
            if let Some(exclude) = exclude_project {
                let proj = job
                    .metadata
                    .labels
                    .as_ref()
                    .and_then(|l| l.get(crate::build_job::LABEL_PROJECT_ID));
                if proj.map(String::as_str) == Some(exclude) {
                    return false;
                }
            }
            is_job_active(job)
        })
        .count()
}

/// How many *new* build Jobs may be created this reconcile pass given a
/// cap and the count of Jobs already in flight: `max(0, cap - in_flight)`.
/// The cap is clamped to ≥1 so a misconfigured `0` never wedges the
/// pipeline entirely. Deferred projects are not lost — they're picked up
/// on a later tick as slots free.
fn build_slots_available(cap: usize, in_flight: usize) -> usize {
    cap.max(1).saturating_sub(in_flight)
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

    #[test]
    fn split_image_ref_handles_tag() {
        assert_eq!(
            split_image_ref("djinn/agent-runtime:dev"),
            ("djinn/agent-runtime".into(), "dev".into())
        );
    }

    #[test]
    fn split_image_ref_handles_registry_port_and_tag() {
        assert_eq!(
            split_image_ref("localhost:5001/djinn/agent-runtime:abc123"),
            ("localhost:5001/djinn/agent-runtime".into(), "abc123".into())
        );
    }

    #[test]
    fn split_image_ref_handles_digest() {
        assert_eq!(
            split_image_ref("reg/x@sha256:deadbeef"),
            ("reg/x".into(), "sha256:deadbeef".into())
        );
    }

    #[test]
    fn split_image_ref_falls_back_to_latest() {
        assert_eq!(
            split_image_ref("djinn/agent-runtime"),
            ("djinn/agent-runtime".into(), "latest".into())
        );
    }

    #[test]
    fn is_job_failed_false_when_no_status() {
        let job = Job::default();
        assert!(!is_job_failed(&job));
    }

    #[test]
    fn is_job_failed_true_when_failed_count_nonzero() {
        use k8s_openapi::api::batch::v1::JobStatus;
        let job = Job {
            status: Some(JobStatus {
                failed: Some(1),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(is_job_failed(&job));
    }

    #[test]
    fn is_job_failed_true_when_failed_condition_is_true() {
        use k8s_openapi::api::batch::v1::{JobCondition, JobStatus};
        let job = Job {
            status: Some(JobStatus {
                conditions: Some(vec![JobCondition {
                    type_: "Failed".into(),
                    status: "True".into(),
                    last_probe_time: None,
                    last_transition_time: None,
                    message: Some("BackoffLimitExceeded".into()),
                    reason: Some("BackoffLimitExceeded".into()),
                }]),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(is_job_failed(&job));
    }

    use std::collections::BTreeMap;

    use k8s_openapi::api::batch::v1::JobStatus;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    /// Build a Job carrying the build label + a project-id label, with an
    /// optional terminal status (`Some(true)` = succeeded, `Some(false)`
    /// = failed, `None` = still active).
    fn build_job_with(project: &str, terminal: Option<bool>) -> Job {
        let mut labels = BTreeMap::new();
        labels.insert(LABEL_BUILD.to_string(), "true".to_string());
        labels.insert(
            crate::build_job::LABEL_PROJECT_ID.to_string(),
            project.to_string(),
        );
        let status = terminal.map(|ok| {
            if ok {
                JobStatus {
                    succeeded: Some(1),
                    ..Default::default()
                }
            } else {
                JobStatus {
                    failed: Some(1),
                    ..Default::default()
                }
            }
        });
        Job {
            metadata: ObjectMeta {
                name: Some(format!("djinn-build-{project}-abc")),
                labels: Some(labels),
                ..Default::default()
            },
            status,
            ..Default::default()
        }
    }

    #[test]
    fn is_job_active_true_when_no_status() {
        // A freshly-created Job has no `.status` populated yet — it must
        // count toward the cap so a burst of just-created Jobs doesn't slip
        // past the gate before the kubelet fills in status.
        assert!(is_job_active(&Job::default()));
    }

    #[test]
    fn is_job_active_false_for_terminal_jobs() {
        assert!(!is_job_active(&build_job_with("p", Some(true))));
        assert!(!is_job_active(&build_job_with("p", Some(false))));
    }

    #[test]
    fn count_active_jobs_ignores_terminal_and_excluded() {
        let jobs = vec![
            build_job_with("a", None),        // active, other project
            build_job_with("b", None),        // active, other project
            build_job_with("c", Some(true)),  // succeeded — not active
            build_job_with("d", Some(false)), // failed — not active
            build_job_with("me", None),       // active, but excluded
        ];
        // Exclude "me": two genuinely-active other-project builds remain.
        assert_eq!(count_active_jobs(&jobs, Some("me")), 2);
        // Without exclusion the current project's active Job counts too.
        assert_eq!(count_active_jobs(&jobs, None), 3);
    }

    #[test]
    fn build_slots_available_is_cap_minus_in_flight() {
        // Given a cap of 4 and 1 in-flight, 3 new Jobs may be created.
        assert_eq!(build_slots_available(4, 1), 3);
        // At the cap, zero new Jobs — deferred projects wait for a later tick.
        assert_eq!(build_slots_available(4, 4), 0);
        // Over the cap (e.g. a hotfix lowered it below current load) clamps
        // to zero rather than underflowing.
        assert_eq!(build_slots_available(4, 9), 0);
        // No in-flight builds → full cap available.
        assert_eq!(build_slots_available(4, 0), 4);
    }

    #[test]
    fn build_slots_available_clamps_zero_cap_to_one() {
        // A misconfigured cap of 0 must not wedge the pipeline forever; it
        // is treated as 1 so at least one build can always proceed.
        assert_eq!(build_slots_available(0, 0), 1);
        assert_eq!(build_slots_available(0, 1), 0);
    }

    #[test]
    fn cap_admits_only_remaining_slots_across_a_burst() {
        // Model the herd: N projects all need a build, M already in flight.
        // Only max(0, cap - M) of them should be admitted this pass; the
        // rest defer (and remain eligible next tick — the controller never
        // marks a deferred project failed).
        let cap = 4;
        let m_in_flight = 1;
        let n_projects = 17;
        let admitted = build_slots_available(cap, m_in_flight).min(n_projects);
        assert_eq!(admitted, 3);
        let deferred = n_projects - admitted;
        assert_eq!(deferred, 14);
    }

    #[test]
    fn is_job_failed_false_when_complete_condition_present() {
        use k8s_openapi::api::batch::v1::{JobCondition, JobStatus};
        let job = Job {
            status: Some(JobStatus {
                conditions: Some(vec![JobCondition {
                    type_: "Complete".into(),
                    status: "True".into(),
                    last_probe_time: None,
                    last_transition_time: None,
                    message: None,
                    reason: None,
                }]),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(!is_job_failed(&job));
    }
}
