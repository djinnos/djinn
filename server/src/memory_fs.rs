use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use djinn_memory::Note;
use djinn_db::{
    NoteRepository, folder_for_type, infer_embedding_branch_from_worktree, infer_note_type,
    normalize_virtual_note_path, permalink_for, permalink_from_virtual_note_path,
    render_note_markdown, task_branch_name, title_from_permalink, virtual_note_path_for_permalink,
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

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum MemoryViewSelection {
    #[default]
    Canonical,
    Branch {
        branch: String,
    },
    Worktree {
        root: PathBuf,
    },
    Task {
        task_short_id: Option<String>,
        worktree_root: Option<PathBuf>,
    },
}

#[derive(Clone)]
pub struct ResolvedMemoryView {
    pub repo: NoteRepository,
    pub branch: String,
    pub worktree_root: Option<PathBuf>,
}

pub trait MemoryViewResolver: Send + Sync {
    fn resolve_view(
        &self,
        selection: &MemoryViewSelection,
        base_repo: &NoteRepository,
    ) -> std::result::Result<ResolvedMemoryView, djinn_db::Error>;
}

#[derive(Debug, Default)]
pub struct DefaultMemoryViewResolver;

impl MemoryViewResolver for DefaultMemoryViewResolver {
    fn resolve_view(
        &self,
        selection: &MemoryViewSelection,
        base_repo: &NoteRepository,
    ) -> std::result::Result<ResolvedMemoryView, djinn_db::Error> {
        let mut repo = base_repo.clone();
        // The repo no longer mirrors notes to disk, so the resolver
        // collapses to "pick the embedding branch and remember the
        // worktree_root for callers who need it as context (e.g. for
        // logging or downstream filesystem mounts that overlay other
        // non-note .djinn/ files)".
        let (branch, worktree_root) = match selection {
            MemoryViewSelection::Canonical => ("main".to_string(), None),
            MemoryViewSelection::Branch { branch } => (branch.clone(), None),
            MemoryViewSelection::Worktree { root } => {
                let branch =
                    infer_embedding_branch_from_worktree(root).unwrap_or_else(|| "main".to_string());
                (branch, Some(root.clone()))
            }
            MemoryViewSelection::Task {
                task_short_id,
                worktree_root,
            } => {
                if let Some(root) = worktree_root.clone() {
                    let branch = infer_embedding_branch_from_worktree(&root)
                        .unwrap_or_else(|| "main".to_string());
                    (branch, Some(root))
                } else if let Some(short_id) = task_short_id {
                    (task_branch_name(short_id), None)
                } else {
                    ("main".to_string(), None)
                }
            }
        };

        repo = repo.with_embedding_branch(Some(branch.clone()));
        Ok(ResolvedMemoryView {
            repo,
            branch,
            worktree_root,
        })
    }
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
    view_resolver: Arc<dyn MemoryViewResolver>,
}

impl MemoryFilesystemCore {
    pub fn new(repo: NoteRepository) -> Self {
        Self {
            repo,
            view_resolver: Arc::new(DefaultMemoryViewResolver),
        }
    }

    pub fn with_view_resolver(mut self, view_resolver: Arc<dyn MemoryViewResolver>) -> Self {
        self.view_resolver = view_resolver;
        self
    }

    pub fn resolve_memory_view(
        &self,
        selection: &MemoryViewSelection,
    ) -> Result<ResolvedMemoryView> {
        Ok(self.view_resolver.resolve_view(selection, &self.repo)?)
    }

    pub async fn resolve_note_path(
        &self,
        project_id: &str,
        path: &str,
    ) -> Result<ResolvedNotePath> {
        self.resolve_note_path_in_view(project_id, &MemoryViewSelection::Canonical, path)
            .await
    }

    pub async fn resolve_note_path_in_view(
        &self,
        project_id: &str,
        selection: &MemoryViewSelection,
        path: &str,
    ) -> Result<ResolvedNotePath> {
        let normalized = normalize_virtual_note_path(path);
        let view = self.resolve_memory_view(selection)?;
        let permalink = permalink_from_virtual_note_path(&normalized).ok_or_else(|| {
            MemoryFsError::NotFile {
                path: normalized.clone(),
            }
        })?;
        let note = view
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
        self.stat_in_view(project_id, &MemoryViewSelection::Canonical, path)
            .await
    }

    pub async fn stat_in_view(
        &self,
        project_id: &str,
        selection: &MemoryViewSelection,
        path: &str,
    ) -> Result<MemoryEntryMetadata> {
        let normalized = normalize_virtual_note_path(path);
        if normalized.is_empty() {
            return Ok(MemoryEntryMetadata {
                path: String::new(),
                kind: MemoryEntryKind::Directory,
                size: 0,
            });
        }

        let tree = self.build_tree(project_id, selection).await?;
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
        self.list_dir_in_view(project_id, &MemoryViewSelection::Canonical, path)
            .await
    }

    pub async fn list_dir_in_view(
        &self,
        project_id: &str,
        selection: &MemoryViewSelection,
        path: &str,
    ) -> Result<Vec<MemoryDirEntry>> {
        let normalized = normalize_virtual_note_path(path);
        let tree = self.build_tree(project_id, selection).await?;

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
        self.read_file_in_view(project_id, &MemoryViewSelection::Canonical, path)
            .await
    }

    pub async fn read_file_in_view(
        &self,
        project_id: &str,
        selection: &MemoryViewSelection,
        path: &str,
    ) -> Result<MemoryFile> {
        let view = self.resolve_memory_view(selection)?;
        let resolved = self
            .resolve_note_path_in_view(project_id, selection, path)
            .await?;
        let note = view
            .repo
            .get_by_permalink(project_id, &resolved.permalink)
            .await?
            .ok_or_else(|| MemoryFsError::NotFound {
                path: resolved.logical_path.clone(),
            })?;

        view.repo.touch_accessed(&note.id).await?;

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
        self.write_file_in_view(
            project_id,
            &MemoryViewSelection::Canonical,
            project_path,
            path,
            content,
        )
        .await
    }

    pub async fn write_file_in_view(
        &self,
        project_id: &str,
        selection: &MemoryViewSelection,
        project_path: &Path,
        path: &str,
        content: &str,
    ) -> Result<MemoryFile> {
        let view = self.resolve_memory_view(selection)?;
        let normalized = normalize_virtual_note_path(path);
        let target_permalink = permalink_from_virtual_note_path(&normalized).ok_or_else(|| {
            MemoryFsError::NotFile {
                path: normalized.clone(),
            }
        })?;
        let parsed = parse_note_write(&target_permalink, content);

        let note = match view
            .repo
            .get_by_permalink(project_id, &target_permalink)
            .await?
        {
            Some(existing) => {
                let desired_permalink = permalink_for(&parsed.note_type, &parsed.title);
                let note = if desired_permalink != existing.permalink {
                    view.repo
                        .move_note(&existing.id, project_path, &parsed.title, &parsed.note_type)
                        .await?
                } else {
                    existing
                };

                view.repo
                    .update(&note.id, &parsed.title, &parsed.content, &parsed.tags)
                    .await?
            }
            None => {
                let _ = project_path;
                view.repo
                    .create(
                        project_id,
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
        self.delete_file_in_view(project_id, &MemoryViewSelection::Canonical, path)
            .await
    }

    pub async fn delete_file_in_view(
        &self,
        project_id: &str,
        selection: &MemoryViewSelection,
        path: &str,
    ) -> Result<()> {
        let view = self.resolve_memory_view(selection)?;
        let resolved = self
            .resolve_note_path_in_view(project_id, selection, path)
            .await?;
        view.repo.delete(&resolved.note_id).await?;
        Ok(())
    }

    pub async fn rename_file(
        &self,
        project_id: &str,
        project_path: &Path,
        from_path: &str,
        to_path: &str,
    ) -> Result<ResolvedNotePath> {
        self.rename_file_in_view(
            project_id,
            &MemoryViewSelection::Canonical,
            project_path,
            from_path,
            to_path,
        )
        .await
    }

    pub async fn rename_file_in_view(
        &self,
        project_id: &str,
        selection: &MemoryViewSelection,
        project_path: &Path,
        from_path: &str,
        to_path: &str,
    ) -> Result<ResolvedNotePath> {
        let view = self.resolve_memory_view(selection)?;
        let source = self
            .resolve_note_path_in_view(project_id, selection, from_path)
            .await?;
        let normalized_target = normalize_virtual_note_path(to_path);
        let target_permalink =
            permalink_from_virtual_note_path(&normalized_target).ok_or_else(|| {
                MemoryFsError::NotFile {
                    path: normalized_target.clone(),
                }
            })?;

        if let Some(existing) = view
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
        let moved = view
            .repo
            .move_note(&source.note_id, project_path, &title, &note_type)
            .await?;

        Ok(ResolvedNotePath {
            logical_path: virtual_note_path_for_permalink(&moved.permalink),
            permalink: moved.permalink,
            note_id: moved.id,
        })
    }

    async fn build_tree(
        &self,
        project_id: &str,
        selection: &MemoryViewSelection,
    ) -> Result<MemoryTree> {
        let view = self.resolve_memory_view(selection)?;
        let notes = view.repo.list(project_id, None).await?;
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

    fn render_note_content(&self, note: &Note) -> String {
        render_note_markdown(&note.title, &note.note_type, &note.tags, &note.content)
    }

    fn memory_file_from_note(&self, note: &Note) -> MemoryFile {
        let content = self.render_note_content(note);
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
    use std::sync::Mutex;

    #[derive(Debug, Default)]
    struct RecordingResolver {
        seen: Mutex<Vec<MemoryViewSelection>>,
    }

    impl MemoryViewResolver for RecordingResolver {
        fn resolve_view(
            &self,
            selection: &MemoryViewSelection,
            base_repo: &NoteRepository,
        ) -> std::result::Result<ResolvedMemoryView, djinn_db::Error> {
            self.seen.lock().unwrap().push(selection.clone());
            DefaultMemoryViewResolver.resolve_view(selection, base_repo)
        }
    }

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
        let (core, db, project_id, _project_root) = make_core().await;
        let repo = NoteRepository::new(db.clone(), EventBus::noop());
        repo.create(
            &project_id,
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
        let (core, db, project_id, _project_root) = make_core().await;
        let repo = NoteRepository::new(db, EventBus::noop());
        repo.create(
            &project_id,
            "Project Brief",
            "Overview",
            "brief",
            "[]",
        )
        .await
        .unwrap();
        repo.create(
            &project_id,
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn task_view_without_context_falls_back_to_main_branch() {
        let (core, _db, _project_id, _project_root) = make_core().await;

        let view = core
            .resolve_memory_view(&MemoryViewSelection::Task {
                task_short_id: None,
                worktree_root: None,
            })
            .unwrap();

        assert_eq!(view.branch, "main");
        assert!(view.worktree_root.is_none());
        assert_eq!(view.repo.embedding_branch(), "main");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn task_view_uses_explicit_worktree_selection_for_mutations() {
        let (core, db, project_id, project_root) = make_core().await;
        let base = std::env::current_dir()
            .unwrap()
            .join("target")
            .join("test-tmp");
        std::fs::create_dir_all(&base).unwrap();
        let worktree_root = tempfile::Builder::new()
            .prefix("memory-fs-task-view-")
            .tempdir_in(&base)
            .unwrap();
        let selection = MemoryViewSelection::Task {
            task_short_id: Some("abc1".to_string()),
            worktree_root: Some(worktree_root.path().to_path_buf()),
        };

        let file = core
            .write_file_in_view(
                &project_id,
                &selection,
                project_root.path(),
                "research/task-note.md",
                "---\ntitle: Task Note\ntype: research\ntags: [\"branch\"]\n---\n\nTask body",
            )
            .await
            .unwrap();
        assert_eq!(file.permalink, "research/task-note");

        let expected_worktree_path = worktree_root.path().join(".djinn/research/task-note.md");
        assert!(expected_worktree_path.exists());
        assert!(
            !project_root
                .path()
                .join(".djinn/research/task-note.md")
                .exists()
        );

        let stat = core
            .stat_in_view(&project_id, &selection, "research/task-note.md")
            .await
            .unwrap();
        assert_eq!(stat.kind, MemoryEntryKind::File);

        let listed = core
            .list_dir_in_view(&project_id, &selection, "research")
            .await
            .unwrap();
        assert!(listed.iter().any(|entry| entry.name == "task-note.md"));

        let read_back = core
            .read_file_in_view(&project_id, &selection, "research/task-note.md")
            .await
            .unwrap();
        assert!(read_back.content.contains("Task body"));

        let renamed = core
            .rename_file_in_view(
                &project_id,
                &selection,
                project_root.path(),
                "research/task-note.md",
                "patterns/task-note-renamed.md",
            )
            .await
            .unwrap();
        assert_eq!(renamed.permalink, "patterns/task-note-renamed");

        core.delete_file_in_view(&project_id, &selection, "patterns/task-note-renamed.md")
            .await
            .unwrap();
        assert!(
            !worktree_root
                .path()
                .join(".djinn/patterns/task-note-renamed.md")
                .exists()
        );

        let persisted = NoteRepository::new(db, EventBus::noop())
            .get_by_permalink(&project_id, "patterns/task-note-renamed")
            .await
            .unwrap();
        assert!(persisted.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn explicit_view_selection_is_threaded_through_core_operations() {
        let base = std::env::current_dir()
            .unwrap()
            .join("target")
            .join("test-tmp");
        std::fs::create_dir_all(&base).unwrap();
        let project_root = tempfile::Builder::new()
            .prefix("memory-fs-recording-")
            .tempdir_in(&base)
            .unwrap();
        let db = Database::open_in_memory().unwrap();
        let project = make_project(&db, project_root.path()).await;
        let repo = NoteRepository::new(db.clone(), EventBus::noop());
        repo.create(
            &project.id,
            "Selection Note",
            "body",
            "reference",
            "[]",
        )
        .await
        .unwrap();

        let recording = Arc::new(RecordingResolver::default());
        let core = MemoryFilesystemCore::new(repo.clone()).with_view_resolver(recording.clone());
        let selection = MemoryViewSelection::Branch {
            branch: "task/sel1".to_string(),
        };

        core.stat_in_view(&project.id, &selection, "reference/selection-note.md")
            .await
            .unwrap();
        core.list_dir_in_view(&project.id, &selection, "reference")
            .await
            .unwrap();
        core.read_file_in_view(&project.id, &selection, "reference/selection-note.md")
            .await
            .unwrap();

        let seen = recording.seen.lock().unwrap();
        assert_eq!(seen.len(), 4);
        assert!(seen.iter().all(|item| item == &selection));
    }
}

#[cfg(test)]
mod integration_tests;
