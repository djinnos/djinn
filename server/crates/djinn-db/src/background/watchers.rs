use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use djinn_core::events::DjinnEventEnvelope;
use notify_debouncer_mini::{DebouncedEventKind, Debouncer, new_debouncer};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::repositories::note::{NoteEmbeddingProvider, NoteRepository};
use crate::repositories::project::ProjectRepository;
use crate::Database;
use djinn_core::events::EventBus;

/// Build an `EventBus` that forwards emitted events into the given
/// broadcast sender. Repository writes inside the watcher (e.g. note
/// reindex) reuse the same broadcast channel the rest of the server
/// subscribes to (SSE, downstream listeners, etc.).
fn event_bus_for(tx: &tokio::sync::broadcast::Sender<DjinnEventEnvelope>) -> EventBus {
    let tx = tx.clone();
    EventBus::new(move |event| {
        let _ = tx.send(event);
    })
}

/// Debounce window — reindex fires this long after the last file change.
const DEBOUNCE: Duration = Duration::from_secs(2);

struct WatcherState {
    /// Per-project debounced watcher keyed by project path.
    watchers: HashMap<PathBuf, Debouncer<notify::RecommendedWatcher>>,
    db: Database,
    events_tx: tokio::sync::broadcast::Sender<DjinnEventEnvelope>,
    embedding_provider: Option<Arc<dyn NoteEmbeddingProvider>>,
}

/// Spawn a background task that watches `.djinn/` directories for all registered
/// projects and triggers `reindex_from_disk` when note files change.
///
/// Dynamically adds/removes watches when `ProjectCreated`/`ProjectDeleted` events fire.
///
/// `embedding_provider` is an optional embedding backend (typically
/// `EmbeddingService` from `djinn-provider`) used to populate note
/// embeddings on reindex; pass `None` to skip embedding generation.
pub fn spawn_kb_watchers(
    db: Database,
    events_tx: tokio::sync::broadcast::Sender<DjinnEventEnvelope>,
    cancel: CancellationToken,
    embedding_provider: Option<Arc<dyn NoteEmbeddingProvider>>,
) {
    let state = Arc::new(Mutex::new(WatcherState {
        watchers: HashMap::new(),
        db: db.clone(),
        events_tx: events_tx.clone(),
        embedding_provider,
    }));

    let state_clone = state.clone();
    tokio::spawn(async move {
        // Initial setup: watch all existing projects.
        {
            let project_repo = ProjectRepository::new(db.clone(), event_bus_for(&events_tx));
            match project_repo.list().await {
                Ok(projects) => {
                    let mut guard = state_clone.lock().await;
                    for project in projects {
                        let path =
                            djinn_core::paths::project_dir(&project.github_owner, &project.github_repo);
                        add_watch(&mut guard, &project.id, &path);
                    }
                    tracing::info!(count = guard.watchers.len(), "KB file watchers initialized");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to list projects for KB watcher setup");
                }
            }
        }

        // Listen for project lifecycle events to add/remove watches.
        let mut events_rx = events_tx.subscribe();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::debug!("KB file watchers shutting down");
                    break;
                }
                recv = events_rx.recv() => {
                    match recv {
                        Ok(envelope) => {
                            if envelope.entity_type == "project" && envelope.action == "created" {
                                let Some(project) = envelope.parse_payload::<djinn_core::models::Project>() else { continue; };
                                let mut guard = state_clone.lock().await;
                                let path =
                                    djinn_core::paths::project_dir(&project.github_owner, &project.github_repo);
                                add_watch(&mut guard, &project.id, &path);
                                tracing::info!(project = %project.slug(), "KB watcher added for new project");
                            } else if envelope.entity_type == "project" && envelope.action == "deleted" {
                            let Some(id) = envelope.id.clone() else { continue; };
                            let mut guard = state_clone.lock().await;
                            // Find and remove by scanning — we don't have path from the delete event.
                            // The watcher is dropped which stops watching.
                            let project_repo = ProjectRepository::new(guard.db.clone(), event_bus_for(&guard.events_tx));
                            // Project is already deleted, so we need to find which watcher to remove.
                            // We'll just try to remove any watcher whose path no longer has a project.
                            let current_projects: std::collections::HashSet<PathBuf> = match project_repo.list().await {
                                Ok(ps) => ps
                                    .into_iter()
                                    .map(|p| djinn_core::paths::project_dir(&p.github_owner, &p.github_repo))
                                    .collect(),
                                Err(_) => continue,
                            };
                            guard.watchers.retain(|path, _| current_projects.contains(path));
                            tracing::info!(project_id = %id, "KB watcher removed for deleted project");
                        }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::debug!(skipped = n, "KB watcher event listener lagged");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
    });
}

fn add_watch(state: &mut WatcherState, project_id: &str, project_path: &Path) {
    let djinn_dir = project_path.join(".djinn");
    if !djinn_dir.exists() {
        tracing::debug!(path = %djinn_dir.display(), "skipping KB watch — .djinn/ does not exist");
        return;
    }

    // Already watching this project.
    if state.watchers.contains_key(project_path) {
        return;
    }

    let db = state.db.clone();
    let events_tx = event_bus_for(&state.events_tx);
    let embedding_provider = state.embedding_provider.clone();
    let project_id = project_id.to_string();
    let project_path_owned = project_path.to_path_buf();
    // Capture the runtime handle here (we're in async context); the debouncer
    // callback runs on notify's own thread which has no Tokio reactor.
    let rt_handle = tokio::runtime::Handle::current();

    // Per-project single-flight gate. Notify-debouncer fires one callback
    // per 2s burst, but while a reindex is in flight (especially on Dolt,
    // where a large reindex can take tens of seconds), the NEXT burst will
    // fire a second callback and spawn a second reindex. Those racing
    // reindexes read the same "existing notes" snapshot and both decide to
    // INSERT for a freshly-created file, which triggers the Dolt
    // unique-constraint conflict at COMMIT time (up to 90s rollback per
    // failed commit — pool starvation). The upsert in `insert_index_entry`
    // handles the race correctly; this flag also prevents doing the
    // redundant work in the first place. The pending-flag lets one
    // rerun queue after the current one, so file changes that arrived
    // mid-scan aren't lost.
    let running = Arc::new(AtomicBool::new(false));
    let pending = Arc::new(AtomicBool::new(false));

    let debouncer = new_debouncer(
        DEBOUNCE,
        move |res: Result<Vec<notify_debouncer_mini::DebouncedEvent>, notify::Error>| {
            match res {
                Ok(events) => {
                    // Only reindex if at least one .md file was affected.
                    let has_md = events.iter().any(|e| {
                        e.kind == DebouncedEventKind::Any
                        && e.path.extension().and_then(|ext| ext.to_str()) == Some("md")
                        // Skip worktrees subdirectory
                        && !path_contains_segment(&e.path, "worktrees")
                    });
                    if !has_md {
                        return;
                    }

                    // If a reindex is already running for this project,
                    // mark that another run is needed and bail — the
                    // in-flight task will re-run before exiting.
                    if running
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_err()
                    {
                        pending.store(true, Ordering::Release);
                        return;
                    }

                    let db = db.clone();
                    let events_tx = events_tx.clone();
                    let project_id = project_id.clone();
                    let project_path = project_path_owned.clone();
                    let embedding_provider = embedding_provider.clone();
                    let running = running.clone();
                    let pending = pending.clone();

                    // Spawn reindex on the captured runtime handle — safe to call
                    // from non-Tokio threads (notify's debouncer thread).
                    rt_handle.spawn(async move {
                        loop {
                            let note_repo = NoteRepository::new(db.clone(), events_tx.clone())
                                .with_embedding_provider(embedding_provider.clone());
                            match note_repo
                                .reindex_from_disk(&project_id, &project_path)
                                .await
                            {
                                Ok(summary) => {
                                    if summary.created > 0
                                        || summary.updated > 0
                                        || summary.deleted > 0
                                    {
                                        tracing::info!(
                                            project = %project_path.display(),
                                            created = summary.created,
                                            updated = summary.updated,
                                            deleted = summary.deleted,
                                            "KB watcher triggered reindex"
                                        );
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        project = %project_path.display(),
                                        error = %e,
                                        "KB watcher reindex failed"
                                    );
                                }
                            }

                            // If a burst arrived while we were running,
                            // drain the pending flag and loop. Otherwise
                            // drop the running flag and exit. Checking
                            // the flag *before* clearing `running` would
                            // race with the callback; the double-check
                            // after release-store is the canonical
                            // pattern for this producer/consumer setup.
                            if pending.swap(false, Ordering::AcqRel) {
                                continue;
                            }
                            running.store(false, Ordering::Release);
                            if pending.swap(false, Ordering::AcqRel)
                                && running
                                    .compare_exchange(
                                        false,
                                        true,
                                        Ordering::AcqRel,
                                        Ordering::Acquire,
                                    )
                                    .is_ok()
                            {
                                continue;
                            }
                            break;
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "KB file watcher error");
                }
            }
        },
    );

    match debouncer {
        Ok(mut debouncer) => {
            // Watch .djinn/ non-recursively for top-level note files (brief.md, etc.).
            if let Err(e) = debouncer
                .watcher()
                .watch(&djinn_dir, notify::RecursiveMode::NonRecursive)
            {
                tracing::warn!(
                    path = %djinn_dir.display(),
                    error = %e,
                    "failed to start KB file watch"
                );
                return;
            }

            // Watch each subdirectory recursively EXCEPT directories that don't
            // contain notes.  On macOS, kqueue opens a fd per watched file, so
            // watching `worktrees/` (full source trees) exhausts the fd table
            // and breaks all process spawning with EBADF.
            const SKIP_DIRS: &[&str] = &["worktrees", "logs", "tasks"];
            if let Ok(entries) = std::fs::read_dir(&djinn_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if !path.is_dir() {
                        continue;
                    }
                    let name = entry.file_name();
                    if SKIP_DIRS.iter().any(|s| name == *s) {
                        continue;
                    }
                    if let Err(e) = debouncer
                        .watcher()
                        .watch(&path, notify::RecursiveMode::Recursive)
                    {
                        tracing::warn!(
                            path = %path.display(),
                            error = %e,
                            "failed to watch KB subdirectory"
                        );
                    }
                }
            }

            state.watchers.insert(project_path.to_path_buf(), debouncer);
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to create KB file watcher");
        }
    }
}

/// Check if any component of the path matches the given segment name.
fn path_contains_segment(path: &Path, segment: &str) -> bool {
    path.components().any(|c| c.as_os_str() == segment)
}

#[cfg(test)]
mod tests {
    use tokio::sync::broadcast;

    use crate::database::test_tempdir;

    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn add_watch_skips_missing_djinn_and_deduplicates_existing_watch() {
        let db = Database::open_in_memory().unwrap();
        let (events_tx, _rx) = broadcast::channel(16);
        let mut state = WatcherState {
            watchers: HashMap::new(),
            db,
            events_tx,
            embedding_provider: None,
        };

        let missing = test_tempdir().unwrap();
        add_watch(&mut state, "project-1", missing.path());
        assert!(state.watchers.is_empty());

        let project = test_tempdir().unwrap();
        std::fs::create_dir_all(project.path().join(".djinn/research")).unwrap();

        add_watch(&mut state, "project-1", project.path());
        assert_eq!(state.watchers.len(), 1);

        add_watch(&mut state, "project-1", project.path());
        assert_eq!(state.watchers.len(), 1);
        assert!(state.watchers.contains_key(project.path()));
    }
}
