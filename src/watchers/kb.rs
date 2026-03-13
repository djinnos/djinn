use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify_debouncer_mini::{DebouncedEventKind, Debouncer, new_debouncer};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::db::NoteRepository;
use crate::db::ProjectRepository;
use crate::db::connection::Database;
use crate::events::{DjinnEvent, DjinnEventEnvelope};

/// Debounce window — reindex fires this long after the last file change.
const DEBOUNCE: Duration = Duration::from_secs(2);

struct WatcherState {
    /// Per-project debounced watcher keyed by project path.
    watchers: HashMap<PathBuf, Debouncer<notify::RecommendedWatcher>>,
    db: Database,
    events_tx: tokio::sync::broadcast::Sender<DjinnEvent>,
}

/// Spawn a background task that watches `.djinn/` directories for all registered
/// projects and triggers `reindex_from_disk` when note files change.
///
/// Dynamically adds/removes watches when `ProjectCreated`/`ProjectDeleted` events fire.
pub fn spawn_kb_watchers(
    db: Database,
    events_tx: tokio::sync::broadcast::Sender<DjinnEvent>,
    cancel: CancellationToken,
) {
    let state = Arc::new(Mutex::new(WatcherState {
        watchers: HashMap::new(),
        db: db.clone(),
        events_tx: events_tx.clone(),
    }));

    let state_clone = state.clone();
    tokio::spawn(async move {
        // Initial setup: watch all existing projects.
        {
            let project_repo = ProjectRepository::new(db.clone(), events_tx.clone());
            match project_repo.list().await {
                Ok(projects) => {
                    let mut guard = state_clone.lock().await;
                    for project in projects {
                        let path = PathBuf::from(&project.path);
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
                        Ok(evt) => {
                            let envelope: DjinnEventEnvelope = evt.into();
                            if envelope.entity_type == "project" && envelope.action == "created" {
                                let Some(project) = envelope.parse_payload::<crate::models::Project>() else { continue; };
                                let mut guard = state_clone.lock().await;
                                let path = PathBuf::from(&project.path);
                                add_watch(&mut guard, &project.id, &path);
                                tracing::info!(project = %project.path, "KB watcher added for new project");
                            } else if envelope.entity_type == "project" && envelope.action == "deleted" {
                            let Some(id) = envelope.id.clone() else { continue; };
                            let mut guard = state_clone.lock().await;
                            // Find and remove by scanning — we don't have path from the delete event.
                            // The watcher is dropped which stops watching.
                            let project_repo = ProjectRepository::new(guard.db.clone(), guard.events_tx.clone());
                            // Project is already deleted, so we need to find which watcher to remove.
                            // We'll just try to remove any watcher whose path no longer has a project.
                            let current_projects: std::collections::HashSet<PathBuf> = match project_repo.list().await {
                                Ok(ps) => ps.into_iter().map(|p| PathBuf::from(p.path)).collect(),
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
    let events_tx = state.events_tx.clone();
    let project_id = project_id.to_string();
    let project_path_owned = project_path.to_path_buf();

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

                    let db = db.clone();
                    let events_tx = events_tx.clone();
                    let project_id = project_id.clone();
                    let project_path = project_path_owned.clone();

                    // Spawn reindex on the tokio runtime.
                    tokio::spawn(async move {
                        let note_repo = NoteRepository::new(db, events_tx);
                        match note_repo
                            .reindex_from_disk(&project_id, &project_path)
                            .await
                        {
                            Ok(summary) => {
                                if summary.created > 0 || summary.updated > 0 || summary.deleted > 0
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
