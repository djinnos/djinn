//! Per-project devcontainer image controller — Phase 3 PR 5.
//!
//! Owns the reconcile loop that hashes a project's committed
//! `.devcontainer/devcontainer.json` (+ optional `devcontainer-lock.json`)
//! on every mirror-fetch tick and, when the hash changes, enqueues a
//! Kubernetes build Job that drives `@devcontainers/cli` against the
//! in-cluster BuildKit daemon and pushes the resulting image to Zot.
//!
//! **In-process placement.** Phase 3 keeps the controller inside the
//! `djinn-server` process (§5.5 of `phase3-devcontainer-and-warming.md`) —
//! the controller is a plain `Arc<ImageController>` that
//! [`mirror_fetcher::fetch_one`] calls at the tail of every successful
//! fetch. No separate Pod, no leader election, no watch loop. The
//! standalone `image-controller-deployment.yaml` stub that PR 4 shipped is
//! deleted in this PR; the ServiceAccount + RBAC for build-Job creation
//! remain for a future dedicated reconciler.
//!
//! **Scope of this PR.** The controller fires the Job and returns. It
//! does not yet watch Job completion — that arrives in a follow-up
//! (plan §5.5 step 6/7). Status flips to `"building"` on enqueue;
//! a later reconcile loop will observe the Job's terminal state and
//! flip the column to `"ready"` / `"failed"`.
//!
//! ```text
//! mirror_fetcher::fetch_one
//!     └─> ImageController::enqueue(project_id, &stack)
//!           ├─ read bare mirror via git2
//!           ├─ sha256(devcontainer.json [+ devcontainer-lock.json])
//!           ├─ compare to projects.image_hash
//!           ├─ if changed: acquire semaphore + in-flight guard
//!           │    └─ create build Job in namespace
//!           └─ flip projects.image_status to "building"
//! ```

pub mod build_job;
pub mod config;
pub mod controller;
pub mod hash;
pub mod types;
pub mod watcher;

pub use config::ImageControllerConfig;
pub use controller::ImageController;
pub use types::{BuildRequest, BuildStatus, ProjectImageView};
pub use watcher::ImageBuildWatcher;
