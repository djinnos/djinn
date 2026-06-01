pub mod auth_context;
pub mod commands;
pub mod error;
pub mod events;
pub mod index_tree;
pub mod liveness;
pub mod message;
pub mod models;
pub mod paths;
pub mod run_progress;

/// Sentinel `users.github_id` for the synthetic "automation" service user that
/// owns system-initiated work (board patrols, wave-planning, escalations,
/// future cron). Real GitHub account ids are ≥ 1, so `0` never collides with a
/// human and never appears in a GitHub org roster — keeping the org-membership
/// sync a no-op for it. The row is the deployment's automation identity: every
/// per-user dispatch axis (credential, model priority, fleet, future spend)
/// resolves through it like any other `users.id`.
pub const AUTOMATION_GITHUB_ID: i64 = 0;

/// `users.github_login` for the automation service user (see
/// [`AUTOMATION_GITHUB_ID`]).
pub const AUTOMATION_LOGIN: &str = "automation";

/// Display name for the automation service user.
pub const AUTOMATION_NAME: &str = "Automation";
