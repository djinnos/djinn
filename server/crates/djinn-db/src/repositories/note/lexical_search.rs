use crate::error::{DbError as Error, DbResult as Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LexicalSearchBackend {
    SqliteFts5,
    PostgresTsvector,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LexicalSearchMode {
    Ranked,
    Dedup,
    Contradiction,
    Discovery,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LexicalSearchPlan {
    pub backend: LexicalSearchBackend,
    pub mode: LexicalSearchMode,
    pub sql: String,
    pub query: String,
    pub score_alias: &'static str,
    pub score_descending: bool,
    pub replacement_notes: Vec<&'static str>,
}

impl LexicalSearchPlan {
    /// Whether the caller must bind `query` as the first positional parameter
    /// when executing the SQL returned by [`executable_lexical_search_sql`].
    ///
    /// Both backends bind the (already-sanitized) query string as the first
    /// positional parameter: SQLite at `?1` inside `MATCH`, Postgres at `$1`
    /// inside `to_tsquery('english', $1)`.
    pub fn needs_query_bind(&self) -> bool {
        matches!(
            self.backend,
            LexicalSearchBackend::SqliteFts5 | LexicalSearchBackend::PostgresTsvector
        )
    }
}

pub fn executable_lexical_search_sql(plan: &LexicalSearchPlan) -> String {
    // Both backends now carry executable, parameter-bound SQL directly in
    // `plan.sql`: SQLite FTS5 with `?N` MATCH placeholders, Postgres with
    // `$N` `to_tsquery` placeholders. The query string is bound as the first
    // positional parameter in both cases (see `needs_query_bind`), so no
    // string rewriting is required.
    plan.sql.clone()
}

pub fn normalize_lexical_score(plan: &LexicalSearchPlan, raw_score: f64) -> f64 {
    if plan.score_descending {
        raw_score
    } else {
        -raw_score
    }
}

pub fn lexical_search_threshold(
    backend: LexicalSearchBackend,
    mode: LexicalSearchMode,
) -> Result<Option<f64>> {
    let threshold = match (backend, mode) {
        (LexicalSearchBackend::SqliteFts5, LexicalSearchMode::Dedup) => Some(-3.0),
        (LexicalSearchBackend::SqliteFts5, LexicalSearchMode::Contradiction) => Some(5.0),
        (LexicalSearchBackend::PostgresTsvector, LexicalSearchMode::Dedup) => Some(0.0),
        (LexicalSearchBackend::PostgresTsvector, LexicalSearchMode::Contradiction) => Some(0.0),
        _ => None,
    };

    if let Some(threshold) = threshold
        && backend == LexicalSearchBackend::PostgresTsvector
    {
        validate_postgres_tsvector_threshold(threshold)?;
    }

    Ok(threshold)
}

pub fn sanitize_sqlite_fts5_query(raw: &str) -> Option<String> {
    let tokens: Vec<&str> = raw
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| {
            let t = t.to_uppercase();
            !t.is_empty() && t != "AND" && t != "OR" && t != "NOT" && t != "NEAR"
        })
        .collect();
    if tokens.is_empty() {
        return None;
    }
    Some(
        tokens
            .into_iter()
            .map(|t| format!("\"{t}\""))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

pub fn sanitize_postgres_tsquery(raw: &str) -> Option<String> {
    let tokens = raw
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .map(|term| term.trim_matches('_'))
        .filter(|term| {
            let upper = term.to_uppercase();
            !term.is_empty() && upper != "AND" && upper != "OR" && upper != "NOT"
        })
        .take(12)
        .collect::<Vec<_>>();

    if tokens.is_empty() {
        return None;
    }

    // Build a valid Postgres `to_tsquery` expression. Terms are AND-joined
    // (`&`) and longer terms get the `:*` prefix-match operator so partial
    // tokens still hit. The sanitizer above strips everything except
    // [A-Za-z0-9_], so the lexemes are safe to interpolate into the tsquery
    // string — but we still pass the whole expression as a *bound parameter*
    // to `to_tsquery`, never as raw SQL.
    Some(
        tokens
            .into_iter()
            .map(|term| {
                if term.len() >= 3 {
                    format!("{term}:*")
                } else {
                    term.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join(" & "),
    )
}

pub fn build_lexical_search_plan(
    backend: LexicalSearchBackend,
    mode: LexicalSearchMode,
    raw_query: &str,
) -> Result<Option<LexicalSearchPlan>> {
    let query = match backend {
        LexicalSearchBackend::SqliteFts5 => sanitize_sqlite_fts5_query(raw_query),
        LexicalSearchBackend::PostgresTsvector => sanitize_postgres_tsquery(raw_query),
    };

    let Some(query) = query else {
        return Ok(None);
    };

    Ok(Some(match (backend, mode) {
        (LexicalSearchBackend::SqliteFts5, LexicalSearchMode::Ranked) => LexicalSearchPlan {
            backend,
            mode,
            sql: "SELECT n.id, bm25(notes_fts, 3.0, 1.0, 2.0) as bm25_score\nFROM notes_fts\nJOIN notes n ON notes_fts.rowid = n.rowid\nWHERE notes_fts MATCH $11\n  AND n.project_id = $22\n  AND n.status = 'active'\n  AND ($33 = '' OR n.folder = $43)\n  AND ($54 = '' OR n.note_type = $64)\nORDER BY bm25(notes_fts, 3.0, 1.0, 2.0)\nLIMIT $75".to_owned(),
            query,
            score_alias: "bm25_score",
            score_descending: false,
            replacement_notes: vec![
                "Uses FTS5 virtual table and bm25() column weighting.",
                "MySQL replacement should order by MATCH() score DESC instead of bm25 ASC.",
            ],
        },
        (LexicalSearchBackend::SqliteFts5, LexicalSearchMode::Dedup) => LexicalSearchPlan {
            backend,
            mode,
            sql: "SELECT n.id, n.permalink, n.title, n.folder, n.note_type, n.content, n.abstract, n.overview,\n       -bm25(notes_fts, 3.0, 1.0, 2.0) as score\nFROM notes_fts\nJOIN notes n ON notes_fts.rowid = n.rowid\nWHERE notes_fts MATCH $11\n  AND n.project_id = $22\n  AND n.status = 'active'\n  AND n.folder = $33\n  AND n.note_type = $44\n  AND -bm25(notes_fts, 3.0, 1.0, 2.0) > $55\nORDER BY bm25(notes_fts, 3.0, 1.0, 2.0)\nLIMIT $66".to_owned(),
            query,
            score_alias: "score",
            score_descending: true,
            replacement_notes: vec![
                "Current threshold is tuned against negated bm25 values.",
                "MySQL cutover will need a new empirical threshold because MATCH() scores are positive and backend-specific.",
            ],
        },
        (LexicalSearchBackend::SqliteFts5, LexicalSearchMode::Contradiction) => LexicalSearchPlan {
            backend,
            mode,
            sql: "SELECT n.id, n.permalink, n.title, n.folder, n.note_type,\n       -bm25(notes_fts, 3.0, 1.0, 2.0) as score\nFROM notes_fts\nJOIN notes n ON notes_fts.rowid = n.rowid\nWHERE notes_fts MATCH $11\n  AND n.id != $22\n  AND n.status = 'active'\n  AND -bm25(notes_fts, 3.0, 1.0, 2.0) > 5.0\nORDER BY bm25(notes_fts, 3.0, 1.0, 2.0)\nLIMIT 3".to_owned(),
            query,
            score_alias: "score",
            score_descending: true,
            replacement_notes: vec![
                "Current contradiction filter assumes a fixed FTS5 score threshold of 5.0.",
                "MySQL cutover should preserve result count and downstream TypeRisk logic while recalibrating thresholds.",
            ],
        },
        (LexicalSearchBackend::SqliteFts5, LexicalSearchMode::Discovery) => LexicalSearchPlan {
            backend,
            mode,
            sql: "SELECT n.id, bm25(notes_fts, 3.0, 1.0, 2.0) as bm25_score\nFROM notes_fts\nJOIN notes n ON notes_fts.rowid = n.rowid\nWHERE notes_fts MATCH $11\n  AND n.project_id = $22\n  AND n.status = 'active'\nORDER BY bm25(notes_fts, 3.0, 1.0, 2.0)\nLIMIT $33".to_owned(),
            query,
            score_alias: "bm25_score",
            score_descending: false,
            replacement_notes: vec![
                "Discovery is candidate generation only; RRF handles final ranking.",
                "MySQL FULLTEXT only needs stable lexical candidates, not bm25 parity.",
            ],
        },
        (LexicalSearchBackend::PostgresTsvector, LexicalSearchMode::Ranked) => LexicalSearchPlan {
            backend,
            mode,
            // Bind order (see `ranked_lexical_scores`): $1 query, $2 project_id,
            // $3/$4 folder (guard + equality), $5/$6 note_type, $7 limit.
            sql: "SELECT n.id, ts_rank(n.search_vector, to_tsquery('english', $1))::float8 AS fulltext_score\nFROM notes n\nWHERE n.project_id = $2\n  AND n.status = 'active'\n  AND ($3 = '' OR n.folder = $4)\n  AND ($5 = '' OR n.note_type = $6)\n  AND n.search_vector @@ to_tsquery('english', $1)\nORDER BY fulltext_score DESC, n.id ASC\nLIMIT $7".to_owned(),
            query,
            score_alias: "fulltext_score",
            score_descending: true,
            replacement_notes: vec![
                "Uses the generated `notes.search_vector` tsvector + GIN index.",
                "Ranks with ts_rank() (higher = better) instead of bm25.",
            ],
        },
        (LexicalSearchBackend::PostgresTsvector, LexicalSearchMode::Dedup) => LexicalSearchPlan {
            backend,
            mode,
            // Bind order (see `dedup_lexical_candidates`): $1 query, $2 project_id,
            // $3 folder, $4 note_type, $5 threshold, $6 limit.
            sql: "SELECT n.id, n.permalink, n.title, n.folder, n.note_type, n.content, n.abstract, n.overview,\n       ts_rank(n.search_vector, to_tsquery('english', $1))::float8 AS score\nFROM notes n\nWHERE n.project_id = $2\n  AND n.status = 'active'\n  AND n.folder = $3\n  AND n.note_type = $4\n  AND n.search_vector @@ to_tsquery('english', $1)\n  AND ts_rank(n.search_vector, to_tsquery('english', $1))::float8 > $5\nORDER BY score DESC, n.id ASC\nLIMIT $6".to_owned(),
            query,
            score_alias: "score",
            score_descending: true,
            replacement_notes: vec![
                "Dedup ranks with ts_rank() (positive scores; threshold is non-negative).",
            ],
        },
        (LexicalSearchBackend::PostgresTsvector, LexicalSearchMode::Contradiction) => LexicalSearchPlan {
            backend,
            mode,
            // Bind order (see `contradiction_lexical_candidates`): $1 query,
            // $2 note_id, $3 threshold. Limit is fixed at 3.
            sql: "SELECT n.id, n.permalink, n.title, n.folder, n.note_type,\n       ts_rank(n.search_vector, to_tsquery('english', $1))::float8 AS score\nFROM notes n\nWHERE n.id != $2\n  AND n.status = 'active'\n  AND n.search_vector @@ to_tsquery('english', $1)\n  AND ts_rank(n.search_vector, to_tsquery('english', $1))::float8 > $3\nORDER BY score DESC, n.id ASC\nLIMIT 3".to_owned(),
            query,
            score_alias: "score",
            score_descending: true,
            replacement_notes: vec![
                "Contradiction search keeps the top-3 contract for downstream TypeRisk filtering.",
            ],
        },
        (LexicalSearchBackend::PostgresTsvector, LexicalSearchMode::Discovery) => LexicalSearchPlan {
            backend,
            mode,
            // Bind order (see `fts_candidates`): $1 query, $2 project_id, $3 limit.
            sql: "SELECT n.id, ts_rank(n.search_vector, to_tsquery('english', $1))::float8 AS fulltext_score\nFROM notes n\nWHERE n.project_id = $2\n  AND n.status = 'active'\n  AND n.search_vector @@ to_tsquery('english', $1)\nORDER BY fulltext_score DESC, n.id ASC\nLIMIT $3".to_owned(),
            query,
            score_alias: "fulltext_score",
            score_descending: true,
            replacement_notes: vec![
                "Discovery is lexical candidate generation for the RRF pipeline.",
            ],
        },
    }))
}

pub fn validate_postgres_tsvector_threshold(threshold: f64) -> Result<()> {
    if threshold.is_sign_negative() {
        return Err(Error::InvalidData(
            "Postgres tsvector thresholds must be non-negative ts_rank() scores".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_query_sanitizer_quotes_terms() {
        assert_eq!(
            sanitize_sqlite_fts5_query("rust OR sqlite + mysql"),
            Some("\"rust\" \"sqlite\" \"mysql\"".to_owned())
        );
    }

    #[test]
    fn postgres_query_sanitizer_builds_tsquery() {
        assert_eq!(
            sanitize_postgres_tsquery("rust sqlite _ bm25"),
            Some("rust:* & sqlite:* & bm25:*".to_owned())
        );
    }

    #[test]
    fn postgres_plan_uses_tsvector_and_to_tsquery() {
        let plan = build_lexical_search_plan(
            LexicalSearchBackend::PostgresTsvector,
            LexicalSearchMode::Ranked,
            "rust sqlite",
        )
        .unwrap()
        .unwrap();

        assert!(
            plan.sql
                .contains("n.search_vector @@ to_tsquery('english', $1)")
        );
        assert!(plan.sql.contains("ts_rank("));
        assert!(!plan.sql.contains("MATCH("));
        assert!(!plan.sql.contains("AGAINST"));
        assert!(!plan.sql.contains("notes_fts"));
        assert_eq!(plan.query, "rust:* & sqlite:*");
        assert!(plan.score_descending);
    }

    #[test]
    fn sqlite_plan_documents_bm25_assumption() {
        let plan = build_lexical_search_plan(
            LexicalSearchBackend::SqliteFts5,
            LexicalSearchMode::Dedup,
            "shared token",
        )
        .unwrap()
        .unwrap();

        assert!(plan.sql.contains("bm25(notes_fts, 3.0, 1.0, 2.0)"));
        assert!(
            plan.replacement_notes
                .iter()
                .any(|note| note.contains("threshold"))
        );
    }

    #[test]
    fn postgres_thresholds_must_be_non_negative() {
        assert!(validate_postgres_tsvector_threshold(0.0).is_ok());
        assert!(validate_postgres_tsvector_threshold(-0.1).is_err());
    }

    #[test]
    fn postgres_execution_sql_binds_query_parameter() {
        let plan = build_lexical_search_plan(
            LexicalSearchBackend::PostgresTsvector,
            LexicalSearchMode::Ranked,
            "rust sqlite",
        )
        .unwrap()
        .unwrap();

        let sql = executable_lexical_search_sql(&plan);
        // The query is bound as $1; no inlined literal, no MySQL syntax.
        assert!(sql.contains("to_tsquery('english', $1)"));
        assert!(!sql.contains("AGAINST"));
        assert!(!sql.contains("MATCH("));
        assert!(plan.needs_query_bind());
    }

    #[test]
    fn score_normalization_preserves_best_first_across_backends() {
        let sqlite_plan = build_lexical_search_plan(
            LexicalSearchBackend::SqliteFts5,
            LexicalSearchMode::Ranked,
            "rust sqlite",
        )
        .unwrap()
        .unwrap();
        let postgres_plan = build_lexical_search_plan(
            LexicalSearchBackend::PostgresTsvector,
            LexicalSearchMode::Ranked,
            "rust sqlite",
        )
        .unwrap()
        .unwrap();

        assert_eq!(normalize_lexical_score(&sqlite_plan, -2.5), 2.5);
        assert_eq!(normalize_lexical_score(&postgres_plan, 2.5), 2.5);
    }

    #[test]
    fn thresholds_follow_backend_score_conventions() {
        assert_eq!(
            lexical_search_threshold(LexicalSearchBackend::SqliteFts5, LexicalSearchMode::Dedup)
                .unwrap(),
            Some(-3.0)
        );
        assert_eq!(
            lexical_search_threshold(
                LexicalSearchBackend::PostgresTsvector,
                LexicalSearchMode::Contradiction,
            )
            .unwrap(),
            Some(0.0)
        );
    }
}
