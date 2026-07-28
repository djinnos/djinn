//! Per-project image controller — post-P5, djinn-native.
//!
//! Runs inside the `djinn-server` process (§5.5 of the plan — no
//! separate Pod, no leader election). `mirror_fetcher::fetch_one` calls
//! [`ImageController::enqueue`] at the tail of every successful fetch;
//! the boot reseed hook also invokes it after seeding an empty
//! `environment_config` column.
//!
//! ## Scope of P5
//!
//! * Reads `projects.environment_config` (migration 10).
//! * Hashes via `djinn_image_builder::compute_environment_hash` — config
//!   JSON + script-bundle sha + agent-worker image ref.
//! * Generates the Dockerfile via
//!   `djinn_image_builder::generate_dockerfile`.
//! * Upserts a per-build ConfigMap (Dockerfile + install scripts) and
//!   dispatches a Job that runs `buildctl build` against it.
//! * No `.devcontainer/**` read path. No `@devcontainers/cli`. No
//!   GitHub shallow-clone in the builder Pod.
//!
//! ```text
//! mirror_fetcher::fetch_one
//!     └─> ImageController::enqueue(project_id)
//!           ├─ read projects.environment_config
//!           ├─ skip if '{}' (un-seeded — boot reseed hook handles)
//!           ├─ compute_environment_hash(cfg, agent_worker_ref)
//!           ├─ compare to projects.image_hash
//!           ├─ if changed: take in-flight guard + check the cluster-wide
//!           │    concurrency cap (count live build Jobs; defer if at cap)
//!           │    ├─ generate_dockerfile(cfg, agent_worker)
//!           │    ├─ upsert per-build ConfigMap (Dockerfile + scripts)
//!           │    └─ create build Job (buildctl)
//!           └─ flip projects.image_status to "building"
//! ```

pub mod build_job;
pub mod config;
pub mod controller;
pub mod reseed;
pub mod retention;
pub mod retention_preflight;
pub mod types;
pub mod watcher;

pub use build_job::{BuildSubject, LABEL_IMAGE_ID, LABEL_PROJECT_ID};
pub use config::ImageControllerConfig;
pub use controller::{
    ImageController, ImageControllerError, format_catalog_image_tag, format_image_tag,
};
pub use reseed::{ReseedStats, reseed_empty_configs, reseed_empty_configs_arc};
pub use retention::{
    DeletedTag, RetainedTag, RetentionPlan, RetentionPolicy, SafePin, SafeReason, SelectedImage,
    UnsafeImage, ZotRepository, ZotTag,
};
pub use retention_preflight::{
    PreflightError, PreflightMode, PreflightOutcome, PreflightOutcomeKind,
    RetentionPreflightConfig, ZotHttpAuth, ZotHttpConfig, ZotHttpStateSource, ZotStateError,
    ZotStateSource, adapt_selected_images, run_preflight,
};
pub use types::{BuildRequest, BuildStatus, ProjectImageView};
pub use watcher::ImageBuildWatcher;
