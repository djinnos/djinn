use serde::Serialize;

use super::tasks_channel;

/// Registration record for a sync channel.
///
/// To add a new channel:
///   1. Implement export/import functions (see `tasks_channel` for reference).
///   2. Add a `ChannelDef` entry to `REGISTERED_CHANNELS`.
///   3. Add match arms in `SyncManager::export_all` and `import_all`.
pub struct ChannelDef {
    pub name: &'static str,
    pub branch: &'static str,
}

/// All registered sync channels. Extend here to add new channels (SYNC-01).
pub const REGISTERED_CHANNELS: &[ChannelDef] = &[ChannelDef {
    name: "tasks",
    branch: tasks_channel::BRANCH,
}];

/// Status snapshot for one channel (serialised for MCP tool responses).
#[derive(Debug, Clone, Serialize)]
pub struct ChannelStatus {
    pub name: String,
    pub branch: String,
    pub enabled: bool,
    /// Sync-enabled project paths (SYNC-07).
    pub project_paths: Vec<String>,
    pub last_synced_at: Option<String>,
    pub last_error: Option<String>,
    pub failure_count: u32,
    /// Seconds to wait before the next retry (0 when not backing off).
    pub backoff_secs: u64,
    /// Whether the channel needs attention (3+ failures) (SYNC-16).
    pub needs_attention: bool,
}

/// Result of an export or import operation on a single channel (SYNC-05).
#[derive(Debug, Serialize)]
pub struct SyncResult {
    pub channel: String,
    pub ok: bool,
    /// Tasks exported / imported; `None` on error.
    pub count: Option<usize>,
    pub error: Option<String>,
}
