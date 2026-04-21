use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use anyhow::Result;
use djinn_db::{NoteRepository, NoteSearchParams};
use serde::{Deserialize, Serialize};

use petgraph::visit::EdgeRef;

use crate::repo_graph::{
    RankedRepoGraphNode, RepoDependencyGraph, RepoGraphEdgeKind, RepoGraphNodeKind,
    RepoGraphRanking,
};
use crate::repo_map_personalization::RepoMapNoteSearcher;
#[cfg(test)]
use crate::repo_map_personalization::{RepoMapNoteHint, RepoMapPersonalizationInput};
use crate::scip_parser::ScipSymbolKind;

mod indexing;
#[cfg(test)]
mod tests;
mod workspaces;

pub(crate) use indexing::run_indexers_already_locked;

/// Tracks `(project_root, indexer)` pairs we have already logged a
/// "missing indexer binary" notice for, so the periodic repo-map refresh
/// does not spam the same info line every cycle.
static MISSING_INDEXER_LOGGED: Mutex<Option<HashSet<(PathBuf, SupportedIndexer)>>> =
    Mutex::new(None);

fn note_missing_indexer_once(project_root: &Path, indexer: SupportedIndexer) -> bool {
    let mut guard = match MISSING_INDEXER_LOGGED.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let set = guard.get_or_insert_with(HashSet::new);
    set.insert((project_root.to_path_buf(), indexer))
}
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
    Clang,
    Ruby,
    DotNet,
}

impl SupportedIndexer {
    pub const ALL: [Self; 8] = [
        Self::RustAnalyzer,
        Self::TypeScript,
        Self::Python,
        Self::Go,
        Self::Java,
        Self::Clang,
        Self::Ruby,
        Self::DotNet,
    ];

    pub fn binary_name(self) -> &'static str {
        match self {
            Self::RustAnalyzer => "rust-analyzer",
            Self::TypeScript => "scip-typescript",
            Self::Python => "scip-python",
            Self::Go => "scip-go",
            Self::Java => "scip-java",
            Self::Clang => "scip-clang",
            Self::Ruby => "scip-ruby",
            Self::DotNet => "scip-dotnet",
        }
    }

    pub fn language(self) -> &'static str {
        match self {
            Self::RustAnalyzer => "rust",
            Self::TypeScript => "typescript",
            Self::Python => "python",
            Self::Go => "go",
            Self::Java => "java",
            Self::Clang => "cpp",
            Self::Ruby => "ruby",
            Self::DotNet => "csharp",
        }
    }

    fn marker_files(self) -> &'static [&'static str] {
        match self {
            Self::RustAnalyzer => &["Cargo.toml"],
            Self::TypeScript => &["tsconfig.json", "package.json"],
            Self::Python => &["pyproject.toml", "setup.py"],
            Self::Go => &["go.mod"],
            Self::Java => &["build.gradle", "pom.xml"],
            Self::Clang => &["CMakeLists.txt", "compile_commands.json"],
            Self::Ruby => &["Gemfile"],
            Self::DotNet => &["*.csproj", "*.sln"],
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
            Self::Clang => vec![
                "--compdb-path".to_string(),
                ".".to_string(),
                "--output-path".to_string(),
                output,
            ],
            Self::Ruby => vec!["index".to_string(), output],
            Self::DotNet => vec!["index".to_string(), output],
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
        // ADR-050 §3 non-negotiable cap: SCIP indexers commonly invoke
        // `cargo check` which fans out into a parallel `cc` build for
        // native deps (openssl-sys et al).  Two simultaneous indexer
        // runs are already serialized by `IndexerLock`, but a single
        // run can still saturate the host without this cap.  4 jobs is
        // empirically sufficient to keep `rust-analyzer scip` warm
        // without melting the box.
        command.env("CARGO_BUILD_JOBS", "4");
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

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Default)]
struct RepoMapPersonalizationRequest<'a> {
    ranked_nodes: &'a [RankedRepoGraphNode],
    title: Option<&'a str>,
    description: Option<&'a str>,
    design: Option<&'a str>,
    memory_refs: &'a [String],
    note_hints: &'a [RepoMapNoteHint],
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
        NoteRepository::search(
            self,
            NoteSearchParams {
                project_id,
                query,
                task_id,
                folder: None,
                note_type: None,
                limit,
                semantic_scores: None,
            },
        )
        .await
    }
}

#[allow(private_interfaces)]
pub fn render_repo_map(
    graph: &RepoDependencyGraph,
    ranking: &RepoGraphRanking,
    options: &RepoMapRenderOptions,
) -> Result<RenderedRepoMap, RepoMapRenderError> {
    let entries = build_repo_map_entries(graph, &ranking.nodes, options);
    render_repo_map_from_entries(&entries, options)
}

#[cfg(test)]
fn personalized_repo_map_ranking(
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

#[cfg(test)]
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
