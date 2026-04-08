use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, broadcast};
use tokio_util::sync::CancellationToken;

use crate::events::DjinnEventEnvelope;

use super::backoff::BackoffState;
use super::helpers::{now_utc, project_repo};
use super::tasks_channel::{self, TaskSyncError};
use super::types::{ChannelStatus, REGISTERED_CHANNELS, SyncResult};
use djinn_db::Database;

#[derive(Debug, Default)]
pub(super) struct ChannelState {
    pub(super) backoff: BackoffState,
    pub(super) last_synced_at: Option<String>,
    pub(super) last_error: Option<String>,
}

pub(super) struct Inner {
    pub(super) states: Mutex<HashMap<String, ChannelState>>,
    pub(super) db: Database,
    pub(super) events_tx: broadcast::Sender<DjinnEventEnvelope>,
}

/// Cheap-to-clone handle to the sync manager. Held in `AppState`.
#[derive(Clone)]
pub struct SyncManager {
    inner: Arc<Inner>,
}

impl SyncManager {
    pub fn new(db: Database, events_tx: broadcast::Sender<DjinnEventEnvelope>) -> Self {
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

    /// Restore is now a no-op — sync-enabled state lives in the `projects` table (SYNC-07).
    pub async fn restore(&self) {
        // Previously read sync.{channel}.project from settings into ChannelState.
        // With SYNC-07, sync-enabled projects are queried from the projects table
        // at export/import time, so no in-memory state needs restoring.
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
            // Auto-import every 60s using two-phase pull (SYNC-09).
            let mut import_interval = tokio::time::interval(Duration::from_secs(60));

            // Skip the first tick (fires immediately on creation).
            debounce.tick().await;
            fallback.tick().await;
            import_interval.tick().await;

            loop {
                tokio::select! {
                    result = events_rx.recv() => {
                        match result {
                            Ok(envelope) => {
                                if envelope.entity_type == "task" && !envelope.from_sync {
                                    pending = true;
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                // We missed some events; schedule a sync anyway.
                                pending = true;
                            }
                            Err(broadcast::error::RecvError::Closed) => break,
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
                    _ = import_interval.tick() => {
                        // Auto-import using two-phase pull (SYNC-09).
                        // Two-phase pull (SYNC-08) makes idle cycles ~50ms.
                        // Events emitted with from_sync=true won't trigger re-export (SYNC-06).
                        mgr.import_all().await;
                    }
                    _ = cancel.cancelled() => break,
                }
            }
        });
    }

    /// Enable sync for a project by setting `sync_enabled=true` in the projects table (SYNC-07).
    pub async fn enable_project(&self, project_id: &str) -> crate::error::Result<()> {
        let project_repo = project_repo(&self.inner);
        project_repo
            .update_config_field(project_id, "sync_enabled", "true")
            .await?;

        // Reset backoff on enable.
        let mut states = self.inner.states.lock().await;
        for st in states.values_mut() {
            st.backoff = BackoffState::new();
            st.last_error = None;
        }

        Ok(())
    }

    /// Disable sync for a project by setting `sync_enabled=false` in the projects table (SYNC-07).
    pub async fn disable_project(&self, project_id: &str) -> crate::error::Result<()> {
        let project_repo = project_repo(&self.inner);
        project_repo
            .update_config_field(project_id, "sync_enabled", "false")
            .await?;

        // Reset backoff.
        let mut states = self.inner.states.lock().await;
        for st in states.values_mut() {
            st.backoff = BackoffState::new();
        }

        Ok(())
    }

    /// Delete the remote branch for a channel on a specific project (team-wide disable) (SYNC-04 + SYNC-07).
    pub async fn delete_remote_branch(
        &self,
        channel: &str,
        project_path: &Path,
    ) -> std::result::Result<(), TaskSyncError> {
        match channel {
            "tasks" => tasks_channel::delete_remote_branch(project_path).await,
            _ => Err(TaskSyncError::Database(format!(
                "unknown channel: {channel}"
            ))),
        }
    }

    /// Per-channel status snapshot for all registered channels (SYNC-01 + SYNC-07 + SYNC-16).
    pub async fn status(&self) -> Vec<ChannelStatus> {
        let mut out = Vec::new();

        let sync_projects = self.list_sync_enabled_projects().await;
        let project_paths: Vec<String> = sync_projects.iter().map(|p| p.path.clone()).collect();

        for def in REGISTERED_CHANNELS {
            let (last_synced_at, last_error, failure_count, backoff_secs, needs_attention) = {
                let states = self.inner.states.lock().await;
                let st = states.get(def.name);
                (
                    st.and_then(|s| s.last_synced_at.clone()),
                    st.and_then(|s| s.last_error.clone()),
                    st.map(|s| s.backoff.failure_count()).unwrap_or(0),
                    st.map(|s| s.backoff.delay_secs()).unwrap_or(0),
                    st.map(|s| s.backoff.needs_attention()).unwrap_or(false),
                )
            };

            out.push(ChannelStatus {
                name: def.name.to_string(),
                branch: def.branch.to_string(),
                enabled: !sync_projects.is_empty(),
                project_paths: project_paths.clone(),
                last_synced_at,
                last_error,
                failure_count,
                backoff_secs,
                needs_attention,
            });
        }
        out
    }

    /// Export all sync-enabled projects across all channels (SYNC-02 + SYNC-07).
    ///
    /// Each channel is independent — a failure in one doesn't stop others (SYNC-05).
    /// Within a channel, each project is exported independently.
    pub async fn export_all(&self, user_id: Option<&str>) -> Vec<SyncResult> {
        let uid = user_id.unwrap_or("local");
        let mut results = Vec::new();

        let sync_projects = self.list_sync_enabled_projects().await;
        if sync_projects.is_empty() {
            return results;
        }

        for def in REGISTERED_CHANNELS {
            for project in &sync_projects {
                let project_path = PathBuf::from(&project.path);

                let result = match def.name {
                    "tasks" => {
                        tasks_channel::export(
                            &project_path,
                            &project.id,
                            uid,
                            &self.inner.db,
                            &self.inner.events_tx,
                        )
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
                        let _ = self
                            .inner
                            .events_tx
                            .send(DjinnEventEnvelope::sync_completed(
                                def.name, "export", count, None,
                            ));
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
                        let _ = self
                            .inner
                            .events_tx
                            .send(DjinnEventEnvelope::sync_completed(
                                def.name,
                                "export",
                                0,
                                Some(e.to_string().as_str()),
                            ));
                        tracing::warn!(
                            channel = def.name,
                            project = project.name,
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
        }

        results
    }

    /// Import all sync-enabled projects across all channels (SYNC-02 + SYNC-07).
    ///
    /// Each channel is independent — a failure in one doesn't stop others (SYNC-05).
    /// Within a channel, each project is imported independently.
    pub async fn import_all(&self) -> Vec<SyncResult> {
        let mut results = Vec::new();

        let sync_projects = self.list_sync_enabled_projects().await;
        if sync_projects.is_empty() {
            return results;
        }

        for def in REGISTERED_CHANNELS {
            for project in &sync_projects {
                let project_path = PathBuf::from(&project.path);

                let result = match def.name {
                    "tasks" => {
                        tasks_channel::import(
                            &project_path,
                            &project.id,
                            &self.inner.db,
                            &self.inner.events_tx,
                        )
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
                                st.last_synced_at = Some(now_utc());
                                st.last_error = None;
                            }
                        }
                        let _ = self
                            .inner
                            .events_tx
                            .send(DjinnEventEnvelope::sync_completed(
                                def.name, "import", count, None,
                            ));
                        results.push(SyncResult {
                            channel: def.name.to_string(),
                            ok: true,
                            count: Some(count),
                            error: None,
                        });
                    }
                    Err(e) => {
                        tracing::warn!(
                            channel = def.name,
                            project = project.name,
                            error = %e,
                            "sync import failed"
                        );
                        {
                            let mut states = self.inner.states.lock().await;
                            if let Some(st) = states.get_mut(def.name) {
                                st.last_error = Some(e.to_string());
                            }
                        }
                        let _ = self
                            .inner
                            .events_tx
                            .send(DjinnEventEnvelope::sync_completed(
                                def.name,
                                "import",
                                0,
                                Some(e.to_string().as_str()),
                            ));
                        results.push(SyncResult {
                            channel: def.name.to_string(),
                            ok: false,
                            count: None,
                            error: Some(e.to_string()),
                        });
                    }
                }
            }
        }

        results
    }

    /// Query sync-enabled projects from the DB (SYNC-07).
    pub(crate) async fn list_sync_enabled_projects(&self) -> Vec<djinn_core::models::Project> {
        project_repo(&self.inner)
            .list_sync_enabled()
            .await
            .unwrap_or_default()
    }
}
