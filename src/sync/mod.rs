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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{Mutex, broadcast};
use tokio_util::sync::CancellationToken;

use crate::db::connection::Database;
use crate::db::repositories::settings::SettingsRepository;
use crate::events::DjinnEvent;
use backoff::BackoffState;
pub use tasks_channel::TaskSyncError;

// ── Channel registration ──────────────────────────────────────────────────────

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

// ── Per-channel in-memory state ───────────────────────────────────────────────

#[derive(Debug, Default)]
struct ChannelState {
    project_path: Option<PathBuf>,
    backoff: BackoffState,
    last_synced_at: Option<String>,
    last_error: Option<String>,
}

// ── Public types ──────────────────────────────────────────────────────────────

/// Status snapshot for one channel (serialised for MCP tool responses).
#[derive(Debug, Clone, Serialize)]
pub struct ChannelStatus {
    pub name: String,
    pub branch: String,
    pub enabled: bool,
    pub project_path: Option<String>,
    pub last_synced_at: Option<String>,
    pub last_error: Option<String>,
    pub failure_count: u32,
    /// Seconds to wait before the next retry (0 when not backing off).
    pub backoff_secs: u64,
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

// ── SyncManager ───────────────────────────────────────────────────────────────

struct Inner {
    states: Mutex<HashMap<String, ChannelState>>,
    db: Database,
    events_tx: broadcast::Sender<DjinnEvent>,
}

/// Cheap-to-clone handle to the sync manager. Held in `AppState`.
#[derive(Clone)]
pub struct SyncManager {
    inner: Arc<Inner>,
}

impl SyncManager {
    pub fn new(db: Database, events_tx: broadcast::Sender<DjinnEvent>) -> Self {
        let mut states = HashMap::new();
        for ch in REGISTERED_CHANNELS {
            states.insert(ch.name.to_string(), ChannelState::default());
        }
        Self {
            inner: Arc::new(Inner {
                states: Mutex::new(states),
                db,
                events_tx,
            }),
        }
    }

    /// Restore persisted project paths from DB settings into in-memory state.
    ///
    /// Call once after the DB is ready (e.g. in `AppState::initialize()`).
    pub async fn restore(&self) {
        let repo = settings_repo(&self.inner);
        let mut states = self.inner.states.lock().await;
        for ch in REGISTERED_CHANNELS {
            let key = format!("sync.{}.project", ch.name);
            if let Ok(Some(s)) = repo.get(&key).await
                && let Some(st) = states.get_mut(ch.name) {
                    st.project_path = Some(PathBuf::from(s.value));
                }
        }
    }

    /// Spawn a background task that auto-exports on task mutations.
    ///
    /// Debounce: triggers export 10s after the last task mutation event.
    /// Fallback: triggers unconditionally every 5 minutes.
    ///
    /// Each channel runs independently — one channel's failure won't affect
    /// others (SYNC-05).
    pub fn spawn_background_task(&self, cancel: CancellationToken, user_id: String) {
        let mgr = self.clone();
        tokio::spawn(async move {
            let mut events_rx = mgr.inner.events_tx.subscribe();
            let mut pending = false;
            let mut debounce = tokio::time::interval(Duration::from_secs(10));
            let mut fallback = tokio::time::interval(Duration::from_secs(300));

            // Skip the first tick (fires immediately on creation).
            debounce.tick().await;
            fallback.tick().await;

            loop {
                tokio::select! {
                    result = events_rx.recv() => {
                        match result {
                            Ok(
                                DjinnEvent::TaskCreated(_)
                                | DjinnEvent::TaskUpdated(_)
                                | DjinnEvent::TaskDeleted { .. },
                            ) => {
                                pending = true;
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                // We missed some events; schedule a sync anyway.
                                pending = true;
                            }
                            Err(broadcast::error::RecvError::Closed) => break,
                            _ => {}
                        }
                    }
                    _ = debounce.tick() => {
                        if pending {
                            pending = false;
                            mgr.export_all(Some(&user_id)).await;
                        }
                    }
                    _ = fallback.tick() => {
                        mgr.export_all(Some(&user_id)).await;
                    }
                    _ = cancel.cancelled() => break,
                }
            }
        });
    }

    // ── Public API ────────────────────────────────────────────────────────────

    /// Enable a channel and store the project path in DB settings (SYNC-04).
    pub async fn enable(&self, channel: &str, project: &Path) -> crate::error::Result<()> {
        let repo = settings_repo(&self.inner);
        repo.set(&format!("sync.{channel}.enabled"), "true").await?;
        repo.set(
            &format!("sync.{channel}.project"),
            project.to_str().unwrap_or_default(),
        )
        .await?;

        // Update in-memory state.
        let mut states = self.inner.states.lock().await;
        if let Some(st) = states.get_mut(channel) {
            st.project_path = Some(project.to_path_buf());
            st.backoff = BackoffState::new();
            st.last_error = None;
        }

        Ok(())
    }

    /// Disable a channel for this machine — clears the local enabled flag (SYNC-04).
    pub async fn disable(&self, channel: &str) -> crate::error::Result<()> {
        settings_repo(&self.inner)
            .set(&format!("sync.{channel}.enabled"), "false")
            .await?;

        let mut states = self.inner.states.lock().await;
        if let Some(st) = states.get_mut(channel) {
            st.project_path = None;
            st.backoff = BackoffState::new();
        }

        Ok(())
    }

    /// Delete the remote branch for a channel (team-wide disable) (SYNC-04).
    pub async fn delete_remote_branch(
        &self,
        channel: &str,
    ) -> std::result::Result<(), TaskSyncError> {
        let project = self.project_from_db(channel).await;
        let Some(project) = project else {
            return Err(TaskSyncError::Database("no project path configured".into()));
        };
        match channel {
            "tasks" => tasks_channel::delete_remote_branch(&project).await,
            _ => Err(TaskSyncError::Database(format!(
                "unknown channel: {channel}"
            ))),
        }
    }

    /// Per-channel status snapshot for all registered channels (SYNC-01).
    pub async fn status(&self) -> Vec<ChannelStatus> {
        let mut out = Vec::new();
        for def in REGISTERED_CHANNELS {
            // Read in-memory state without holding the lock across awaits.
            let (project_path, last_synced_at, last_error, failure_count, backoff_secs) = {
                let states = self.inner.states.lock().await;
                let st = states.get(def.name);
                (
                    st.and_then(|s| s.project_path.as_ref())
                        .map(|p| p.to_string_lossy().into_owned()),
                    st.and_then(|s| s.last_synced_at.clone()),
                    st.and_then(|s| s.last_error.clone()),
                    st.map(|s| s.backoff.failure_count()).unwrap_or(0),
                    st.map(|s| s.backoff.delay_secs()).unwrap_or(0),
                )
            }; // Lock released here.

            let enabled = self.is_enabled(def.name).await;

            out.push(ChannelStatus {
                name: def.name.to_string(),
                branch: def.branch.to_string(),
                enabled,
                project_path,
                last_synced_at,
                last_error,
                failure_count,
                backoff_secs,
            });
        }
        out
    }

    /// Export all enabled channels (SYNC-02).
    ///
    /// Each channel is independent — a failure in one doesn't stop others (SYNC-05).
    pub async fn export_all(&self, user_id: Option<&str>) -> Vec<SyncResult> {
        let uid = user_id.unwrap_or("local");
        let mut results = Vec::new();

        for def in REGISTERED_CHANNELS {
            if !self.is_enabled(def.name).await {
                continue;
            }

            let project = self.project_from_memory(def.name).await;
            let Some(project) = project else {
                results.push(SyncResult {
                    channel: def.name.to_string(),
                    ok: false,
                    count: None,
                    error: Some("no project path configured".into()),
                });
                continue;
            };

            let result = match def.name {
                "tasks" => {
                    tasks_channel::export(&project, uid, &self.inner.db, &self.inner.events_tx)
                        .await
                }
                _ => Err(TaskSyncError::Database(format!(
                    "unknown channel: {}",
                    def.name
                ))),
            };

            match result {
                Ok(count) => {
                    {
                        let mut states = self.inner.states.lock().await;
                        if let Some(st) = states.get_mut(def.name) {
                            st.backoff.record_success();
                            st.last_synced_at = Some(now_utc());
                            st.last_error = None;
                        }
                    }
                    results.push(SyncResult {
                        channel: def.name.to_string(),
                        ok: true,
                        count: Some(count),
                        error: None,
                    });
                }
                Err(e) => {
                    let delay = {
                        let mut states = self.inner.states.lock().await;
                        states
                            .get_mut(def.name)
                            .map(|st| {
                                st.last_error = Some(e.to_string());
                                st.backoff.record_failure()
                            })
                            .unwrap_or(Duration::ZERO)
                    };
                    tracing::warn!(
                        channel = def.name,
                        error = %e,
                        backoff_secs = delay.as_secs(),
                        "sync export failed"
                    );
                    results.push(SyncResult {
                        channel: def.name.to_string(),
                        ok: false,
                        count: None,
                        error: Some(e.to_string()),
                    });
                }
            }
        }

        results
    }

    /// Import all enabled channels (SYNC-02).
    ///
    /// Each channel is independent — a failure in one doesn't stop others (SYNC-05).
    pub async fn import_all(&self) -> Vec<SyncResult> {
        let mut results = Vec::new();

        for def in REGISTERED_CHANNELS {
            if !self.is_enabled(def.name).await {
                continue;
            }

            let project = self.project_from_memory(def.name).await;
            let Some(project) = project else {
                results.push(SyncResult {
                    channel: def.name.to_string(),
                    ok: false,
                    count: None,
                    error: Some("no project path configured".into()),
                });
                continue;
            };

            let result = match def.name {
                "tasks" => {
                    tasks_channel::import(&project, &self.inner.db, &self.inner.events_tx).await
                }
                _ => Err(TaskSyncError::Database(format!(
                    "unknown channel: {}",
                    def.name
                ))),
            };

            match result {
                Ok(count) => {
                    {
                        let mut states = self.inner.states.lock().await;
                        if let Some(st) = states.get_mut(def.name) {
                            st.last_synced_at = Some(now_utc());
                            st.last_error = None;
                        }
                    }
                    results.push(SyncResult {
                        channel: def.name.to_string(),
                        ok: true,
                        count: Some(count),
                        error: None,
                    });
                }
                Err(e) => {
                    tracing::warn!(channel = def.name, error = %e, "sync import failed");
                    {
                        let mut states = self.inner.states.lock().await;
                        if let Some(st) = states.get_mut(def.name) {
                            st.last_error = Some(e.to_string());
                        }
                    }
                    results.push(SyncResult {
                        channel: def.name.to_string(),
                        ok: false,
                        count: None,
                        error: Some(e.to_string()),
                    });
                }
            }
        }

        results
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    /// Check if a channel is enabled (reads from DB settings).
    async fn is_enabled(&self, channel: &str) -> bool {
        settings_repo(&self.inner)
            .get(&format!("sync.{channel}.enabled"))
            .await
            .ok()
            .flatten()
            .map(|s| s.value == "true")
            .unwrap_or(false)
    }

    /// Get project path from in-memory state (set by `enable()` or `restore()`).
    async fn project_from_memory(&self, channel: &str) -> Option<PathBuf> {
        self.inner
            .states
            .lock()
            .await
            .get(channel)?
            .project_path
            .clone()
    }

    /// Get project path from DB settings (for operations that don't need
    /// in-memory state to be current, e.g. `delete_remote_branch`).
    async fn project_from_db(&self, channel: &str) -> Option<PathBuf> {
        settings_repo(&self.inner)
            .get(&format!("sync.{channel}.project"))
            .await
            .ok()
            .flatten()
            .map(|s| PathBuf::from(s.value))
    }
}

// ── Module-level helpers ──────────────────────────────────────────────────────

fn settings_repo(inner: &Inner) -> SettingsRepository {
    SettingsRepository::new(inner.db.clone(), inner.events_tx.clone())
}

/// Current UTC time as an ISO-8601 string (second precision).
fn now_utc() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (y, mo, d, h, m, s) = unix_to_ymd_hms(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

fn unix_to_ymd_hms(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let s = (secs % 60) as u32;
    let m = ((secs / 60) % 60) as u32;
    let h = ((secs / 3600) % 24) as u32;
    let mut days = secs / 86400;

    let mut y = 1970u32;
    loop {
        let days_in_year: u64 = if y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400)) {
            366
        } else {
            365
        };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        y += 1;
    }

    let leap = y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400));
    let month_days: [u32; 12] = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut mo = 1u32;
    let mut d = days as u32;
    for &dim in &month_days {
        if d < dim {
            d += 1;
            break;
        }
        d -= dim;
        mo += 1;
    }

    (y, mo, d, h, m, s)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_channels_has_tasks() {
        assert!(!REGISTERED_CHANNELS.is_empty());
        assert_eq!(REGISTERED_CHANNELS[0].name, "tasks");
        assert_eq!(REGISTERED_CHANNELS[0].branch, "djinn/tasks");
    }

    #[tokio::test]
    async fn new_manager_has_all_channels() {
        let db = crate::test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(16);
        let mgr = SyncManager::new(db, tx);
        let status = mgr.status().await;
        assert_eq!(status.len(), REGISTERED_CHANNELS.len());
    }

    #[tokio::test]
    async fn enable_persists_flag_to_db() {
        let db = crate::test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(16);
        let mgr = SyncManager::new(db, tx);
        let project = PathBuf::from("/tmp/test-project");
        mgr.enable("tasks", &project).await.unwrap();
        assert!(mgr.is_enabled("tasks").await);
    }

    #[tokio::test]
    async fn disable_clears_flag() {
        let db = crate::test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(16);
        let mgr = SyncManager::new(db, tx);
        let project = PathBuf::from("/tmp/test-project");
        mgr.enable("tasks", &project).await.unwrap();
        mgr.disable("tasks").await.unwrap();
        assert!(!mgr.is_enabled("tasks").await);
    }

    #[tokio::test]
    async fn status_shows_enabled_and_project() {
        let db = crate::test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(16);
        let mgr = SyncManager::new(db, tx);
        let project = PathBuf::from("/tmp/my-repo");
        mgr.enable("tasks", &project).await.unwrap();

        let statuses = mgr.status().await;
        let ch = statuses.iter().find(|s| s.name == "tasks").unwrap();
        assert!(ch.enabled);
        assert_eq!(ch.project_path.as_deref(), Some("/tmp/my-repo"));
    }

    #[tokio::test]
    async fn export_all_skips_when_disabled() {
        let db = crate::test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(16);
        let mgr = SyncManager::new(db, tx);
        // Not enabled — export_all should return an empty vec.
        let results = mgr.export_all(Some("user1")).await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn import_all_skips_when_disabled() {
        let db = crate::test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(16);
        let mgr = SyncManager::new(db, tx);
        let results = mgr.import_all().await;
        assert!(results.is_empty());
    }

    #[test]
    fn now_utc_is_reasonable() {
        let s = now_utc();
        assert!(s.starts_with("20"), "timestamp should start with '20': {s}");
        assert!(s.ends_with('Z'), "timestamp should end with 'Z': {s}");
        assert_eq!(s.len(), 20, "timestamp should be 20 chars: {s}");
    }
}
