//! djinn/ namespace sync — pluggable channel-based git sync.
//!
//! The `SyncManager` owns a registry of named channels. Each channel maps to a
//! `djinn/*` git branch. Channels export data from the local DB, commit it to
//! their branch, and push. Import pulls and merges peer data.
//!
//! For v1 only the `tasks` channel (`djinn/tasks`) is registered. Future
//! channels (`djinn/memory`, `djinn/settings`) plug in without changing the
//! sync infrastructure: add a `ChannelDef` to `REGISTERED_CHANNELS` and add a
//! match arm in `export_all`/`import_all`.
//!
//! Per-channel state (enabled flag, project path) is persisted in the
//! `settings` table using namespaced keys: `sync.{channel}.{field}`.
//! In-memory state (backoff, last sync time) is held in `SyncManager`.
//!
//! SYNC-01: SyncManager + pluggable channel registration
//! SYNC-02: Fetch-rebase-push per channel; LWW on updated_at
//! SYNC-03: Per-channel backoff (30s → 15min exponential)
//! SYNC-04: Enable/disable per-machine (DB flag) or team-wide (remote delete)
//! SYNC-05: Channel failure isolation — one channel failing doesn't block others

pub mod backoff;
pub mod tasks_channel;

mod helpers;
mod manager;
#[cfg(test)]
mod tests;
mod types;

pub use helpers::now_utc;
pub use manager::SyncManager;
pub use tasks_channel::TaskSyncError;
pub use types::{ChannelDef, ChannelStatus, REGISTERED_CHANNELS, SyncResult};
