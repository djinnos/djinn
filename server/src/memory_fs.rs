use std::collections::{BTreeMap, BTreeSet};

use djinn_core::models::NoteCompact;
use djinn_db::{
    NoteRepository, folder_for_type, normalize_virtual_note_path, permalink_from_virtual_note_path,
    render_note_markdown, virtual_note_path_for_permalink,
};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryEntryKind {
    Directory,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntryMetadata {
    pub path: String,
    pub kind: MemoryEntryKind,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryDirEntry {
    pub name: String,
    pub metadata: MemoryEntryMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryFile {
    pub metadata: MemoryEntryMetadata,
    pub content: String,
    pub note_id: String,
    pub permalink: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedNotePath {
    pub logical_path: String,
    pub permalink: String,
    pub note_id: String,
}

#[derive(Debug, Error)]
pub enum MemoryFsError {
    #[error("path not found: {path}")]
    NotFound { path: String },
    #[error("not a directory: {path}")]
    NotDirectory { path: String },
    #[error("not a file: {path}")]
    NotFile { path: String },
    #[error(transparent)]
    Repository(#[from] djinn_db::Error),
}

pub type Result<T> = std::result::Result<T, MemoryFsError>;

#[derive(Debug, Default)]
struct MemoryTree {
    directories: BTreeSet<String>,
    files: BTreeMap<String, NoteCompact>,
}

impl MemoryTree {
    fn parent_dir(path: &str) -> String {
        path.rsplit_once('/')
            .map(|(parent, _)| parent.to_string())
            .unwrap_or_default()
    }

    fn child_name(path: &str) -> String {
        path.rsplit('/').next().unwrap_or(path).to_string()
    }

    fn insert_dir_hierarchy(&mut self, path: &str) {
        self.directories.insert(String::new());
        if path.is_empty() {
            return;
        }

        let mut current = String::new();
        for segment in path.split('/') {
            if !current.is_empty() {
                current.push('/');
            }
            current.push_str(segment);
            self.directories.insert(current.clone());
        }
    }

    fn insert_note(&mut self, note: NoteCompact) {
        let file_path = virtual_note_path_for_permalink(&note.permalink);
        let parent = Self::parent_dir(&file_path);
        self.insert_dir_hierarchy(&parent);
        self.files.insert(file_path, note);
    }
}

pub struct MemoryFilesystemCore {
    repo: NoteRepository,
}

impl MemoryFilesystemCore {
    pub fn new(repo: NoteRepository) -> Self {
        Self { repo }
    }

    pub async fn resolve_note_path(
        &self,
        project_id: &str,
        path: &str,
    ) -> Result<ResolvedNotePath> {
        let normalized = normalize_virtual_note_path(path);
        let permalink = permalink_from_virtual_note_path(&normalized).ok_or_else(|| {
            MemoryFsError::NotFile {
                path: normalized.clone(),
            }
        })?;
        let note = self
            .repo
            .get_by_permalink(project_id, &permalink)
            .await?
            .ok_or_else(|| MemoryFsError::NotFound {
                path: normalized.clone(),
            })?;

        Ok(ResolvedNotePath {
            logical_path: normalized,
            permalink,
            note_id: note.id,
        })
    }

    pub async fn stat(&self, project_id: &str, path: &str) -> Result<MemoryEntryMetadata> {
        let normalized = normalize_virtual_note_path(path);
        if normalized.is_empty() {
            return Ok(MemoryEntryMetadata {
                path: String::new(),
                kind: MemoryEntryKind::Directory,
                size: 0,
            });
        }

        let tree = self.build_tree(project_id).await?;
        if tree.directories.contains(&normalized) {
            return Ok(MemoryEntryMetadata {
                path: normalized,
                kind: MemoryEntryKind::Directory,
                size: 0,
            });
        }

        if let Some(note) = tree.files.get(&normalized) {
            return Ok(self.file_metadata(note));
        }

        Err(MemoryFsError::NotFound { path: normalized })
    }

    pub async fn list_dir(&self, project_id: &str, path: &str) -> Result<Vec<MemoryDirEntry>> {
        let normalized = normalize_virtual_note_path(path);
        let tree = self.build_tree(project_id).await?;

        if !normalized.is_empty() && !tree.directories.contains(&normalized) {
            if tree.files.contains_key(&normalized) {
                return Err(MemoryFsError::NotDirectory { path: normalized });
            }
            return Err(MemoryFsError::NotFound { path: normalized });
        }

        let mut entries = BTreeMap::<String, MemoryDirEntry>::new();

        for dir in &tree.directories {
            if dir.is_empty() || dir == &normalized {
                continue;
            }
            if MemoryTree::parent_dir(dir) == normalized {
                let name = MemoryTree::child_name(dir);
                entries.insert(
                    name.clone(),
                    MemoryDirEntry {
                        name,
                        metadata: MemoryEntryMetadata {
                            path: dir.clone(),
                            kind: MemoryEntryKind::Directory,
                            size: 0,
                        },
                    },
                );
            }
        }

        for (file_path, note) in &tree.files {
            if MemoryTree::parent_dir(file_path) == normalized {
                let name = MemoryTree::child_name(file_path);
                entries.insert(
                    name.clone(),
                    MemoryDirEntry {
                        name,
                        metadata: self.file_metadata(note),
                    },
                );
            }
        }

        Ok(entries.into_values().collect())
    }

    pub async fn read_file(&self, project_id: &str, path: &str) -> Result<MemoryFile> {
        let resolved = self.resolve_note_path(project_id, path).await?;
        let note = self
            .repo
            .get_by_permalink(project_id, &resolved.permalink)
            .await?
            .ok_or_else(|| MemoryFsError::NotFound {
                path: resolved.logical_path.clone(),
            })?;

        self.repo.touch_accessed(&note.id).await?;

        let content = render_note_markdown(&note.title, &note.note_type, &note.tags, &note.content);
        let metadata = MemoryEntryMetadata {
            path: resolved.logical_path.clone(),
            kind: MemoryEntryKind::File,
            size: content.len() as u64,
        };

        Ok(MemoryFile {
            metadata,
            content,
            note_id: note.id,
            permalink: note.permalink,
        })
    }

    async fn build_tree(&self, project_id: &str) -> Result<MemoryTree> {
        let notes = self.repo.list_compact(project_id, None, None, 0).await?;
        let mut tree = MemoryTree::default();
        tree.directories.insert(String::new());

        for folder in known_note_folders() {
            tree.insert_dir_hierarchy(&folder);
        }

        for note in notes {
            tree.insert_note(note);
        }

        Ok(tree)
    }

    fn file_metadata(&self, note: &NoteCompact) -> MemoryEntryMetadata {
        let path = virtual_note_path_for_permalink(&note.permalink);
        MemoryEntryMetadata {
            path,
            kind: MemoryEntryKind::File,
            size: 0,
        }
    }
}

fn known_note_folders() -> Vec<String> {
    let mut folders = BTreeSet::new();
    for note_type in [
        "adr",
        "proposed_adr",
        "pattern",
        "case",
        "pitfall",
        "research",
        "requirement",
        "reference",
        "design",
        "session",
        "persona",
        "journey",
        "design_spec",
        "competitive",
        "tech_spike",
        "repo_map",
        "brief",
        "roadmap",
    ] {
        let folder = folder_for_type(note_type);
        if !folder.is_empty() {
            folders.insert(folder.to_string());
        }
    }
    folders.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::events::EventBus;
    use djinn_db::Database;
    use djinn_db::test_support::make_project;

    async fn make_core() -> (MemoryFilesystemCore, Database, String, tempfile::TempDir) {
        let base = std::env::current_dir()
            .unwrap()
            .join("target")
            .join("test-tmp");
        std::fs::create_dir_all(&base).unwrap();
        let project_root = tempfile::Builder::new()
            .prefix("memory-fs-")
            .tempdir_in(&base)
            .unwrap();
        let db = Database::open_in_memory().unwrap();
        let project = make_project(&db, project_root.path()).await;
        let repo = NoteRepository::new(db.clone(), EventBus::noop());
        (
            MemoryFilesystemCore::new(repo),
            db,
            project.id,
            project_root,
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolves_note_paths_and_reads_content() {
        let (core, db, project_id, project_root) = make_core().await;
        let repo = NoteRepository::new(db.clone(), EventBus::noop());
        repo.create(
            &project_id,
            project_root.path(),
            "Reusable Flow",
            "Body text",
            "pattern",
            "[\"flow\"]",
        )
        .await
        .unwrap();

        let resolved = core
            .resolve_note_path(&project_id, "/patterns/reusable-flow.md")
            .await
            .unwrap();
        assert_eq!(resolved.permalink, "patterns/reusable-flow");

        let file = core
            .read_file(&project_id, "patterns/reusable-flow.md")
            .await
            .unwrap();
        assert_eq!(file.metadata.kind, MemoryEntryKind::File);
        assert!(file.content.contains("title: Reusable Flow"));
        assert!(file.content.contains("Body text"));

        let touched = repo
            .get_by_permalink(&project_id, "patterns/reusable-flow")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(touched.access_count, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stats_and_lists_directories() {
        let (core, db, project_id, project_root) = make_core().await;
        let repo = NoteRepository::new(db, EventBus::noop());
        repo.create(
            &project_id,
            project_root.path(),
            "Project Brief",
            "Overview",
            "brief",
            "[]",
        )
        .await
        .unwrap();
        repo.create(
            &project_id,
            project_root.path(),
            "Routing Guide",
            "Design body",
            "design_spec",
            "[]",
        )
        .await
        .unwrap();

        let root_stat = core.stat(&project_id, "").await.unwrap();
        assert_eq!(root_stat.kind, MemoryEntryKind::Directory);

        let design_stat = core.stat(&project_id, "design").await.unwrap();
        assert_eq!(design_stat.kind, MemoryEntryKind::Directory);

        let root_entries = core.list_dir(&project_id, "").await.unwrap();
        assert!(root_entries.iter().any(|entry| entry.name == "brief.md"));
        assert!(root_entries.iter().any(|entry| entry.name == "design"));

        let design_entries = core.list_dir(&project_id, "design").await.unwrap();
        assert!(design_entries.iter().any(|entry| entry.name == "specs"));

        let specs_entries = core.list_dir(&project_id, "design/specs").await.unwrap();
        assert!(
            specs_entries
                .iter()
                .any(|entry| entry.name == "routing-guide.md")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missing_paths_report_not_found_for_files_and_directories() {
        let (core, _db, project_id, _project_root) = make_core().await;

        let missing_file = core.read_file(&project_id, "patterns/missing.md").await;
        assert!(matches!(
            missing_file,
            Err(MemoryFsError::NotFound { path }) if path == "patterns/missing.md"
        ));

        let missing_dir = core.list_dir(&project_id, "patterns/missing").await;
        assert!(matches!(
            missing_dir,
            Err(MemoryFsError::NotFound { path }) if path == "patterns/missing"
        ));
    }
}
