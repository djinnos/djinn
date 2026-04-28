//! Glue between the canonical-graph warm pipeline and the chunk-and-embed
//! pipeline that lives in `djinn-db`.
//!
//! The warm path (`canonical_graph::ensure_canonical_graph`) calls
//! [`spawn_chunk_and_embed_pass`] right after it has installed the canonical
//! graph in RAM. The spawned task:
//!
//! 1. Bails immediately when `DJINN_CODE_CHUNKS_BACKEND` isn't set to
//!    `qdrant`. Empty / unset is the default rollout state per the plan
//!    (env var lives in §"Engineering Practices → Feature flags").
//! 2. Tries to claim the per-project in-flight slot via
//!    [`djinn_db::try_claim_project`]; coalesces (no-op) when another
//!    pass is already running.
//! 3. Walks the canonical graph's `symbol_ranges_by_file` index, reads
//!    each file off disk, builds a [`FileInput`] per file, and feeds the
//!    batch through [`djinn_db::chunk_and_embed_files`].
//! 4. Logs the resulting [`djinn_db::ChunkAndEmbedReport`]. Errors are
//!    swallowed (warmer runs unblocked by embedding failures).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use djinn_db::repositories::code_chunk::chunker::{
    ChunkConfig, FileInput, RepoMetadata, SymbolChunkKind, SymbolInput,
};
use djinn_db::{
    CodeChunkEmbeddingProvider, CodeChunkVectorStore, Database, ProjectRepository,
    chunk_and_embed_files, try_claim_project,
};

use crate::repo_graph::{RepoDependencyGraph, RepoGraphNodeKind, SymbolRange};
use crate::scip_parser::ScipSymbolKind;
use crate::scip_parser::ScipVisibility;

/// Side-effect-free env probe so callers can short-circuit before doing
/// any I/O (matching the "feature gate" rule in the plan: function returns
/// early with no work when the flag is unset).
pub fn code_chunks_backend_enabled() -> bool {
    std::env::var("DJINN_CODE_CHUNKS_BACKEND")
        .map(|v| v.eq_ignore_ascii_case("qdrant"))
        .unwrap_or(false)
}

/// Fire-and-forget kickoff used by the warmer. Returns immediately —
/// the actual pass runs on a detached `tokio::spawn`. Never blocks.
///
/// Skips when:
/// * `DJINN_CODE_CHUNKS_BACKEND` isn't `qdrant`.
/// * Another pass for the same project is already in flight (coalesced).
pub fn spawn_chunk_and_embed_pass(
    db: Database,
    embeddings: Arc<dyn CodeChunkEmbeddingProvider>,
    vector_store: Arc<dyn CodeChunkVectorStore>,
    graph: Arc<RepoDependencyGraph>,
    project_id: String,
    project_root: PathBuf,
) {
    if !code_chunks_backend_enabled() {
        return;
    }
    tokio::spawn(async move {
        let guard = match try_claim_project(&project_id).await {
            Some(guard) => guard,
            None => {
                tracing::debug!(
                    project_id = %project_id,
                    "chunk_and_embed: another pass already in flight; coalescing"
                );
                return;
            }
        };

        let started = std::time::Instant::now();
        match run_chunk_and_embed_pass(
            &db,
            embeddings,
            vector_store,
            graph,
            &project_id,
            &project_root,
        )
        .await
        {
            Ok(report) => {
                tracing::info!(
                    project_id = %project_id,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    chunks_total = report.chunks_total,
                    chunks_embedded = report.chunks_embedded,
                    chunks_ready = report.chunks_ready,
                    chunks_pending = report.chunks_pending,
                    chunks_embed_failed = report.chunks_embed_failed,
                    chunks_skipped_stale_match = report.chunks_skipped_stale_match,
                    "chunk_and_embed: pass complete"
                );
            }
            Err(error) => {
                tracing::warn!(
                    project_id = %project_id,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    %error,
                    "chunk_and_embed: pass failed (logged, not surfaced to warmer)"
                );
            }
        }
        drop(guard);
    });
}

/// Direct (non-spawning) entrypoint. The warmer uses
/// [`spawn_chunk_and_embed_pass`]; tests and the repair tool can call
/// this synchronously when they want the report back.
pub async fn run_chunk_and_embed_pass(
    db: &Database,
    embeddings: Arc<dyn CodeChunkEmbeddingProvider>,
    vector_store: Arc<dyn CodeChunkVectorStore>,
    graph: Arc<RepoDependencyGraph>,
    project_id: &str,
    project_root: &Path,
) -> Result<djinn_db::ChunkAndEmbedReport, String> {
    let project = ProjectRepository::new(db.clone(), djinn_core::events::EventBus::noop())
        .get(project_id)
        .await
        .map_err(|e| format!("lookup project {project_id}: {e}"))?
        .ok_or_else(|| format!("project {project_id} not found"))?;

    let owner = project.github_owner;
    let repo = project.github_repo;

    // First pass: collect (path, content, symbols) tuples up-front so we
    // can hand `chunk_and_embed_files` a borrow-friendly slice. Reading
    // every file's contents up-front is the simple correct shape — most
    // canonical-graph projects fit in tens of MB of source.
    struct PreparedFile {
        path: String,
        content: String,
        symbols: Vec<SymbolInput>,
    }
    let mut prepared: Vec<PreparedFile> = Vec::new();
    for (rel_path, ranges) in graph.symbol_ranges_by_file() {
        let abs_path = project_root.join(rel_path);
        let content = match tokio::fs::read_to_string(&abs_path).await {
            Ok(s) => s,
            Err(error) => {
                tracing::debug!(
                    %error,
                    path = %abs_path.display(),
                    "chunk_and_embed: skipping file (read failed; likely outside index_tree or non-utf8)"
                );
                continue;
            }
        };

        let symbols = collect_symbols_for_file(graph.as_ref(), ranges);
        if symbols.is_empty() {
            continue;
        }
        prepared.push(PreparedFile {
            path: rel_path.to_string_lossy().into_owned(),
            content,
            symbols,
        });
    }

    if prepared.is_empty() {
        tracing::debug!(
            project_id,
            "chunk_and_embed: no symbols found in graph index — skipping pass"
        );
        return Ok(djinn_db::ChunkAndEmbedReport::default());
    }

    let file_inputs: Vec<FileInput<'_>> = prepared
        .iter()
        .map(|p| FileInput {
            path: p.path.as_str(),
            content: p.content.as_str(),
            symbols: p.symbols.as_slice(),
        })
        .collect();

    let report = chunk_and_embed_files(
        db,
        embeddings,
        vector_store,
        project_id,
        RepoMetadata {
            owner: owner.as_str(),
            repo: repo.as_str(),
        },
        &file_inputs,
        ChunkConfig::default(),
    )
    .await
    .map_err(|e| format!("chunk_and_embed_files: {e}"))?;

    Ok(report)
}

/// Build the per-file `SymbolInput` list from the graph's symbol ranges.
/// Skips file nodes (we only chunk symbols), external symbols, and
/// nodes whose enclosing range is missing data.
fn collect_symbols_for_file(
    graph: &RepoDependencyGraph,
    ranges: &[SymbolRange],
) -> Vec<SymbolInput> {
    let mut out: Vec<SymbolInput> = Vec::with_capacity(ranges.len());
    for range in ranges {
        let node = graph.node(range.node);
        if node.kind != RepoGraphNodeKind::Symbol || node.is_external {
            continue;
        }
        let Some(symbol_key) = node.symbol.clone() else {
            continue;
        };
        let kind = chunker_kind_for_scip(node.symbol_kind.as_ref());
        let is_export = matches!(node.visibility, Some(ScipVisibility::Public));
        out.push(SymbolInput {
            symbol_key,
            display_name: node.display_name.clone(),
            kind,
            start_line: range.start_line,
            end_line: range.end_line,
            is_export,
            signature: node.signature.clone(),
            documentation: node.documentation.clone(),
        });
    }
    out
}

fn chunker_kind_for_scip(kind: Option<&ScipSymbolKind>) -> SymbolChunkKind {
    match kind {
        Some(ScipSymbolKind::Function | ScipSymbolKind::Method | ScipSymbolKind::Constructor) => {
            SymbolChunkKind::Function
        }
        Some(
            ScipSymbolKind::Type
            | ScipSymbolKind::Struct
            | ScipSymbolKind::Enum
            | ScipSymbolKind::Interface,
        ) => SymbolChunkKind::Declaration,
        Some(
            ScipSymbolKind::Field
            | ScipSymbolKind::Property
            | ScipSymbolKind::Variable
            | ScipSymbolKind::Constant
            | ScipSymbolKind::EnumMember,
        ) => SymbolChunkKind::Field,
        _ => SymbolChunkKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::{create_test_db, workspace_tempdir};
    use djinn_core::events::EventBus;
    use djinn_db::{
        EmbeddedCodeChunk, NoopCodeChunkVectorStore, ProjectRepository,
        repositories::code_chunk::CodeChunkRepository,
    };

    /// Embedding provider that returns a fixed 8-d vector.
    struct FakeProvider;

    #[async_trait::async_trait]
    impl djinn_db::CodeChunkEmbeddingProvider for FakeProvider {
        fn model_version(&self) -> String {
            "fake-graph-test@v1".to_string()
        }
        async fn embed_chunk(&self, _text: &str) -> std::result::Result<EmbeddedCodeChunk, String> {
            Ok(EmbeddedCodeChunk {
                values: vec![0.1_f32; 8],
                model_version: "fake-graph-test@v1".to_string(),
            })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_chunk_and_embed_pass_walks_graph_symbol_ranges() {
        // Build a repo on disk that matches a known graph fixture.  The
        // chunk pipeline reads files off disk via project_root, so the
        // test has to materialize source text the symbol_ranges point at.
        let tmp = workspace_tempdir("chunk-and-embed-");
        let project_root = tmp.path().join("repo");
        let src_dir = project_root.join("src");
        tokio::fs::create_dir_all(&src_dir).await.unwrap();

        // Mirror the line ranges built into `build_test_parsed_index_fixture`:
        //   helper.rs::helper:    line 1   (def_occ uses 0-indexed -> 1-indexed = 1)
        //   app.rs::main:         line 1
        //
        // Both are 1-line definitions so the chunker emits 1 small chunk
        // per symbol.  The full file body must contain those lines.
        let helper_src = "fn helper() { let answer = 42; }\n";
        let app_src = "fn main() { helper(); }\n";
        tokio::fs::write(src_dir.join("helper.rs"), helper_src)
            .await
            .unwrap();
        tokio::fs::write(src_dir.join("app.rs"), app_src)
            .await
            .unwrap();

        let db = create_test_db();
        // Project row required by run_chunk_and_embed_pass for the
        // owner/repo metadata.
        let proj = ProjectRepository::new(db.clone(), EventBus::noop())
            .create("chunk-embed-graph-test", "djinnos", "djinn")
            .await
            .unwrap();

        // Build a graph fixture whose symbol_ranges actually have data.
        // The canonical graph helper `build_test_parsed_index_fixture`
        // uses def_occ() with no enclosing_range, which means
        // symbol_ranges is empty after build. Use the nested-ranges
        // fixture-style definitions but tailored so source files exist.
        use crate::scip_parser::{
            ParsedScipIndex, ScipFile, ScipMetadata, ScipOccurrence, ScipRange,
            ScipRelationshipKind, ScipSymbol, ScipSymbolKind, ScipSymbolRole,
        };
        use std::collections::BTreeSet;
        let _ = ScipRelationshipKind::Implementation;

        fn def_with_enclosing(symbol: &str, start: i32, end: i32) -> ScipOccurrence {
            ScipOccurrence {
                symbol: symbol.to_string(),
                range: ScipRange {
                    start_line: start,
                    start_character: 0,
                    end_line: start,
                    end_character: 6,
                },
                enclosing_range: Some(ScipRange {
                    start_line: start,
                    start_character: 0,
                    end_line: end,
                    end_character: 0,
                }),
                roles: BTreeSet::from([ScipSymbolRole::Definition]),
                syntax_kind: None,
                override_documentation: vec![],
            }
        }

        let helper_sym_id = "scip-rust pkg src/helper.rs `helper`().".to_string();
        let main_sym_id = "scip-rust pkg src/app.rs `main`().".to_string();

        let parsed = ParsedScipIndex {
            metadata: ScipMetadata::default(),
            files: vec![
                ScipFile {
                    language: "rust".to_string(),
                    relative_path: std::path::PathBuf::from("src/helper.rs"),
                    definitions: vec![def_with_enclosing(&helper_sym_id, 0, 0)],
                    references: vec![],
                    occurrences: vec![def_with_enclosing(&helper_sym_id, 0, 0)],
                    symbols: vec![ScipSymbol {
                        symbol: helper_sym_id.clone(),
                        kind: Some(ScipSymbolKind::Function),
                        display_name: Some("helper".to_string()),
                        signature: Some("fn helper()".to_string()),
                        documentation: vec![],
                        relationships: vec![],
                        visibility: Some(crate::scip_parser::ScipVisibility::Public),
                    }],
                },
                ScipFile {
                    language: "rust".to_string(),
                    relative_path: std::path::PathBuf::from("src/app.rs"),
                    definitions: vec![def_with_enclosing(&main_sym_id, 0, 0)],
                    references: vec![],
                    occurrences: vec![def_with_enclosing(&main_sym_id, 0, 0)],
                    symbols: vec![ScipSymbol {
                        symbol: main_sym_id.clone(),
                        kind: Some(ScipSymbolKind::Function),
                        display_name: Some("main".to_string()),
                        signature: Some("fn main()".to_string()),
                        documentation: vec![],
                        relationships: vec![],
                        visibility: Some(crate::scip_parser::ScipVisibility::Public),
                    }],
                },
            ],
            external_symbols: vec![],
        };
        let graph = Arc::new(RepoDependencyGraph::build(&[parsed]));

        let embeddings: Arc<dyn djinn_db::CodeChunkEmbeddingProvider> = Arc::new(FakeProvider);
        let store: Arc<dyn djinn_db::CodeChunkVectorStore> = Arc::new(NoopCodeChunkVectorStore);

        let report = run_chunk_and_embed_pass(
            &db,
            embeddings,
            store,
            graph,
            &proj.id,
            &project_root,
        )
        .await
        .expect("pass succeeds");

        assert!(
            report.chunks_total >= 2,
            "expected ≥2 chunks (one per symbol), got {}",
            report.chunks_total
        );
        assert_eq!(report.chunks_embedded, report.chunks_total);

        // DB round-trip — both chunks landed.
        let rows = CodeChunkRepository::new(db)
            .list_repair_embedding_rows(&proj.id)
            .await
            .unwrap();
        assert!(rows.len() >= 2, "expected ≥2 chunk rows in DB");
        let file_paths: std::collections::HashSet<_> =
            rows.iter().map(|r| r.file_path.clone()).collect();
        assert!(file_paths.contains("src/helper.rs"));
        assert!(file_paths.contains("src/app.rs"));
    }
}
