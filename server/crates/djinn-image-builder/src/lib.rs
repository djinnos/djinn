//! `djinn-image-builder` — generate a per-project Dockerfile + installer
//! script bundle from an [`EnvironmentConfig`], and compute the image
//! hash that invalidates the cached build.
//!
//! This crate is the replacement for the `@devcontainers/cli` build
//! path. It ships as pure functions in P3 with no consumers; P5 wires
//! it into `djinn-image-controller`'s build Job.
//!
//! ## API
//!
//! * [`generate_dockerfile`] — given an [`EnvironmentConfig`], emit the
//!   Dockerfile string + the script bundle that belongs alongside it in
//!   the builder Pod's ConfigMap.
//! * [`compute_environment_hash`] — sha256 over the canonicalised
//!   config JSON + the script bundle digest + the worker binary digest.
//!   Any change to any of those nulls the cached image hash and
//!   re-triggers a build.
//!
//! ## Multi-toolchain
//!
//! Language installers accept a space-separated `TOOLCHAINS` env var
//! (or `NODE_VERSIONS`, `PYTHON_VERSIONS`, ...) and loop over each
//! entry. That's how the plan's motivating case — two Rust workspaces
//! pinned to different channels — becomes a single `RUN` line in the
//! generated Dockerfile.
//!
//! ## Script bundle
//!
//! Scripts are embedded via `include_str!` at compile time from
//! `scripts/`. The [`SCRIPTS`] constant carries `(filename, body)`
//! tuples. [`SCRIPT_BUNDLE_SHA`] is computed at build time via
//! `compute_script_bundle_sha` so hash invalidation picks up any edit
//! under `scripts/` even without a version bump.

pub mod dockerfile;
pub mod hash;
pub mod scripts;

pub use dockerfile::{AgentWorkerImage, BuildContext, DockerfileError, generate_dockerfile};
pub use hash::{compute_environment_hash, compute_script_bundle_sha};
pub use scripts::{ScriptFile, SCRIPTS};
