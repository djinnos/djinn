use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use djinn_core::models::Note;
use djinn_db::{
    NoteRepository, folder_for_type, infer_note_type, normalize_virtual_note_path, permalink_for,
    permalink_from_virtual_note_path, render_note_markdown, title_from_permalink,
    virtual_note_path_for_permalink,
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedMemoryNoteWrite {
    title: String,
    note_type: String,
    tags: String,
    content: String,
}

#[derive(Debug, Default)]
struct MemoryTree {
    directories: BTreeSet<String>,
    files: BTreeMap<String, Note>,
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

    fn insert_note(&mut self, note: Note) {
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

        let content = self.render_note_content(&note);
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

    pub async fn write_file(
        &self,
        project_id: &str,
        project_path: &Path,
        path: &str,
        content: &str,
    ) -> Result<MemoryFile> {
        let normalized = normalize_virtual_note_path(path);
        let target_permalink = permalink_from_virtual_note_path(&normalized).ok_or_else(|| {
            MemoryFsError::NotFile {
                path: normalized.clone(),
            }
        })?;
        let parsed = parse_note_write(&target_permalink, content);

        let note = match self
            .repo
            .get_by_permalink(project_id, &target_permalink)
            .await?
        {
            Some(existing) => {
                let desired_permalink = permalink_for(&parsed.note_type, &parsed.title);
                let note = if desired_permalink != existing.permalink {
                    self.repo
                        .move_note(&existing.id, project_path, &parsed.title, &parsed.note_type)
                        .await?
                } else {
                    existing
                };

                self.repo
                    .update(&note.id, &parsed.title, &parsed.content, &parsed.tags)
                    .await?
            }
            None => {
                self.repo
                    .create(
                        project_id,
                        project_path,
                        &parsed.title,
                        &parsed.content,
                        &parsed.note_type,
                        &parsed.tags,
                    )
                    .await?
            }
        };

        Ok(self.memory_file_from_note(&note))
    }

    pub async fn delete_file(&self, project_id: &str, path: &str) -> Result<()> {
        let resolved = self.resolve_note_path(project_id, path).await?;
        self.repo.delete(&resolved.note_id).await?;
        Ok(())
    }

    pub async fn rename_file(
        &self,
        project_id: &str,
        project_path: &Path,
        from_path: &str,
        to_path: &str,
    ) -> Result<ResolvedNotePath> {
        let source = self.resolve_note_path(project_id, from_path).await?;
        let normalized_target = normalize_virtual_note_path(to_path);
        let target_permalink =
            permalink_from_virtual_note_path(&normalized_target).ok_or_else(|| {
                MemoryFsError::NotFile {
                    path: normalized_target.clone(),
                }
            })?;

        if let Some(existing) = self
            .repo
            .get_by_permalink(project_id, &target_permalink)
            .await?
            && existing.id != source.note_id
        {
            return Err(MemoryFsError::Repository(djinn_db::Error::InvalidData(
                format!("path already exists: {normalized_target}"),
            )));
        }

        let title = title_from_permalink(&target_permalink);
        let note_type = infer_note_type(&target_permalink);
        let moved = self
            .repo
            .move_note(&source.note_id, project_path, &title, &note_type)
            .await?;

        Ok(ResolvedNotePath {
            logical_path: virtual_note_path_for_permalink(&moved.permalink),
            permalink: moved.permalink,
            note_id: moved.id,
        })
    }

    async fn build_tree(&self, project_id: &str) -> Result<MemoryTree> {
        let notes = self.repo.list(project_id, None).await?;
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

    fn file_metadata(&self, note: &Note) -> MemoryEntryMetadata {
        let path = virtual_note_path_for_permalink(&note.permalink);
        let size = self.render_note_content(note).len() as u64;
        MemoryEntryMetadata {
            path,
            kind: MemoryEntryKind::File,
            size,
        }
    }

<<<<<<< HEAD
    fn memory_file_from_note(&self, note: &djinn_core::models::Note) -> MemoryFile {
        let content = render_note_markdown(&note.title, &note.note_type, &note.tags, &note.content);
        MemoryFile {
            metadata: MemoryEntryMetadata {
                path: virtual_note_path_for_permalink(&note.permalink),
                kind: MemoryEntryKind::File,
                size: content.len() as u64,
            },
            content,
            note_id: note.id.clone(),
            permalink: note.permalink.clone(),
        }
    }
}

fn parse_note_write(target_permalink: &str, raw: &str) -> ParsedMemoryNoteWrite {
    let (frontmatter, body) = split_frontmatter(raw);
    let note_type = frontmatter
        .and_then(|fm| frontmatter_value(fm, "type"))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| infer_note_type(target_permalink));
    let title = frontmatter
        .and_then(|fm| frontmatter_value(fm, "title"))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| title_from_permalink(target_permalink));
    let tags = frontmatter
        .and_then(|fm| frontmatter_value(fm, "tags"))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "[]".to_string());

    ParsedMemoryNoteWrite {
        title,
        note_type,
        tags,
        content: body.to_string(),
    }
}

fn split_frontmatter(raw: &str) -> (Option<&str>, &str) {
    if let Some(rest) = raw.strip_prefix("---\n")
        && let Some(end) = rest.find("\n---\n")
    {
        let frontmatter = &rest[..end];
        let body = rest[end + 5..]
            .strip_prefix('\n')
            .unwrap_or(&rest[end + 5..]);
        return (Some(frontmatter), body);
    }

    (None, raw)
}

fn frontmatter_value(frontmatter: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}: ");
    frontmatter.lines().find_map(|line| {
        line.strip_prefix(&prefix)
            .map(|value| value.trim().to_string())
    })
=======
    fn render_note_content(&self, note: &Note) -> String {
        render_note_markdown(&note.title, &note.note_type, &note.tags, &note.content)
    }
>>>>>>> origin/main
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

        let before = repo
            .get_by_permalink(&project_id, "patterns/reusable-flow")
            .await
            .unwrap()
            .unwrap();

        let file = core
            .read_file(&project_id, "patterns/reusable-flow.md")
            .await
            .unwrap();
        assert_eq!(file.metadata.kind, MemoryEntryKind::File);
        assert!(file.content.contains("title: Reusable Flow"));
        assert!(file.content.contains("Body text"));
        assert_eq!(file.metadata.size, file.content.len() as u64);

        let touched = repo
            .get_by_permalink(&project_id, "patterns/reusable-flow")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(touched.access_count, before.access_count + 1);
        assert!(touched.last_accessed >= before.last_accessed);
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

        let brief_stat = core.stat(&project_id, "brief.md").await.unwrap();
        assert_eq!(brief_stat.kind, MemoryEntryKind::File);
        assert!(brief_stat.size > 0);
        assert_eq!(
            root_entries
                .iter()
                .find(|entry| entry.name == "brief.md")
                .unwrap()
                .metadata
                .size,
            brief_stat.size
        );

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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn write_file_create_update_rename_and_delete_flow_stays_indexed() {
        let (core, db, project_id, project_root) = make_core().await;
        let repo = NoteRepository::new(db, EventBus::noop());

        repo.create(
            &project_id,
            project_root.path(),
            "Target Note",
            "Target body",
            "reference",
            "[]",
        )
        .await
        .unwrap();

        let created = core
            .write_file(
                &project_id,
                project_root.path(),
                "patterns/source-note.md",
                "---\ntitle: Source Note\ntype: pattern\ntags: [\"fs\"]\n---\n\nLinks to [[Target Note]].",
            )
            .await
            .unwrap();
        assert_eq!(created.permalink, "patterns/source-note");
        assert!(created.content.contains("title: Source Note"));

        let created_note = repo
            .get_by_permalink(&project_id, "patterns/source-note")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(created_note.note_type, "pattern");
        assert_eq!(created_note.tags, "[\"fs\"]");
        assert_eq!(repo.broken_links(&project_id, None).await.unwrap().len(), 0);
        assert_eq!(repo.graph(&project_id).await.unwrap().edges.len(), 1);

        let updated = core
            .write_file(
                &project_id,
                project_root.path(),
                "patterns/source-note.md",
                "---\ntitle: Source Note\ntype: pattern\ntags: [\"fs\",\"updated\"]\n---\n\nNow links to [[Missing Note]].",
            )
            .await
            .unwrap();
        assert!(updated.content.contains("updated"));

        let updated_note = repo
            .get_by_permalink(&project_id, "patterns/source-note")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated_note.tags, "[\"fs\",\"updated\"]");
        assert_eq!(repo.graph(&project_id).await.unwrap().edges.len(), 0);
        assert_eq!(repo.broken_links(&project_id, None).await.unwrap().len(), 1);

        let renamed = core
            .rename_file(
                &project_id,
                project_root.path(),
                "patterns/source-note.md",
                "research/renamed-note.md",
            )
            .await
            .unwrap();
        assert_eq!(renamed.logical_path, "research/renamed-note.md");
        assert!(
            core.read_file(&project_id, "patterns/source-note.md")
                .await
                .is_err()
        );

        let renamed_note = repo
            .get_by_permalink(&project_id, "research/renamed-note")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(renamed_note.title, "Renamed Note");
        assert_eq!(renamed_note.note_type, "research");
        assert_eq!(renamed_note.folder, "research");
        assert_eq!(repo.broken_links(&project_id, None).await.unwrap().len(), 1);

        core.delete_file(&project_id, "research/renamed-note.md")
            .await
            .unwrap();
        assert!(
            repo.get_by_permalink(&project_id, "research/renamed-note")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(repo.broken_links(&project_id, None).await.unwrap().len(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn write_file_uses_frontmatter_metadata_consistently() {
        let (core, db, project_id, project_root) = make_core().await;
        let repo = NoteRepository::new(db, EventBus::noop());

        let file = core
            .write_file(
                &project_id,
                project_root.path(),
                "design/frontmatter-title.md",
                "---\ntitle: Frontmatter Title\ntype: design\ntags: [\"meta\",\"frontmatter\"]\n---\n\nDesign body",
            )
            .await
            .unwrap();

        assert_eq!(file.permalink, "design/frontmatter-title");
        let note = repo
            .get_by_permalink(&project_id, "design/frontmatter-title")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(note.title, "Frontmatter Title");
        assert_eq!(note.note_type, "design");
        assert_eq!(note.tags, "[\"meta\",\"frontmatter\"]");
        assert_eq!(note.content, "Design body");
    }
}
