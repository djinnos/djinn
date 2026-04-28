//! PR B4 hybrid `code_graph search` orchestrator.
//!
//! Composes three signals into a single ranked list via Reciprocal Rank
//! Fusion (k=60):
//!
//! 1. **Lexical** — `LIKE %query%` over `code_chunks.embedded_text`
//!    (and `symbol_key` / `file_path` as boost surfaces). Lives in
//!    `djinn-db::repositories::code_chunk::search`. Lexical strategy is
//!    `LIKE` rather than Dolt FULLTEXT because the `code_chunks` table
//!    has no FULLTEXT index today and Dolt's parameterised `MATCH ...
//!    AGAINST (?)` path is fragile (per the notes-side workaround).
//!    The plan accepts: "or fall back to Tantivy/in-memory if Dolt FTS
//!    limits hit." `LIKE` is the simplest fallback — when Dolt's
//!    FULLTEXT support stabilises for `code_chunks`, swap it in
//!    `djinn-db::code_chunk::search` without touching this file.
//! 2. **Semantic** — Qdrant cosine search on the `code_chunks`
//!    collection, using the same `nomic-embed-text-v1.5` query
//!    embedding as the notes pipeline.
//! 3. **Structural** — `RepoDependencyGraph::search_by_name`, the same
//!    name-index hits the `mode=name` fast path returns.
//!
//! Per-signal hits are capped at top-3-per-file *before* fusion so a
//! single test file with 30 assertion chunks can't dominate.
//!
//! ## Cache
//!
//! A 30s in-process cache keyed by `(project_id, sha256(query))` short-
//! circuits the orchestrator. Stored as a `std::sync::RwLock<HashMap>`
//! — no new dep, no cross-await holds. The cache is per-process; a
//! restart re-warms on first call.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, Instant};

use djinn_control_plane::bridge::{ProjectCtx, SearchHit};
use djinn_db::CodeChunkSearchHit;
use djinn_db::repositories::note::rrf::rrf_fuse;
use sha2::{Digest, Sha256};

use crate::server::AppState;

const CACHE_TTL: Duration = Duration::from_secs(30);
const PER_FILE_CAP: usize = 3;
const RRF_K: f64 = 60.0;

#[derive(Clone)]
struct CacheEntry {
    inserted_at: Instant,
    hits: Vec<SearchHit>,
}

fn cache() -> &'static RwLock<HashMap<String, CacheEntry>> {
    static CACHE: OnceLock<RwLock<HashMap<String, CacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

fn cache_key(project_id: &str, query: &str, kind_filter: Option<&str>, limit: usize) -> String {
    // SHA-256 over `(project_id, query, kind_filter, limit)` so two
    // queries with the same text but different kind filters / limits
    // don't share a cache entry.
    let mut hasher = Sha256::new();
    hasher.update(project_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(query.as_bytes());
    hasher.update(b"\0");
    hasher.update(kind_filter.unwrap_or("").as_bytes());
    hasher.update(b"\0");
    hasher.update(limit.to_le_bytes());
    let digest = hasher.finalize();
    format!("{:x}", digest)
}

fn read_cached(key: &str) -> Option<Vec<SearchHit>> {
    let guard = cache().read().ok()?;
    let entry = guard.get(key)?;
    if entry.inserted_at.elapsed() <= CACHE_TTL {
        Some(entry.hits.clone())
    } else {
        None
    }
}

fn write_cache(key: String, hits: Vec<SearchHit>) {
    if let Ok(mut guard) = cache().write() {
        // Opportunistic GC: drop expired entries on every write so the
        // map doesn't grow unboundedly across long-lived processes.
        guard.retain(|_, entry| entry.inserted_at.elapsed() <= CACHE_TTL);
        guard.insert(
            key,
            CacheEntry {
                inserted_at: Instant::now(),
                hits,
            },
        );
    }
}

/// Test-only: clear the cache so successive tests don't observe each
/// other's writes.
#[cfg(test)]
pub(crate) fn clear_cache_for_tests() {
    if let Ok(mut guard) = cache().write() {
        guard.clear();
    }
}

/// Cap a list of code-chunk hits to `PER_FILE_CAP` per file. Wrapper so
/// the orchestrator stays declarative.
fn cap_hits(hits: Vec<CodeChunkSearchHit>) -> Vec<CodeChunkSearchHit> {
    djinn_db::cap_per_file(hits, PER_FILE_CAP)
}

/// Convert a `CodeChunkSearchHit` to the bridge's `SearchHit` shape.
/// Symbol-bearing chunks key on the symbol_key (so callers can pass
/// the result back into `neighbors`/`impact`/`context`); chunks without
/// a symbol fall back to a `chunk:<id>` synthetic key the UI can
/// render but the structural ops will reject — that's intentional, so
/// non-symbol hits don't poison downstream graph queries.
fn chunk_hit_to_search_hit(hit: &CodeChunkSearchHit, match_kind: &str) -> SearchHit {
    let key = hit
        .symbol_key
        .clone()
        .unwrap_or_else(|| format!("chunk:{}", hit.chunk_id));
    let display_name = hit
        .symbol_key
        .as_deref()
        .and_then(|s| s.rsplit_once(['#', '/']).map(|(_, name)| name.to_string()))
        .unwrap_or_else(|| {
            // Fall back to the file path's tail so the UI never shows
            // an empty name.
            hit.file_path
                .rsplit('/')
                .next()
                .unwrap_or(hit.file_path.as_str())
                .to_string()
        });
    SearchHit {
        key,
        kind: hit.kind.clone(),
        display_name,
        score: hit.score,
        file: Some(hit.file_path.clone()),
        match_kind: Some(match_kind.to_string()),
    }
}

/// Run the three-signal hybrid search and return up to `limit` hits.
pub(crate) async fn run(
    state: &AppState,
    ctx: &ProjectCtx,
    query: &str,
    kind_filter: Option<&str>,
    limit: usize,
) -> Result<Vec<SearchHit>, String> {
    if query.is_empty() {
        return Ok(vec![]);
    }
    let key = cache_key(&ctx.id, query, kind_filter, limit);
    if let Some(hits) = read_cached(&key) {
        return Ok(hits);
    }

    // Per-signal fetch budget. We over-fetch (4×) so the post-fusion
    // top-K still has headroom after per-file capping and de-dup.
    let signal_limit = (limit.saturating_mul(4)).clamp(limit, 200);

    let lexical = run_lexical_signal(state, &ctx.id, query, signal_limit);
    let semantic = run_semantic_signal(state, &ctx.id, query, signal_limit);
    let structural = run_structural_signal(state, ctx, query, kind_filter, signal_limit);

    // Run all three signals concurrently — they hit independent
    // backends (Dolt SQL, Qdrant, in-memory canonical graph) so there's
    // no contention to worry about.
    let (lex_hits, sem_hits, struct_hits) = tokio::join!(lexical, semantic, structural);

    let lex_hits = lex_hits.unwrap_or_else(|error| {
        tracing::debug!(%error, "hybrid_search: lexical signal failed; treating as empty");
        Vec::new()
    });
    let sem_hits = sem_hits.unwrap_or_else(|error| {
        tracing::debug!(%error, "hybrid_search: semantic signal failed; treating as empty");
        Vec::new()
    });
    let struct_hits = struct_hits.unwrap_or_else(|error| {
        tracing::debug!(%error, "hybrid_search: structural signal failed; treating as empty");
        Vec::new()
    });

    let fused = fuse_signals(lex_hits, sem_hits, struct_hits, limit);
    write_cache(key, fused.clone());
    Ok(fused)
}

/// Lexical signal: `LIKE %query%` against `code_chunks`.
async fn run_lexical_signal(
    state: &AppState,
    project_id: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<CodeChunkSearchHit>, String> {
    let hits = djinn_db::lexical_search_chunks(state.db(), project_id, query, limit)
        .await
        .map_err(|e| e.to_string())?;
    Ok(cap_hits(hits))
}

/// Semantic signal: Qdrant cosine over `code_chunks`. Returns empty when
/// the embedding is degraded or the vector store is unavailable.
async fn run_semantic_signal(
    state: &AppState,
    project_id: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<CodeChunkSearchHit>, String> {
    let outcome = state.embedding_service().embed_query(query).await;
    let embedding = match outcome {
        djinn_provider::embeddings::EmbeddingOutcome::Ready(vector) => vector.values,
        djinn_provider::embeddings::EmbeddingOutcome::Degraded(_) => return Ok(vec![]),
    };

    let store = state.code_chunk_vector_store();
    let matches = store
        .query_similar(project_id, &embedding, limit)
        .await
        .map_err(|e| e.to_string())?;
    if matches.is_empty() {
        return Ok(vec![]);
    }

    let scored: Vec<(String, f64)> = matches
        .into_iter()
        .map(|m| (m.chunk_id, m.score))
        .collect();
    let hits = djinn_db::hydrate_chunk_ids(state.db(), project_id, &scored)
        .await
        .map_err(|e| e.to_string())?;
    Ok(cap_hits(hits))
}

/// Structural signal: existing canonical-graph name index. Returns
/// `SearchHit`s directly (no chunk join needed) but we wrap them in the
/// chunk-shaped intermediate type so the fuser sees a single contract.
///
/// File-level capping doesn't apply here — the name index already
/// produces at most one hit per node, and there's no chunk-of-test-file
/// failure mode to defang.
async fn run_structural_signal(
    state: &AppState,
    ctx: &ProjectCtx,
    query: &str,
    kind_filter: Option<&str>,
    limit: usize,
) -> Result<Vec<SearchHit>, String> {
    use djinn_graph::repo_graph::RepoGraphNodeKind;
    let graph =
        djinn_graph::canonical_graph::load_canonical_graph_only(state, &ctx.id, &ctx.clone_path)
            .await?;
    let filter = match kind_filter {
        Some("file") => Some(RepoGraphNodeKind::File),
        Some("symbol") => Some(RepoGraphNodeKind::Symbol),
        _ => None,
    };
    let hits = graph.search_by_name(query, filter, limit);
    Ok(hits
        .into_iter()
        .map(|hit| {
            let node = graph.node(hit.node_index);
            SearchHit {
                key: super::graph_neighbors::format_node_key(&node.id),
                kind: format!("{:?}", node.kind).to_lowercase(),
                display_name: node.display_name.clone(),
                score: hit.score,
                file: node.file_path.as_ref().map(|p| p.display().to_string()),
                match_kind: Some("structural".to_string()),
            }
        })
        .collect())
}

/// Per-signal hit registry — keyed by the `key` field that downstream
/// ops (`neighbors`, `impact`, `context`) take. Lexical + semantic
/// chunk hits convert to `SearchHit`s with their `match_kind` stamped;
/// structural hits arrive already in `SearchHit` form.
struct Registry {
    by_key: HashMap<String, SearchHit>,
}

impl Registry {
    fn new() -> Self {
        Self {
            by_key: HashMap::new(),
        }
    }

    fn ingest_chunks(&mut self, hits: &[CodeChunkSearchHit], match_kind: &str) -> Vec<(String, f64)> {
        let mut ranked = Vec::with_capacity(hits.len());
        for hit in hits {
            let entry = chunk_hit_to_search_hit(hit, match_kind);
            ranked.push((entry.key.clone(), hit.score));
            // First writer wins on display fields; later signals just
            // promote the existing record's `match_kind` to "hybrid".
            self.by_key
                .entry(entry.key.clone())
                .and_modify(|existing| {
                    let already = existing.match_kind.as_deref().unwrap_or("");
                    if already != match_kind && already != "hybrid" {
                        existing.match_kind = Some("hybrid".to_string());
                    }
                })
                .or_insert(entry);
        }
        ranked
    }

    fn ingest_search_hits(&mut self, hits: &[SearchHit]) -> Vec<(String, f64)> {
        let mut ranked = Vec::with_capacity(hits.len());
        for hit in hits {
            ranked.push((hit.key.clone(), hit.score));
            let key = hit.key.clone();
            let incoming_match = hit.match_kind.clone();
            self.by_key
                .entry(key)
                .and_modify(|existing| {
                    let already = existing.match_kind.as_deref().unwrap_or("");
                    let incoming = incoming_match.as_deref().unwrap_or("");
                    if !already.is_empty() && already != incoming && already != "hybrid" {
                        existing.match_kind = Some("hybrid".to_string());
                    }
                })
                .or_insert_with(|| hit.clone());
        }
        ranked
    }
}

/// PR B4 fusion routine — ingests three already-ranked signal lists
/// into a registry, fuses via `rrf_fuse`, and returns the top `limit`
/// hits with their final score swapped out for the RRF-fused score.
///
/// Pulled out of `run` so unit tests can exercise the fusion logic
/// against synthetic signals without standing up a full `AppState`.
pub(crate) fn fuse_signals(
    lexical: Vec<CodeChunkSearchHit>,
    semantic: Vec<CodeChunkSearchHit>,
    structural: Vec<SearchHit>,
    limit: usize,
) -> Vec<SearchHit> {
    let mut registry = Registry::new();
    let lex_ranked = registry.ingest_chunks(&lexical, "lexical");
    let sem_ranked = registry.ingest_chunks(&semantic, "semantic");
    let struct_ranked = registry.ingest_search_hits(&structural);

    let signals = vec![
        (lex_ranked, RRF_K),
        (sem_ranked, RRF_K),
        (struct_ranked, RRF_K),
    ];

    let confidence = HashMap::new();
    let fused = rrf_fuse(&signals, &confidence);

    fused
        .into_iter()
        .filter_map(|(key, score)| {
            let mut entry = registry.by_key.remove(&key)?;
            entry.score = score;
            Some(entry)
        })
        .take(limit)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(id: &str, file: &str, sym: Option<&str>, score: f64) -> CodeChunkSearchHit {
        CodeChunkSearchHit {
            chunk_id: id.to_string(),
            file_path: file.to_string(),
            symbol_key: sym.map(str::to_string),
            kind: "function".to_string(),
            start_line: 1,
            end_line: 10,
            score,
        }
    }

    fn struct_hit(key: &str, file: &str, score: f64) -> SearchHit {
        SearchHit {
            key: key.to_string(),
            kind: "function".to_string(),
            display_name: key.split(['#', '/']).last().unwrap_or(key).to_string(),
            score,
            file: Some(file.to_string()),
            match_kind: Some("structural".to_string()),
        }
    }

    #[test]
    fn fuse_signals_ranks_three_signal_agreement_first() {
        // sym-A appears top in all three signals → must come first.
        // sym-B appears top in only one → ranks lower despite higher
        // raw score, because RRF rewards multi-signal agreement.
        let lex = vec![
            chunk("c1", "src/auth.rs", Some("rust:auth::sym_a"), 5.0),
            chunk("c2", "src/auth.rs", Some("rust:auth::sym_c"), 4.0),
        ];
        let sem = vec![
            chunk("c1", "src/auth.rs", Some("rust:auth::sym_a"), 0.95),
            chunk("c3", "src/auth.rs", Some("rust:auth::sym_d"), 0.90),
        ];
        let structural = vec![
            struct_hit("rust:auth::sym_a", "src/auth.rs", 3.0),
            struct_hit("rust:auth::sym_b", "src/auth.rs", 99.0),
        ];

        let fused = fuse_signals(lex, sem, structural, 10);
        assert_eq!(fused[0].key, "rust:auth::sym_a");
        // sym-A appeared in lexical, semantic, and structural so its
        // match_kind should have been promoted to `"hybrid"`.
        assert_eq!(fused[0].match_kind.as_deref(), Some("hybrid"));
    }

    #[test]
    fn fuse_signals_match_kind_single_signal_keeps_label() {
        let lex = vec![chunk("c1", "src/foo.rs", Some("rust::foo"), 5.0)];
        let fused = fuse_signals(lex, vec![], vec![], 10);
        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].match_kind.as_deref(), Some("lexical"));
    }

    #[test]
    fn fuse_signals_top_3_per_file_cap_already_applied() {
        // The orchestrator caps before fusion; this test simulates
        // pre-capped inputs to confirm fuse_signals doesn't add cap
        // logic of its own — it preserves whatever the caller passed.
        let lex = vec![
            chunk("c1", "tests/big.rs", Some("rust::test_a"), 5.0),
            chunk("c2", "tests/big.rs", Some("rust::test_b"), 4.5),
            chunk("c3", "tests/big.rs", Some("rust::test_c"), 4.0),
        ];
        let fused = fuse_signals(lex, vec![], vec![], 10);
        assert_eq!(fused.len(), 3);
    }

    #[test]
    fn fuse_signals_dedupes_across_signals() {
        let lex = vec![chunk("c1", "src/foo.rs", Some("rust::foo"), 5.0)];
        let sem = vec![chunk("c1", "src/foo.rs", Some("rust::foo"), 0.9)];
        let structural = vec![struct_hit("rust::foo", "src/foo.rs", 3.0)];
        let fused = fuse_signals(lex, sem, structural, 10);
        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].match_kind.as_deref(), Some("hybrid"));
    }

    #[test]
    fn fuse_signals_respects_limit() {
        let lex = (0..50)
            .map(|i| {
                chunk(
                    &format!("c{i}"),
                    &format!("src/f{i}.rs"),
                    Some(&format!("rust::sym_{i}")),
                    50.0 - i as f64,
                )
            })
            .collect::<Vec<_>>();
        let fused = fuse_signals(lex, vec![], vec![], 5);
        assert_eq!(fused.len(), 5);
    }

    #[test]
    fn fuse_signals_empty_inputs_yields_empty() {
        let fused = fuse_signals(vec![], vec![], vec![], 10);
        assert!(fused.is_empty());
    }

    #[test]
    fn fuse_signals_preserves_only_lexical_when_other_two_empty() {
        // Sanity: the structural-only / lexical-only / semantic-only
        // path produces the input rank unchanged. Sym-A first because
        // it has rank 1 in lexical; sym-B second, sym-C third.
        let lex = vec![
            chunk("c1", "f1.rs", Some("rust::sym_a"), 9.0),
            chunk("c2", "f2.rs", Some("rust::sym_b"), 8.0),
            chunk("c3", "f3.rs", Some("rust::sym_c"), 7.0),
        ];
        let fused = fuse_signals(lex, vec![], vec![], 10);
        assert_eq!(
            fused.iter().map(|h| h.key.as_str()).collect::<Vec<_>>(),
            vec!["rust::sym_a", "rust::sym_b", "rust::sym_c"]
        );
    }

    #[test]
    fn cache_key_is_stable_for_identical_inputs() {
        let a = cache_key("proj-1", "permissions", Some("symbol"), 20);
        let b = cache_key("proj-1", "permissions", Some("symbol"), 20);
        assert_eq!(a, b);
    }

    #[test]
    fn cache_key_differs_on_kind_filter() {
        let a = cache_key("proj-1", "permissions", Some("symbol"), 20);
        let b = cache_key("proj-1", "permissions", Some("file"), 20);
        assert_ne!(a, b);
    }

    #[test]
    fn cache_key_differs_on_limit() {
        let a = cache_key("proj-1", "permissions", None, 20);
        let b = cache_key("proj-1", "permissions", None, 50);
        assert_ne!(a, b);
    }

    #[test]
    fn cache_round_trip_writes_then_reads() {
        clear_cache_for_tests();
        let key = cache_key("proj-1", "round-trip", None, 10);
        let hit = SearchHit {
            key: "rust::round_trip".to_string(),
            kind: "function".to_string(),
            display_name: "round_trip".to_string(),
            score: 0.42,
            file: Some("src/lib.rs".to_string()),
            match_kind: Some("hybrid".to_string()),
        };
        write_cache(key.clone(), vec![hit.clone()]);
        let cached = read_cached(&key).expect("cache hit expected");
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].key, hit.key);
    }

    #[test]
    fn chunk_hit_to_search_hit_falls_back_to_file_tail_when_no_symbol() {
        let h = chunk("c1", "src/foo/bar.rs", None, 1.0);
        let out = chunk_hit_to_search_hit(&h, "lexical");
        assert_eq!(out.display_name, "bar.rs");
        assert_eq!(out.key, "chunk:c1");
        assert_eq!(out.match_kind.as_deref(), Some("lexical"));
    }
}

// `format_node_key` lives in `mcp_bridge::graph_neighbors` and is
// `pub(crate)` so this sibling module can call it directly via
// `super::graph_neighbors::format_node_key` without the parent having
// to re-export it.
