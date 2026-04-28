//! Lexical + semantic query helpers for the hybrid `code_graph search`
//! mode (PR B4 of the code-graph + RAG overhaul).
//!
//! The heavy lifting (RRF fusion, top-3-per-file capping, structural
//! signal blending, in-memory cache) lives in the server bridge layer —
//! this module is just the two SQL/Qdrant helpers the bridge calls into.
//!
//! ## Lexical strategy
//!
//! Dolt's MySQL wire today rejects `MATCH(...) AGAINST (?)` for
//! `embedded_text` (the column is `TEXT` and FULLTEXT-indexed for
//! `notes` only — not yet for `code_chunks`). The plan explicitly
//! permits "or fall back to Tantivy/in-memory if Dolt FTS limits hit."
//! We pick the simplest fallback: case-insensitive `LIKE %query%` over
//! `embedded_text` and `(symbol_key, file_path)`, with a hand-rolled
//! score that prefers exact-token hits over substring noise. This
//! mirrors the spirit of FTS's "did the term appear?" candidate
//! generation step — RRF then fuses lexical with semantic + structural,
//! so we don't need bm25-quality ranks here, just a stable candidate
//! list that the fuser can interleave.
//!
//! When (later) Dolt's FULLTEXT support stabilises for the
//! `code_chunks` table, this module is the single place to swap LIKE
//! for `MATCH ... AGAINST` without touching the bridge or the chunker.

use crate::database::Database;
use crate::error::DbResult as Result;

/// One hit returned by either [`lexical_search_chunks`] or
/// [`semantic_search_chunks`]. The bridge folds these into its own
/// `SearchHit` shape after RRF fusion + top-3-per-file capping.
#[derive(Clone, Debug, PartialEq)]
pub struct CodeChunkSearchHit {
    pub chunk_id: String,
    pub file_path: String,
    pub symbol_key: Option<String>,
    pub kind: String,
    pub start_line: u32,
    pub end_line: u32,
    /// Per-signal raw score. Larger = better. The fuser converts this
    /// to ranks via `rrf_fuse`, so the absolute scale doesn't matter —
    /// only the order within one signal does.
    pub score: f64,
}

/// Sanitize a free-text query into a safe `LIKE` pattern + a list of
/// individual word tokens for boosting. Returns `None` when the query
/// has no usable tokens (empty, whitespace only, or pure punctuation).
fn sanitize_like_query(raw: &str) -> Option<(String, Vec<String>)> {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            // Strip SQL `LIKE` metacharacters defensively even though
            // we sqlx-bind below — keeps the rendered pattern readable
            // and prevents accidental matches on `%` / `_` typed by
            // the caller.
            if c == '%' || c == '_' || c == '\\' {
                ' '
            } else {
                c
            }
        })
        .collect();

    let tokens: Vec<String> = cleaned
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| !t.is_empty())
        .take(12)
        .map(|t| t.to_ascii_lowercase())
        .collect();

    if tokens.is_empty() {
        return None;
    }

    let pattern = format!("%{}%", cleaned.trim().to_ascii_lowercase());
    Some((pattern, tokens))
}

/// Lexical signal — `LIKE %query%` over `embedded_text` and the
/// human-readable bits of the chunk row. Returns up to `limit` hits
/// scoped to `project_id`.
///
/// Score breakdown (purely a within-signal ordering signal — RRF
/// converts to ranks downstream, so absolute scale is irrelevant):
/// * +3.0 if the symbol_key contains every token
/// * +2.0 if the file_path contains every token
/// * +1.0 if the embedded_text contains the full query phrase
/// * +0.1 per token that appears in embedded_text
pub async fn lexical_search_chunks(
    db: &Database,
    project_id: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<CodeChunkSearchHit>> {
    db.ensure_initialized().await?;
    let Some((pattern, tokens)) = sanitize_like_query(query) else {
        return Ok(vec![]);
    };

    // Over-fetch a bit so the post-filter score-and-rerank still has
    // headroom after we drop weak hits. Cap at 200 to keep the SQL
    // result set bounded on huge projects.
    let fetch_limit = (limit.saturating_mul(4)).clamp(limit, 200) as i64;

    // NOTE: dynamic SQL not used — sqlx::query! with positional binds.
    // We match against three columns (symbol_key, file_path,
    // embedded_text) with a single OR so Dolt only scans `code_chunks`
    // once. The CASE expression in the SELECT is the boost score — it
    // runs after row qualification so the optimizer doesn't pay for
    // it on rejected rows.
    let rows = sqlx::query!(
        r#"SELECT id, file_path, symbol_key, kind, start_line, end_line, embedded_text
             FROM code_chunks
            WHERE project_id = ?
              AND (LOWER(symbol_key) LIKE ?
                   OR LOWER(file_path) LIKE ?
                   OR LOWER(embedded_text) LIKE ?)
            LIMIT ?"#,
        project_id,
        pattern,
        pattern,
        pattern,
        fetch_limit,
    )
    .fetch_all(db.pool())
    .await?;

    let mut hits: Vec<CodeChunkSearchHit> = rows
        .into_iter()
        .map(|r| {
            let symbol_lower = r
                .symbol_key
                .as_deref()
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_default();
            let file_lower = r.file_path.to_ascii_lowercase();
            let body_lower = r.embedded_text.to_ascii_lowercase();

            let trimmed_pattern = pattern.trim_matches('%');
            let phrase_hit = !trimmed_pattern.is_empty()
                && body_lower.contains(trimmed_pattern);

            let symbol_full = !tokens.is_empty()
                && tokens.iter().all(|t| symbol_lower.contains(t));
            let file_full = !tokens.is_empty()
                && tokens.iter().all(|t| file_lower.contains(t));

            let mut score = 0.0;
            if symbol_full {
                score += 3.0;
            }
            if file_full {
                score += 2.0;
            }
            if phrase_hit {
                score += 1.0;
            }
            for token in &tokens {
                if body_lower.contains(token) {
                    score += 0.1;
                }
            }

            CodeChunkSearchHit {
                chunk_id: r.id,
                file_path: r.file_path,
                symbol_key: r.symbol_key,
                kind: r.kind,
                start_line: r.start_line as u32,
                end_line: r.end_line as u32,
                score,
            }
        })
        .filter(|h| h.score > 0.0)
        .collect();

    hits.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.chunk_id.cmp(&b.chunk_id))
    });
    hits.truncate(limit);
    Ok(hits)
}

/// Hydrate a list of Qdrant payload + score results into
/// [`CodeChunkSearchHit`]s by joining against `code_chunks`. Used by the
/// bridge after a `qdrant_client::search_points` call.
///
/// Pulled out so the bridge can stay free of `sqlx::query!` macros.
pub async fn hydrate_chunk_ids(
    db: &Database,
    project_id: &str,
    chunk_id_scores: &[(String, f64)],
) -> Result<Vec<CodeChunkSearchHit>> {
    if chunk_id_scores.is_empty() {
        return Ok(vec![]);
    }
    db.ensure_initialized().await?;

    let placeholders = std::iter::repeat_n("?", chunk_id_scores.len())
        .collect::<Vec<_>>()
        .join(", ");
    // NOTE: dynamic SQL (IN-list of chunk ids built at runtime) — the
    // placeholders are filled exclusively from the caller-supplied
    // string list, every value is rebound rather than interpolated.
    let sql = format!(
        "SELECT id, file_path, symbol_key, kind, start_line, end_line
           FROM code_chunks
          WHERE project_id = ?
            AND id IN ({})",
        placeholders
    );

    let mut q = sqlx::query_as::<
        sqlx::MySql,
        (String, String, Option<String>, String, i32, i32),
    >(&sql)
    .bind(project_id);
    for (chunk_id, _) in chunk_id_scores {
        q = q.bind(chunk_id);
    }
    let rows = q.fetch_all(db.pool()).await?;
    let by_id: std::collections::HashMap<String, (String, Option<String>, String, i32, i32)> = rows
        .into_iter()
        .map(|(id, file_path, symbol_key, kind, start_line, end_line)| {
            (id, (file_path, symbol_key, kind, start_line, end_line))
        })
        .collect();

    let hits: Vec<CodeChunkSearchHit> = chunk_id_scores
        .iter()
        .filter_map(|(chunk_id, score)| {
            let (file_path, symbol_key, kind, start_line, end_line) =
                by_id.get(chunk_id)?.clone();
            Some(CodeChunkSearchHit {
                chunk_id: chunk_id.clone(),
                file_path,
                symbol_key,
                kind,
                start_line: start_line as u32,
                end_line: end_line as u32,
                score: *score,
            })
        })
        .collect();
    Ok(hits)
}

/// Cap the number of hits per file to `cap`. Preserves the original
/// hit ordering (assumed already sorted best-first by the caller).
///
/// Plan §"PR B4": "Top-3 chunks per file cap before fusion to defang
/// test files." A single test module can otherwise dominate the
/// signal because every assertion is its own chunk.
pub fn cap_per_file(hits: Vec<CodeChunkSearchHit>, cap: usize) -> Vec<CodeChunkSearchHit> {
    if cap == 0 {
        return Vec::new();
    }
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut out = Vec::with_capacity(hits.len());
    for hit in hits {
        let entry = counts.entry(hit.file_path.clone()).or_insert(0);
        if *entry < cap {
            *entry += 1;
            out.push(hit);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_like_query_strips_metachars() {
        let (pattern, tokens) =
            sanitize_like_query("permissions check %_\\").expect("non-empty");
        assert!(!pattern.contains('%') || pattern.starts_with('%') && pattern.ends_with('%'));
        // The %s on the outside are our wildcard; only those two should remain.
        assert_eq!(pattern.matches('%').count(), 2);
        assert_eq!(tokens, vec!["permissions", "check"]);
    }

    #[test]
    fn sanitize_like_query_empty_is_none() {
        assert!(sanitize_like_query("").is_none());
        assert!(sanitize_like_query("   \t\n  ").is_none());
        assert!(sanitize_like_query("%%%%").is_none());
    }

    #[test]
    fn cap_per_file_keeps_first_n_per_path() {
        let make = |id: &str, file: &str, score: f64| CodeChunkSearchHit {
            chunk_id: id.to_string(),
            file_path: file.to_string(),
            symbol_key: None,
            kind: "function".to_string(),
            start_line: 1,
            end_line: 2,
            score,
        };
        let hits = vec![
            make("a", "tests/big_test.rs", 9.0),
            make("b", "tests/big_test.rs", 8.0),
            make("c", "tests/big_test.rs", 7.0),
            make("d", "tests/big_test.rs", 6.0),
            make("e", "tests/big_test.rs", 5.0),
            make("f", "src/main.rs", 4.0),
        ];
        let capped = cap_per_file(hits, 3);
        assert_eq!(capped.len(), 4);
        assert_eq!(
            capped.iter().map(|h| h.chunk_id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "c", "f"]
        );
    }

    #[test]
    fn cap_per_file_zero_returns_empty() {
        let hits = vec![CodeChunkSearchHit {
            chunk_id: "x".to_string(),
            file_path: "src/lib.rs".to_string(),
            symbol_key: None,
            kind: "function".to_string(),
            start_line: 1,
            end_line: 2,
            score: 1.0,
        }];
        assert!(cap_per_file(hits, 0).is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lexical_search_returns_empty_on_blank_query() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let hits = lexical_search_chunks(&db, "proj-1", "   ", 10)
            .await
            .unwrap();
        assert!(hits.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lexical_search_finds_substring_in_embedded_text() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();

        // Insert a chunk whose `embedded_text` mentions "permissions" and
        // another that doesn't, plus a third whose symbol_key matches.
        for (id, file, sym, body) in [
            ("c1", "src/auth.rs", Some("rust:auth::check_permissions"),
             "Label: check_permissions\nFile: src/auth.rs\nKind: function\n…"),
            ("c2", "src/unrelated.rs", Some("rust::other::helper"),
             "Label: helper\nFile: src/unrelated.rs\nKind: function\n…"),
            ("c3", "src/auth.rs", None,
             "Label: file body\nFile: src/auth.rs\nNote: discusses permissions logic"),
        ] {
            sqlx::query!(
                r#"INSERT INTO code_chunks
                    (id, project_id, file_path, symbol_key, kind,
                     start_line, end_line, content_hash, embedded_text)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
                id,
                "proj-1",
                file,
                sym,
                "function",
                1_i32,
                10_i32,
                "deadbeef",
                body,
            )
            .execute(db.pool())
            .await
            .unwrap();
        }

        let hits = lexical_search_chunks(&db, "proj-1", "permissions", 10)
            .await
            .unwrap();
        let ids: Vec<&str> = hits.iter().map(|h| h.chunk_id.as_str()).collect();
        assert!(ids.contains(&"c1"), "symbol_key match should hit");
        assert!(ids.contains(&"c3"), "body substring match should hit");
        assert!(!ids.contains(&"c2"), "unrelated chunk must not hit");
        // c1 (symbol_key match, +3.0) must rank ahead of c3 (body only).
        assert_eq!(hits[0].chunk_id, "c1");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lexical_search_scoped_to_project() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();

        for (id, project) in [("c1", "proj-1"), ("c2", "proj-2")] {
            sqlx::query!(
                r#"INSERT INTO code_chunks
                    (id, project_id, file_path, symbol_key, kind,
                     start_line, end_line, content_hash, embedded_text)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
                id,
                project,
                "src/foo.rs",
                Some::<&str>("rust::foo::widget"),
                "function",
                1_i32,
                5_i32,
                "deadbeef",
                "Label: widget\nKind: function\n…",
            )
            .execute(db.pool())
            .await
            .unwrap();
        }

        let hits = lexical_search_chunks(&db, "proj-1", "widget", 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].chunk_id, "c1");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hydrate_chunk_ids_preserves_caller_order() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();

        for id in ["c1", "c2", "c3"] {
            sqlx::query!(
                r#"INSERT INTO code_chunks
                    (id, project_id, file_path, symbol_key, kind,
                     start_line, end_line, content_hash, embedded_text)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
                id,
                "proj-1",
                "src/foo.rs",
                None::<&str>,
                "function",
                1_i32,
                2_i32,
                "deadbeef",
                "body",
            )
            .execute(db.pool())
            .await
            .unwrap();
        }

        let scores = vec![
            ("c3".to_string(), 0.9),
            ("c1".to_string(), 0.7),
            ("missing".to_string(), 0.5),
        ];
        let hits = hydrate_chunk_ids(&db, "proj-1", &scores).await.unwrap();
        assert_eq!(
            hits.iter().map(|h| h.chunk_id.as_str()).collect::<Vec<_>>(),
            vec!["c3", "c1"]
        );
    }
}
