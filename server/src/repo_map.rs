use std::collections::HashSet;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow};
use djinn_db::NoteRepository;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use petgraph::visit::EdgeRef;

use crate::process;
use crate::repo_graph::{
    RankedRepoGraphNode, RepoDependencyGraph, RepoGraphEdgeKind, RepoGraphNodeKind,
    RepoGraphRanking,
};
use crate::repo_map_personalization::{
    RepoMapNoteHint, RepoMapNoteSearcher, RepoMapPersonalizationInput,
};
use crate::scip_parser::ScipSymbolKind;

const SCIP_ARTIFACT_EXTENSION: &str = "scip";
const DEFAULT_MAX_FILES: usize = 12;
const DEFAULT_MAX_SYMBOLS_PER_FILE: usize = 4;
const DEFAULT_MAX_RELATIONSHIPS_PER_FILE: usize = 3;
pub const REPO_MAP_NOTE_TYPE: &str = "repo_map";
pub const REPO_MAP_NOTE_FOLDER: &str = "reference/repo-maps";
pub const REPO_MAP_NOTE_TAG: &str = "repo-map";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SupportedIndexer {
    RustAnalyzer,
    TypeScript,
    Python,
    Go,
    Java,
}

impl SupportedIndexer {
    pub const ALL: [Self; 5] = [
        Self::RustAnalyzer,
        Self::TypeScript,
        Self::Python,
        Self::Go,
        Self::Java,
    ];

    pub fn binary_name(self) -> &'static str {
        match self {
            Self::RustAnalyzer => "rust-analyzer",
            Self::TypeScript => "scip-typescript",
            Self::Python => "scip-python",
            Self::Go => "scip-go",
            Self::Java => "scip-java",
        }
    }

    pub fn language(self) -> &'static str {
        match self {
            Self::RustAnalyzer => "rust",
            Self::TypeScript => "typescript",
            Self::Python => "python",
            Self::Go => "go",
            Self::Java => "java",
        }
    }

    fn marker_files(self) -> &'static [&'static str] {
        match self {
            Self::RustAnalyzer => &["Cargo.toml"],
            Self::TypeScript => &["tsconfig.json", "package.json"],
            Self::Python => &["pyproject.toml", "setup.py"],
            Self::Go => &["go.mod"],
            Self::Java => &["build.gradle", "pom.xml"],
        }
    }

    fn default_output_path(
        self,
        project_root: &Path,
        output_root: &Path,
        workspace_slug: &str,
    ) -> PathBuf {
        let project_name = project_root
            .file_name()
            .and_then(OsStr::to_str)
            .filter(|name| !name.is_empty())
            .unwrap_or("project");
        output_root.join(format!(
            "{project_name}-{}-{workspace_slug}.scip",
            self.language()
        ))
    }

    fn command_args(self, output_path: &Path) -> Vec<String> {
        let output = output_path.to_string_lossy().into_owned();
        match self {
            Self::RustAnalyzer => vec![
                "scip".to_string(),
                ".".to_string(),
                "--output".to_string(),
                output,
            ],
            Self::TypeScript => vec!["index".to_string(), output],
            Self::Python => vec!["index".to_string(), output],
            Self::Go => vec!["index".to_string(), output],
            Self::Java => vec!["index".to_string(), output],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredWorkspace {
    pub indexer: SupportedIndexer,
    pub root: PathBuf,
    pub slug: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexerAvailability {
    pub indexer: SupportedIndexer,
    pub binary: String,
    pub path: Option<PathBuf>,
}

impl IndexerAvailability {
    pub fn is_available(&self) -> bool {
        self.path.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedIndexerCommand {
    pub indexer: SupportedIndexer,
    pub binary_path: PathBuf,
    pub args: Vec<String>,
    pub working_directory: PathBuf,
    pub workspace_root: PathBuf,
    pub output_path: PathBuf,
}

impl PlannedIndexerCommand {
    fn build_command(&self) -> Command {
        let mut command = Command::new(&self.binary_path);
        command.current_dir(&self.working_directory);
        command.args(&self.args);
        command
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutedIndexerCommand {
    pub plan: PlannedIndexerCommand,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScipArtifact {
    pub path: PathBuf,
    pub indexer: Option<SupportedIndexer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexingRun {
    pub project_root: PathBuf,
    pub output_root: PathBuf,
    pub commands: Vec<ExecutedIndexerCommand>,
    pub artifacts: Vec<ScipArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoMapRenderOptions {
    pub token_budget: usize,
    pub max_files: usize,
    pub max_symbols_per_file: usize,
    pub max_relationships_per_file: usize,
}

impl RepoMapRenderOptions {
    pub fn new(token_budget: usize) -> Self {
        Self {
            token_budget,
            max_files: DEFAULT_MAX_FILES,
            max_symbols_per_file: DEFAULT_MAX_SYMBOLS_PER_FILE,
            max_relationships_per_file: DEFAULT_MAX_RELATIONSHIPS_PER_FILE,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedRepoMap {
    pub content: String,
    pub token_estimate: usize,
    pub included_entries: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoMapNoteSpec {
    pub title: String,
    pub permalink: String,
    pub tags_json: String,
}

pub fn repo_map_note_spec(commit_sha: &str) -> RepoMapNoteSpec {
    let short_sha: String = commit_sha.chars().take(12).collect();
    let title = format!("Repository Map {short_sha}");
    let permalink = format!("{REPO_MAP_NOTE_FOLDER}/{short_sha}");
    let tags_json = serde_json::to_string(&vec![REPO_MAP_NOTE_TAG.to_string(), short_sha.clone()])
        .unwrap_or_else(|_| "[]".to_string());

    RepoMapNoteSpec {
        title,
        permalink,
        tags_json,
    }
}

pub async fn persist_repo_map_note(
    note_repo: &NoteRepository,
    project_id: &str,
    commit_sha: &str,
    rendered: &RenderedRepoMap,
) -> Result<djinn_core::models::Note, djinn_db::Error> {
    let spec = repo_map_note_spec(commit_sha);
    note_repo
        .upsert_db_note_by_permalink(
            project_id,
            &spec.permalink,
            &spec.title,
            &rendered.content,
            REPO_MAP_NOTE_TYPE,
            &spec.tags_json,
        )
        .await
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoMapRenderError {
    MinimalRepresentationExceedsBudget {
        budget: usize,
        required_tokens: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RepoMapEntry {
    file_path: PathBuf,
    language: Option<String>,
    score_milli: i64,
    boost_score: u32,
    symbols: Vec<RepoMapSymbol>,
    relationships: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RepoMapSymbol {
    name: String,
    kind: Option<ScipSymbolKind>,
    score_milli: i64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RepoMapPersonalizationRequest<'a> {
    pub ranked_nodes: &'a [RankedRepoGraphNode],
    pub title: Option<&'a str>,
    pub description: Option<&'a str>,
    pub design: Option<&'a str>,
    pub memory_refs: &'a [String],
    pub note_hints: &'a [RepoMapNoteHint],
}

impl RepoMapNoteSearcher for NoteRepository {
    type Error = djinn_db::Error;

    async fn search<'a>(
        &'a self,
        project_id: &'a str,
        query: &'a str,
        task_id: Option<&'a str>,
        limit: usize,
    ) -> Result<Vec<djinn_core::models::NoteSearchResult>, Self::Error> {
        NoteRepository::search(self, project_id, query, task_id, None, None, limit).await
    }
}

pub fn detect_indexers() -> Vec<IndexerAvailability> {
    detect_indexers_in_path(env::var("PATH").unwrap_or_default())
}

fn detect_indexers_in_path(path_var: impl AsRef<str>) -> Vec<IndexerAvailability> {
    let path_var = path_var.as_ref();
    SupportedIndexer::ALL
        .into_iter()
        .map(|indexer| IndexerAvailability {
            indexer,
            binary: indexer.binary_name().to_string(),
            path: which_in_path(indexer.binary_name(), path_var),
        })
        .collect()
}

pub fn plan_indexer_commands(
    project_root: impl AsRef<Path>,
    output_root: impl AsRef<Path>,
    available_indexers: &[IndexerAvailability],
) -> Vec<PlannedIndexerCommand> {
    let project_root = project_root.as_ref();
    let output_root = output_root.as_ref();

    available_indexers
        .iter()
        .flat_map(|availability| {
            let Some(binary_path) = availability.path.as_ref() else {
                return Vec::new();
            };

            discover_workspaces(project_root, availability.indexer)
                .into_iter()
                .map(|workspace| {
                    let working_directory = project_root.join(&workspace.root);
                    let output_path = availability.indexer.default_output_path(
                        project_root,
                        output_root,
                        &workspace.slug,
                    );
                    PlannedIndexerCommand {
                        indexer: availability.indexer,
                        binary_path: binary_path.clone(),
                        args: availability.indexer.command_args(&output_path),
                        working_directory: working_directory.clone(),
                        workspace_root: working_directory,
                        output_path,
                    }
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

pub async fn run_indexers(
    project_root: impl AsRef<Path>,
    output_root: impl AsRef<Path>,
) -> Result<IndexingRun> {
    let project_root = project_root.as_ref().to_path_buf();
    let output_root = output_root.as_ref().to_path_buf();
    fs::create_dir_all(&output_root)
        .with_context(|| format!("create SCIP output dir {}", output_root.display()))?;

    let available = detect_indexers();
    let plans = plan_indexer_commands(&project_root, &output_root, &available);
    let mut commands = Vec::with_capacity(plans.len());

    for plan in plans {
        let output = process::output(plan.build_command())
            .await
            .with_context(|| format!("run {}", plan.indexer.binary_name()))?;

        if !output.status.success() {
            return Err(anyhow!(
                "SCIP indexer {} failed with status {:?}: {}",
                plan.indexer.binary_name(),
                output.status.code(),
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        commands.push(ExecutedIndexerCommand {
            plan,
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    let artifacts = collect_scip_artifacts(&output_root, &commands)?;

    Ok(IndexingRun {
        project_root,
        output_root,
        commands,
        artifacts,
    })
}

pub fn collect_scip_artifacts(
    output_root: impl AsRef<Path>,
    commands: &[ExecutedIndexerCommand],
) -> Result<Vec<ScipArtifact>> {
    let output_root = output_root.as_ref();
    let mut seen = HashSet::new();
    let mut artifacts = Vec::new();

    let expected_paths: Vec<(PathBuf, SupportedIndexer)> = commands
        .iter()
        .map(|command| (command.plan.output_path.clone(), command.plan.indexer))
        .collect();

    for path in discover_scip_files(output_root)? {
        if seen.insert(path.clone()) {
            let indexer = expected_paths
                .iter()
                .find_map(|(expected, indexer)| (expected == &path).then_some(*indexer));
            artifacts.push(ScipArtifact { path, indexer });
        }
    }

    artifacts.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(artifacts)
}

pub fn render_repo_map(
    graph: &RepoDependencyGraph,
    ranking: &RepoGraphRanking,
    options: &RepoMapRenderOptions,
) -> Result<RenderedRepoMap, RepoMapRenderError> {
    let entries = build_repo_map_entries(graph, &ranking.nodes, options);
    render_repo_map_from_entries(&entries, options)
}

pub fn personalized_repo_map_ranking(
    graph: &RepoDependencyGraph,
    request: &RepoMapPersonalizationRequest<'_>,
) -> Vec<RankedRepoGraphNode> {
    let mut identifiers = crate::repo_map_personalization::extract_identifier_candidates(
        &RepoMapPersonalizationInput {
            title: request.title,
            description: request.description,
            design: request.design,
            memory_refs: request.memory_refs,
        },
    );

    identifiers.extend(
        request
            .note_hints
            .iter()
            .flat_map(|hint| hint.normalized_tokens.iter().cloned()),
    );
    identifiers.sort();
    identifiers.dedup();

    let mut ranked = request.ranked_nodes.to_vec();
    ranked.sort_by(|left, right| {
        let left_boost = repo_map_entry_boost(graph, left, &identifiers);
        let right_boost = repo_map_entry_boost(graph, right, &identifiers);
        right_boost
            .cmp(&left_boost)
            .then_with(|| right.score.total_cmp(&left.score))
            .then_with(|| left.key.cmp(&right.key))
    });
    ranked
}

fn build_repo_map_entries(
    graph: &RepoDependencyGraph,
    ranked_nodes: &[RankedRepoGraphNode],
    options: &RepoMapRenderOptions,
) -> Vec<RepoMapEntry> {
    let mut files = Vec::new();

    for ranked in ranked_nodes
        .iter()
        .filter(|node| node.kind == RepoGraphNodeKind::File)
    {
        if files.len() >= options.max_files {
            break;
        }

        let file_node = graph.node(ranked.node_index);
        let Some(file_path) = file_node.file_path.clone() else {
            continue;
        };

        let mut symbols = graph
            .graph()
            .neighbors(ranked.node_index)
            .filter_map(|neighbor| {
                let node = graph.node(neighbor);
                if node.kind != RepoGraphNodeKind::Symbol || node.is_external {
                    return None;
                }
                if node.file_path.as_ref() != Some(&file_path) {
                    return None;
                }
                ranked_nodes
                    .iter()
                    .find(|ranked_symbol| ranked_symbol.node_index == neighbor)
                    .map(|ranked_symbol| RepoMapSymbol {
                        name: node.display_name.clone(),
                        kind: node.symbol_kind.clone(),
                        score_milli: score_to_milli(ranked_symbol.score),
                    })
            })
            .collect::<Vec<_>>();

        symbols.sort_by(|left, right| {
            right
                .score_milli
                .cmp(&left.score_milli)
                .then_with(|| left.name.cmp(&right.name))
                .then_with(|| {
                    format_symbol_kind(left.kind.as_ref())
                        .cmp(format_symbol_kind(right.kind.as_ref()))
                })
        });
        symbols.truncate(options.max_symbols_per_file);

        let mut relationships = graph
            .graph()
            .edges(ranked.node_index)
            .filter_map(|edge| {
                let target = graph.node(edge.target());
                if target.is_external || target.kind != RepoGraphNodeKind::File {
                    return None;
                }
                let target_path = target.file_path.as_ref()?;
                Some(format!(
                    "{} {}",
                    format_edge_kind(edge.weight().kind),
                    target_path.display()
                ))
            })
            .collect::<Vec<_>>();
        relationships.sort();
        relationships.dedup();
        relationships.truncate(options.max_relationships_per_file);

        files.push(RepoMapEntry {
            file_path,
            language: file_node.language.clone(),
            score_milli: score_to_milli(ranked.score),
            boost_score: 0,
            symbols,
            relationships,
        });
    }

    files.sort_by(|left, right| {
        right
            .boost_score
            .cmp(&left.boost_score)
            .then_with(|| right.score_milli.cmp(&left.score_milli))
            .then_with(|| left.file_path.cmp(&right.file_path))
    });
    files
}

fn render_repo_map_from_entries(
    entries: &[RepoMapEntry],
    options: &RepoMapRenderOptions,
) -> Result<RenderedRepoMap, RepoMapRenderError> {
    let minimal_content = render_repo_map_slice(entries, 1, options);
    let minimal_tokens = estimate_tokens(&minimal_content);
    if minimal_tokens > options.token_budget {
        return Err(RepoMapRenderError::MinimalRepresentationExceedsBudget {
            budget: options.token_budget,
            required_tokens: minimal_tokens,
        });
    }

    let mut low = 1usize;
    let mut high = entries.len();
    let mut best = RenderedRepoMap {
        content: minimal_content,
        token_estimate: minimal_tokens,
        included_entries: 1,
    };

    while low <= high {
        let mid = low + ((high - low) / 2);
        let content = render_repo_map_slice(entries, mid, options);
        let token_estimate = estimate_tokens(&content);

        if token_estimate <= options.token_budget {
            best = RenderedRepoMap {
                content,
                token_estimate,
                included_entries: mid,
            };
            low = mid.saturating_add(1);
        } else if mid == 0 {
            break;
        } else {
            high = mid - 1;
        }
    }

    Ok(best)
}

fn repo_map_entry_boost(
    graph: &RepoDependencyGraph,
    ranked: &RankedRepoGraphNode,
    identifiers: &[String],
) -> u32 {
    if identifiers.is_empty() {
        return 0;
    }

    let node = graph.node(ranked.node_index);
    let mut haystacks = Vec::new();
    haystacks.push(node.display_name.to_ascii_lowercase());
    if let Some(file_path) = &node.file_path {
        haystacks.push(file_path.display().to_string().to_ascii_lowercase());
    }
    if let Some(symbol) = &node.symbol {
        haystacks.push(symbol.to_ascii_lowercase());
    }

    for neighbor in graph.graph().neighbors(ranked.node_index) {
        let neighbor_node = graph.node(neighbor);
        haystacks.push(neighbor_node.display_name.to_ascii_lowercase());
        if let Some(file_path) = &neighbor_node.file_path {
            haystacks.push(file_path.display().to_string().to_ascii_lowercase());
        }
    }

    for edge in graph.graph().edges(ranked.node_index) {
        let target = graph.node(edge.target());
        if let Some(file_path) = &target.file_path {
            haystacks.push(
                format!(
                    "{} {}",
                    format_edge_kind(edge.weight().kind),
                    file_path.display()
                )
                .to_ascii_lowercase(),
            );
        }
        haystacks.push(target.display_name.to_ascii_lowercase());
    }

    identifiers
        .iter()
        .filter(|identifier| {
            haystacks
                .iter()
                .any(|haystack| haystack.contains(identifier.as_str()))
        })
        .count() as u32
}

fn render_repo_map_slice(
    entries: &[RepoMapEntry],
    included_entries: usize,
    options: &RepoMapRenderOptions,
) -> String {
    let included_entries = included_entries.min(entries.len());
    let mut lines = vec![
        "# Repository Map".to_string(),
        format!(
            "Showing {} ranked files under ~{} token budget.",
            included_entries, options.token_budget
        ),
    ];

    for entry in entries.iter().take(included_entries) {
        let language = entry.language.as_deref().unwrap_or("unknown");
        lines.push(format!(
            "- file: {} [{}] score={}",
            entry.file_path.display(),
            language,
            format_score(entry.score_milli)
        ));

        if !entry.symbols.is_empty() {
            let rendered = entry
                .symbols
                .iter()
                .map(|symbol| {
                    format!(
                        "{}{} ({})",
                        symbol.name,
                        format_symbol_kind_suffix(symbol.kind.as_ref()),
                        format_score(symbol.score_milli)
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(format!("  symbols: {rendered}"));
        }

        if !entry.relationships.is_empty() {
            lines.push(format!("  links: {}", entry.relationships.join(", ")));
        }
    }

    lines.join("\n")
}

fn estimate_tokens(content: &str) -> usize {
    let chars = content.chars().count();
    if chars == 0 { 0 } else { chars.div_ceil(4) }
}

fn score_to_milli(score: f64) -> i64 {
    (score * 1000.0).round() as i64
}

fn format_score(score_milli: i64) -> String {
    format!("{:.3}", score_milli as f64 / 1000.0)
}

fn format_symbol_kind(kind: Option<&ScipSymbolKind>) -> &'static str {
    match kind {
        Some(ScipSymbolKind::Type) | Some(ScipSymbolKind::Struct) => "type",
        Some(ScipSymbolKind::Enum) => "enum",
        Some(ScipSymbolKind::Interface) => "interface",
        Some(ScipSymbolKind::Method) | Some(ScipSymbolKind::Constructor) => "method",
        Some(ScipSymbolKind::Function) => "function",
        Some(ScipSymbolKind::Variable) => "variable",
        Some(ScipSymbolKind::Field) => "field",
        Some(ScipSymbolKind::Property) => "property",
        Some(ScipSymbolKind::Constant) => "constant",
        Some(_) => "symbol",
        None => "symbol",
    }
}

fn format_symbol_kind_suffix(kind: Option<&ScipSymbolKind>) -> String {
    format!(":{}", format_symbol_kind(kind))
}

fn format_edge_kind(kind: RepoGraphEdgeKind) -> &'static str {
    match kind {
        RepoGraphEdgeKind::ContainsDefinition => "contains",
        RepoGraphEdgeKind::DeclaredInFile => "declares",
        RepoGraphEdgeKind::FileReference => "references",
        RepoGraphEdgeKind::SymbolReference => "symbol-ref",
        RepoGraphEdgeKind::SymbolRelationshipReference => "rel-ref",
        RepoGraphEdgeKind::SymbolRelationshipImplementation => "implements",
        RepoGraphEdgeKind::SymbolRelationshipTypeDefinition => "type-def",
        RepoGraphEdgeKind::SymbolRelationshipDefinition => "definition",
    }
}

fn discover_workspaces(project_root: &Path, indexer: SupportedIndexer) -> Vec<DiscoveredWorkspace> {
    let mut roots = HashSet::new();
    let mut discovered = Vec::new();

    if let Err(error) = visit_dirs(project_root, &mut |path| {
        let Some(file_name) = path.file_name().and_then(OsStr::to_str) else {
            return Ok(());
        };

        if !indexer.marker_files().contains(&file_name) {
            return Ok(());
        }

        let Some(parent) = path.parent() else {
            return Ok(());
        };

        if !matches_workspace_marker(indexer, path)? {
            return Ok(());
        }

        let relative_root = parent
            .strip_prefix(project_root)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| parent.to_path_buf());
        if roots.insert(relative_root.clone()) {
            discovered.push(DiscoveredWorkspace {
                indexer,
                slug: workspace_slug(&relative_root),
                root: relative_root,
            });
        }
        Ok(())
    }) {
        tracing::warn!(
            project_root = %project_root.display(),
            language = indexer.language(),
            error = %error,
            "failed to discover workspace roots; falling back to project root"
        );
    }

    if discovered.is_empty() {
        discovered.push(DiscoveredWorkspace {
            indexer,
            root: PathBuf::new(),
            slug: "root".to_string(),
        });
    }

    discovered.sort_by(|left, right| left.root.cmp(&right.root));
    discovered
}

fn matches_workspace_marker(indexer: SupportedIndexer, path: &Path) -> Result<bool> {
    match indexer {
        SupportedIndexer::RustAnalyzer => file_contains(path, "[workspace]"),
        SupportedIndexer::TypeScript => {
            let file_name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
            if file_name == "tsconfig.json" {
                Ok(true)
            } else {
                package_json_has_workspaces(path)
            }
        }
        SupportedIndexer::Python | SupportedIndexer::Go | SupportedIndexer::Java => Ok(true),
    }
}

fn file_contains(path: &Path, needle: &str) -> Result<bool> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("read workspace marker {}", path.display()))?;
    Ok(content.contains(needle))
}

fn package_json_has_workspaces(path: &Path) -> Result<bool> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("read package.json {}", path.display()))?;
    let json: Value = serde_json::from_str(&content)
        .with_context(|| format!("parse package.json {}", path.display()))?;
    Ok(match json.get("workspaces") {
        Some(Value::Array(values)) => !values.is_empty(),
        Some(Value::Object(map)) => map
            .get("packages")
            .and_then(Value::as_array)
            .is_some_and(|values| !values.is_empty()),
        Some(_) => true,
        None => false,
    })
}

fn workspace_slug(root: &Path) -> String {
    if root.as_os_str().is_empty() {
        return "root".to_string();
    }

    let slug = root
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .flat_map(|segment| {
            segment
                .chars()
                .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
                .collect::<String>()
                .split('-')
                .filter(|part| !part.is_empty())
                .map(str::to_ascii_lowercase)
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>()
        .join("-");

    if slug.is_empty() {
        "root".to_string()
    } else {
        slug
    }
}

fn discover_scip_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut artifacts = Vec::new();
    visit_dirs(root, &mut |path| {
        if path
            .extension()
            .and_then(OsStr::to_str)
            .is_some_and(|ext| ext == SCIP_ARTIFACT_EXTENSION)
        {
            artifacts.push(path.to_path_buf());
        }
        Ok(())
    })?;
    Ok(artifacts)
}

fn visit_dirs(root: &Path, visitor: &mut dyn FnMut(&Path) -> Result<()>) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }

    let metadata =
        fs::metadata(root).with_context(|| format!("read metadata for {}", root.display()))?;
    if metadata.is_file() {
        visitor(root)?;
        return Ok(());
    }

    for entry in fs::read_dir(root).with_context(|| format!("read dir {}", root.display()))? {
        let entry = entry.with_context(|| format!("read dir entry under {}", root.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("read file type for {}", path.display()))?;
        if file_type.is_dir() {
            visit_dirs(&path, visitor)?;
        } else if file_type.is_file() {
            visitor(&path)?;
        }
    }

    Ok(())
}

fn which_in_path(binary: &str, path_var: &str) -> Option<PathBuf> {
    for dir in env::split_paths(path_var) {
        let candidate = dir.join(binary);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }

        let nested_candidate = dir.join("bin").join(binary);
        if is_executable_file(&nested_candidate) {
            return Some(nested_candidate);
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => is_executable(metadata),
        _ => false,
    }
}

#[cfg(unix)]
fn is_executable(metadata: fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;

    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: fs::Metadata) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::repo_map_personalization::{
        RepoMapPersonalizationInput, extract_identifier_candidates,
    };
    use crate::scip_parser::{
        ParsedScipIndex, ScipFile, ScipMetadata, ScipOccurrence, ScipRange, ScipRelationship,
        ScipRelationshipKind, ScipSymbol, ScipSymbolRole,
    };
    use tempfile::TempDir;

    fn tempdir_in_tmp() -> TempDir {
        tempfile::Builder::new()
            .prefix("djinn-repo-map-")
            .tempdir_in(".")
            .expect("create test tempdir")
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("set permissions");
    }

    #[test]
    fn detect_indexers_reports_supported_binaries() {
        let tmp = tempdir_in_tmp();
        for indexer in SupportedIndexer::ALL {
            let path = tmp.path().join(indexer.binary_name());
            fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write binary");
            #[cfg(unix)]
            make_executable(&path);
        }

        let detections = detect_indexers_in_path(tmp.path().display().to_string());

        assert_eq!(detections.len(), SupportedIndexer::ALL.len());
        for detection in detections {
            assert!(detection.is_available(), "{detection:?}");
            assert_eq!(detection.path, Some(tmp.path().join(detection.binary)));
        }
    }

    #[test]
    fn plan_indexer_commands_only_includes_available_indexers() {
        let project_root = PathBuf::from("/tmp/example-project");
        let output_root = PathBuf::from("/tmp/example-project/.djinn/scip");
        let available = vec![
            IndexerAvailability {
                indexer: SupportedIndexer::RustAnalyzer,
                binary: "rust-analyzer".to_string(),
                path: Some(PathBuf::from("/tooling/rust-analyzer")),
            },
            IndexerAvailability {
                indexer: SupportedIndexer::Python,
                binary: "scip-python".to_string(),
                path: None,
            },
            IndexerAvailability {
                indexer: SupportedIndexer::TypeScript,
                binary: "scip-typescript".to_string(),
                path: Some(PathBuf::from("/tooling/scip-typescript")),
            },
        ];

        let plans = plan_indexer_commands(&project_root, &output_root, &available);

        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].indexer, SupportedIndexer::RustAnalyzer);
        assert_eq!(
            plans[0].working_directory,
            PathBuf::from("/tmp/example-project")
        );
        assert_eq!(
            plans[0].workspace_root,
            PathBuf::from("/tmp/example-project")
        );
        assert_eq!(
            plans[0].args,
            vec![
                "scip",
                ".",
                "--output",
                "/tmp/example-project/.djinn/scip/example-project-rust-root.scip"
            ]
        );
        assert_eq!(plans[1].indexer, SupportedIndexer::TypeScript);
        assert_eq!(
            plans[1].args,
            vec![
                "index",
                "/tmp/example-project/.djinn/scip/example-project-typescript-root.scip"
            ]
        );
    }

    #[test]
    fn discovers_monorepo_workspaces_per_language() {
        let tmp = tempdir_in_tmp();
        let project_root = tmp.path().join("djinn");
        fs::create_dir_all(project_root.join("server")).expect("create server dir");
        fs::create_dir_all(project_root.join("desktop")).expect("create desktop dir");
        fs::create_dir_all(project_root.join("website")).expect("create website dir");
        fs::write(
            project_root.join("server/Cargo.toml"),
            "[workspace]\nmembers = []\n",
        )
        .expect("write rust workspace");
        fs::write(project_root.join("desktop/tsconfig.json"), "{}\n")
            .expect("write desktop tsconfig");
        fs::write(
            project_root.join("website/package.json"),
            "{\"private\": true, \"workspaces\": [\"apps/*\"]}\n",
        )
        .expect("write website package.json");

        let rust_workspaces = discover_workspaces(&project_root, SupportedIndexer::RustAnalyzer);
        assert_eq!(rust_workspaces.len(), 1);
        assert_eq!(rust_workspaces[0].root, PathBuf::from("server"));
        assert_eq!(rust_workspaces[0].slug, "server");

        let ts_workspaces = discover_workspaces(&project_root, SupportedIndexer::TypeScript);
        assert_eq!(
            ts_workspaces
                .iter()
                .map(|workspace| workspace.root.clone())
                .collect::<Vec<_>>(),
            vec![PathBuf::from("desktop"), PathBuf::from("website")]
        );
        assert_eq!(
            ts_workspaces
                .iter()
                .map(|workspace| workspace.slug.as_str())
                .collect::<Vec<_>>(),
            vec!["desktop", "website"]
        );
    }

    #[test]
    fn monorepo_command_planning_emits_per_workspace_outputs() {
        let tmp = tempdir_in_tmp();
        let project_root = tmp.path().join("djinn");
        let output_root = project_root.join(".djinn/scip");
        fs::create_dir_all(project_root.join("server")).expect("create server dir");
        fs::create_dir_all(project_root.join("desktop")).expect("create desktop dir");
        fs::create_dir_all(project_root.join("website")).expect("create website dir");
        fs::write(
            project_root.join("server/Cargo.toml"),
            "[workspace]\nmembers = []\n",
        )
        .expect("write rust workspace");
        fs::write(project_root.join("desktop/tsconfig.json"), "{}\n")
            .expect("write desktop tsconfig");
        fs::write(
            project_root.join("website/package.json"),
            "{\"private\": true, \"workspaces\": [\"apps/*\"]}\n",
        )
        .expect("write website package.json");

        let available = vec![
            IndexerAvailability {
                indexer: SupportedIndexer::RustAnalyzer,
                binary: "rust-analyzer".to_string(),
                path: Some(PathBuf::from("/tooling/rust-analyzer")),
            },
            IndexerAvailability {
                indexer: SupportedIndexer::TypeScript,
                binary: "scip-typescript".to_string(),
                path: Some(PathBuf::from("/tooling/scip-typescript")),
            },
        ];

        let plans = plan_indexer_commands(&project_root, &output_root, &available);
        assert_eq!(plans.len(), 3);
        assert_eq!(plans[0].working_directory, project_root.join("server"));
        assert_eq!(plans[0].workspace_root, project_root.join("server"));
        assert_eq!(
            plans[0].output_path,
            output_root.join("djinn-rust-server.scip")
        );
        assert_eq!(
            plans[1..]
                .iter()
                .map(|plan| plan
                    .working_directory
                    .strip_prefix(&project_root)
                    .unwrap()
                    .to_path_buf())
                .collect::<Vec<_>>(),
            vec![PathBuf::from("desktop"), PathBuf::from("website")]
        );
        assert_eq!(
            plans[1..]
                .iter()
                .map(|plan| plan
                    .output_path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned())
                .collect::<Vec<_>>(),
            vec![
                "djinn-typescript-desktop.scip".to_string(),
                "djinn-typescript-website.scip".to_string()
            ]
        );
    }

    #[test]
    fn command_planning_falls_back_to_project_root_when_no_workspace_detected() {
        let project_root = PathBuf::from("/workspace/repo");
        let output_root = PathBuf::from("/workspace/repo/.djinn/scip");
        let available = vec![IndexerAvailability {
            indexer: SupportedIndexer::Python,
            binary: "scip-python".to_string(),
            path: Some(PathBuf::from("/tooling/scip-python")),
        }];

        let plans = plan_indexer_commands(&project_root, &output_root, &available);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].working_directory, project_root);
        assert_eq!(plans[0].workspace_root, PathBuf::from("/workspace/repo"));
        assert_eq!(
            plans[0].args,
            vec!["index", "/workspace/repo/.djinn/scip/repo-python-root.scip"]
        );
    }

    #[test]
    fn collect_scip_artifacts_tags_multiple_planned_outputs_per_indexer() {
        let tmp = tempdir_in_tmp();
        let output_root = tmp.path().join("out");
        fs::create_dir_all(&output_root).expect("create output dirs");

        let planned_rust = PlannedIndexerCommand {
            indexer: SupportedIndexer::RustAnalyzer,
            binary_path: PathBuf::from("/tooling/rust-analyzer"),
            args: vec![
                "scip".to_string(),
                output_root
                    .join("repo-rust-server.scip")
                    .display()
                    .to_string(),
            ],
            working_directory: PathBuf::from("/tmp/project/server"),
            workspace_root: PathBuf::from("/tmp/project/server"),
            output_path: output_root.join("repo-rust-server.scip"),
        };
        let planned_ts = PlannedIndexerCommand {
            indexer: SupportedIndexer::TypeScript,
            binary_path: PathBuf::from("/tooling/scip-typescript"),
            args: vec![
                "index".to_string(),
                output_root
                    .join("repo-typescript-desktop.scip")
                    .display()
                    .to_string(),
            ],
            working_directory: PathBuf::from("/tmp/project/desktop"),
            workspace_root: PathBuf::from("/tmp/project/desktop"),
            output_path: output_root.join("repo-typescript-desktop.scip"),
        };
        fs::write(&planned_rust.output_path, b"rust-index").expect("write rust output");
        fs::write(&planned_ts.output_path, b"ts-index").expect("write ts output");

        let artifacts = collect_scip_artifacts(
            &output_root,
            &[
                ExecutedIndexerCommand {
                    plan: planned_rust,
                    exit_code: Some(0),
                    stdout: String::new(),
                    stderr: String::new(),
                },
                ExecutedIndexerCommand {
                    plan: planned_ts,
                    exit_code: Some(0),
                    stdout: String::new(),
                    stderr: String::new(),
                },
            ],
        )
        .expect("collect artifacts");

        assert_eq!(artifacts.len(), 2);
        assert_eq!(artifacts[0].indexer, Some(SupportedIndexer::RustAnalyzer));
        assert_eq!(artifacts[1].indexer, Some(SupportedIndexer::TypeScript));
    }

    #[test]
    fn collect_scip_artifacts_finds_nested_files_and_tags_known_outputs() {
        let tmp = tempdir_in_tmp();
        let output_root = tmp.path().join("out");
        fs::create_dir_all(output_root.join("nested")).expect("create output dirs");

        let planned = PlannedIndexerCommand {
            indexer: SupportedIndexer::Go,
            binary_path: PathBuf::from("/tooling/scip-go"),
            args: vec![
                "index".to_string(),
                output_root.join("example-go.scip").display().to_string(),
            ],
            working_directory: PathBuf::from("/tmp/project"),
            workspace_root: PathBuf::from("/tmp/project"),
            output_path: output_root.join("example-go.scip"),
        };
        fs::write(&planned.output_path, b"go-index").expect("write planned output");
        let nested = output_root.join("nested").join("manual.scip");
        fs::write(&nested, b"nested").expect("write nested output");

        let artifacts = collect_scip_artifacts(
            &output_root,
            &[ExecutedIndexerCommand {
                plan: planned,
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            }],
        )
        .expect("collect artifacts");

        assert_eq!(artifacts.len(), 2);
        assert_eq!(artifacts[0].indexer, Some(SupportedIndexer::Go));
        assert_eq!(artifacts[1].indexer, None);
    }

    #[test]
    fn command_planning_covers_all_supported_indexers() {
        let project_root = PathBuf::from("/workspace/repo");
        let output_root = PathBuf::from("/workspace/repo/.djinn/scip");

        let available: Vec<_> = SupportedIndexer::ALL
            .into_iter()
            .enumerate()
            .map(|(idx, indexer)| IndexerAvailability {
                indexer,
                binary: indexer.binary_name().to_string(),
                path: Some(PathBuf::from(format!(
                    "/tool/{idx}/{}",
                    indexer.binary_name()
                ))),
            })
            .collect();

        let plans = plan_indexer_commands(&project_root, &output_root, &available);
        assert_eq!(plans.len(), SupportedIndexer::ALL.len());
        assert_eq!(
            plans.iter().map(|plan| plan.indexer).collect::<Vec<_>>(),
            SupportedIndexer::ALL
        );
        assert_eq!(
            plans[0].args,
            vec![
                "scip",
                ".",
                "--output",
                "/workspace/repo/.djinn/scip/repo-rust-root.scip"
            ]
        );
        assert_eq!(
            plans[1].args,
            vec![
                "index",
                "/workspace/repo/.djinn/scip/repo-typescript-root.scip"
            ]
        );
        assert_eq!(
            plans[2].args,
            vec!["index", "/workspace/repo/.djinn/scip/repo-python-root.scip"]
        );
        assert_eq!(
            plans[3].args,
            vec!["index", "/workspace/repo/.djinn/scip/repo-go-root.scip"]
        );
        assert_eq!(
            plans[4].args,
            vec!["index", "/workspace/repo/.djinn/scip/repo-java-root.scip"]
        );
    }

    #[test]
    fn collect_scip_artifacts_ignores_missing_root() {
        let missing = PathBuf::from("/tmp/does-not-exist-djinn-scip");
        let artifacts = collect_scip_artifacts(&missing, &[]).expect("collect artifacts");
        assert!(artifacts.is_empty());
    }

    #[test]
    fn render_repo_map_is_deterministic_and_budget_aware() {
        let graph = RepoDependencyGraph::build(&[fixture_index()]);
        let ranking = graph.rank();
        let options = RepoMapRenderOptions::new(120);

        let first = render_repo_map(&graph, &ranking, &options).expect("render succeeds");
        let second = render_repo_map(&graph, &ranking, &options).expect("render succeeds");

        assert_eq!(first, second);
        assert!(first.token_estimate <= options.token_budget);
        assert!(first.content.contains("# Repository Map"));
        assert!(first.content.contains("src/helper.rs") || first.content.contains("src/app.rs"));
    }

    #[test]
    fn render_repo_map_shrinks_with_budget_using_bounded_search() {
        let graph = RepoDependencyGraph::build(&[fixture_index()]);
        let ranking = graph.rank();

        let roomy = render_repo_map(&graph, &ranking, &RepoMapRenderOptions::new(300))
            .expect("roomy render succeeds");
        let tight = render_repo_map(
            &graph,
            &ranking,
            &RepoMapRenderOptions {
                token_budget: 90,
                max_files: 1,
                max_symbols_per_file: 1,
                max_relationships_per_file: 1,
            },
        )
        .expect("tight render succeeds");

        assert!(roomy.included_entries > tight.included_entries);
        assert!(tight.token_estimate <= 90);
    }

    #[test]
    fn render_repo_map_reports_when_minimal_representation_cannot_fit() {
        let graph = RepoDependencyGraph::build(&[fixture_index()]);
        let ranking = graph.rank();

        let err = render_repo_map(&graph, &ranking, &RepoMapRenderOptions::new(10))
            .expect_err("budget should be too small");

        assert!(matches!(
            err,
            RepoMapRenderError::MinimalRepresentationExceedsBudget { .. }
        ));
    }

    #[test]
    fn repo_map_note_spec_uses_repo_map_folder_and_stable_permalink() {
        let spec = repo_map_note_spec("abcdef1234567890");
        assert_eq!(spec.title, "Repository Map abcdef123456");
        assert_eq!(spec.permalink, "reference/repo-maps/abcdef123456");
        assert!(spec.tags_json.contains("repo-map"));
        assert!(spec.tags_json.contains("abcdef123456"));
    }

    #[test]
    fn personalized_identifier_extraction_filters_low_signal_tokens() {
        let memory_refs = vec![
            "decisions/adr-043-repository-map-scip-powered-structural-context-for-agent-sessions"
                .to_string(),
            "notes/RepoMapQueryHelper".to_string(),
        ];
        let identifiers = extract_identifier_candidates(&RepoMapPersonalizationInput {
            title: Some("Phase 2: Extract task-aware identifiers for RepoMapQueryHelper"),
            description: Some(
                "Parse session title/description/design text and prefer repo_map.rs plus TaskSession42.",
            ),
            design: Some(
                "Bias selection toward repo_map_personalization.rs and relationship display text like symbol-ref repo_map.rs",
            ),
            memory_refs: &memory_refs,
        });

        assert!(identifiers.contains(&"repomapqueryhelper".to_string()));
        assert!(identifiers.contains(&"tasksession42".to_string()));
        assert!(identifiers.contains(&"repository".to_string()));
        assert!(identifiers.contains(&"scip".to_string()));
        assert!(!identifiers.contains(&"task".to_string()));
        assert!(!identifiers.contains(&"map".to_string()));
        assert!(!identifiers.contains(&"043".to_string()));
    }

    #[test]
    fn personalized_repo_map_ranking_boosts_matching_file_symbol_and_relationship_entries() {
        let graph = RepoDependencyGraph::build(&[fixture_index()]);
        let ranking = graph.rank();
        let memory_refs = vec!["docs/other-note".to_string()];
        let note_hints = vec![RepoMapNoteHint {
            permalink: "docs/helpertrait".to_string(),
            title: "HelperTrait notes".to_string(),
            snippet: "src/helper.rs helper HelperTrait symbol-ref src/helper.rs".to_string(),
            normalized_tokens: vec![
                "helpertrait".to_string(),
                "helper".to_string(),
                "src".to_string(),
            ],
        }];

        let personalized = personalized_repo_map_ranking(
            &graph,
            &RepoMapPersonalizationRequest {
                ranked_nodes: &ranking.nodes,
                title: Some("Investigate implementation details"),
                description: Some("Need note-linked helper concepts"),
                design: Some("Prefer note-linked relationship display text"),
                memory_refs: &memory_refs,
                note_hints: &note_hints,
            },
        );

        let personalized_files = personalized
            .iter()
            .filter(|node| node.kind == RepoGraphNodeKind::File)
            .map(|node| graph.node(node.node_index).display_name.clone())
            .collect::<Vec<_>>();

        assert_eq!(personalized_files[0], "src/app.rs");
        assert_eq!(personalized_files[1], "src/helper.rs");
    }

    #[test]
    fn personalized_repo_map_ranking_preserves_baseline_order_without_note_hints() {
        let graph = RepoDependencyGraph::build(&[fixture_index()]);
        let ranking = graph.rank();

        let personalized = personalized_repo_map_ranking(
            &graph,
            &RepoMapPersonalizationRequest {
                ranked_nodes: &ranking.nodes,
                title: None,
                description: None,
                design: None,
                memory_refs: &[],
                note_hints: &[],
            },
        );

        assert_eq!(personalized, ranking.nodes);
    }

    fn fixture_index() -> ParsedScipIndex {
        let helper_symbol_name = "scip-rust pkg src/helper.rs `helper`().".to_string();
        let helper_symbol = ScipSymbol {
            symbol: helper_symbol_name.clone(),
            kind: Some(ScipSymbolKind::Function),
            display_name: Some("helper".to_string()),
            signature: Some("fn helper()".to_string()),
            documentation: vec!["returns a value".to_string()],
            relationships: vec![],
        };
        let trait_symbol = ScipSymbol {
            symbol: "scip-rust pkg src/types.rs `HelperTrait`#".to_string(),
            kind: Some(ScipSymbolKind::Type),
            display_name: Some("HelperTrait".to_string()),
            signature: None,
            documentation: vec![],
            relationships: vec![],
        };
        let main_symbol = ScipSymbol {
            symbol: "scip-rust pkg src/app.rs `main`().".to_string(),
            kind: Some(ScipSymbolKind::Function),
            display_name: Some("main".to_string()),
            signature: Some("fn main()".to_string()),
            documentation: vec![],
            relationships: vec![ScipRelationship {
                source_symbol: "scip-rust pkg src/app.rs `main`().".to_string(),
                target_symbol: "scip-rust pkg src/types.rs `HelperTrait`#".to_string(),
                kinds: BTreeSet::from([ScipRelationshipKind::Implementation]),
            }],
        };

        ParsedScipIndex {
            metadata: ScipMetadata {
                project_root: Some("file:///workspace/repo".to_string()),
                tool_name: Some("rust-analyzer".to_string()),
                tool_version: Some("1.0.0".to_string()),
            },
            files: vec![
                ScipFile {
                    language: "rust".to_string(),
                    relative_path: PathBuf::from("src/helper.rs"),
                    definitions: vec![definition_occurrence(&helper_symbol_name)],
                    references: vec![],
                    occurrences: vec![definition_occurrence(&helper_symbol_name)],
                    symbols: vec![helper_symbol],
                },
                ScipFile {
                    language: "rust".to_string(),
                    relative_path: PathBuf::from("src/app.rs"),
                    definitions: vec![definition_occurrence(&main_symbol.symbol)],
                    references: vec![reference_occurrence(&helper_symbol_name)],
                    occurrences: vec![
                        definition_occurrence(&main_symbol.symbol),
                        reference_occurrence(&helper_symbol_name),
                    ],
                    symbols: vec![main_symbol, trait_symbol],
                },
            ],
            external_symbols: vec![],
        }
    }

    fn definition_occurrence(symbol: &str) -> ScipOccurrence {
        ScipOccurrence {
            symbol: symbol.to_string(),
            range: ScipRange {
                start_line: 0,
                start_character: 0,
                end_line: 0,
                end_character: 6,
            },
            enclosing_range: None,
            roles: BTreeSet::from([ScipSymbolRole::Definition]),
            syntax_kind: None,
            override_documentation: vec![],
        }
    }

    fn reference_occurrence(symbol: &str) -> ScipOccurrence {
        ScipOccurrence {
            symbol: symbol.to_string(),
            range: ScipRange {
                start_line: 1,
                start_character: 4,
                end_line: 1,
                end_character: 10,
            },
            enclosing_range: None,
            roles: BTreeSet::from([ScipSymbolRole::ReadAccess]),
            syntax_kind: None,
            override_documentation: vec![],
        }
    }
}
