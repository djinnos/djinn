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
//! 4. Acquire the in-flight guard + semaphore permit, upsert the
//!    per-build ConfigMap carrying the generated Dockerfile + scripts,
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
use kube::api::{Api, Patch, PatchParams, PostParams};
use tokio::sync::{Mutex, Semaphore};
use tracing::{debug, info, warn};

use crate::build_job::{
    build_context_config_map_name, build_image_build_context_config_map, build_image_build_job,
    build_job_owner_reference, sanitize_id,
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
    semaphore: Arc<Semaphore>,
    in_flight: Arc<Mutex<HashSet<String>>>,
}

impl ImageController {
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

    pub fn config(&self) -> &ImageControllerConfig {
        &self.config
    }

    /// Reconcile one project's image state.
    pub async fn enqueue(&self, project_id: &str) -> Result<()> {
        let repo = ProjectRepository::new(self.db.clone(), djinn_core::events::EventBus::noop());

        let raw = match repo.get_environment_config(project_id).await? {
            Some(s) => s,
            None => {
                debug!(project_id, "image_controller: project row missing — skipping");
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

        let cfg: EnvironmentConfig = serde_json::from_str(&raw).map_err(|e| {
            ImageControllerError::ConfigParse {
                project_id: project_id.to_string(),
                reason: e.to_string(),
            }
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

        let outcome = self
            .submit_build_job(&repo, project_id, &cfg, &new_hash, current.as_ref())
            .await;

        self.in_flight.lock().await.remove(project_id);
        outcome
    }

    async fn submit_build_job(
        &self,
        repo: &ProjectRepository,
        project_id: &str,
        cfg: &EnvironmentConfig,
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

        let (repo_ref, tag_ref) = split_image_ref(&self.config.agent_worker_image);
        let agent_worker_image = AgentWorkerImage::new(repo_ref, tag_ref);

        // 1. Generate the Dockerfile + script bundle.
        let build_context =
            generate_dockerfile(cfg, &agent_worker_image).map_err(|source| {
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

        let created_job = match jobs.get_opt(&job_name).await? {
            Some(existing) => {
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

        drop(permit);

        let image = ProjectImage {
            tag: previous.and_then(|p| p.tag.clone()),
            hash: Some(new_hash.to_string()),
            status: ProjectImageStatus::BUILDING.into(),
            last_error: None,
        };
        repo.set_project_image(project_id, &image).await?;
        Ok(())
    }

    async fn upsert_build_context_cm(
        &self,
        project_id: &str,
        hash_prefix: &str,
        ctx: &BuildContext,
    ) -> Result<()> {
        let cms: Api<ConfigMap> =
            Api::namespaced(self.client.clone(), &self.config.namespace);
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
        let cms: Api<ConfigMap> =
            Api::namespaced(self.client.clone(), &self.config.namespace);
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
}
