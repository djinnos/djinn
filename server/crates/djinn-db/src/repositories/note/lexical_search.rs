use crate::error::{DbError as Error, DbResult as Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LexicalSearchBackend {
    SqliteFts5,
    MysqlFulltext,
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
    /// For `MysqlFulltext` the query is inlined as a SQL literal (see the
    /// Dolt workaround comment in `executable_lexical_search_sql`), so callers
    /// must skip the first bind.
    pub fn needs_query_bind(&self) -> bool {
        matches!(self.backend, LexicalSearchBackend::SqliteFts5)
    }
}

pub fn executable_lexical_search_sql(plan: &LexicalSearchPlan) -> String {
    match plan.backend {
        LexicalSearchBackend::SqliteFts5 => plan.sql.clone(),
        LexicalSearchBackend::MysqlFulltext => {
            // Dolt's MySQL wire protocol currently fails to bind a prepared
            // parameter inside MATCH(...) AGAINST (?), returning
            // "MySQLToType failed: unsupported type" for the column metadata.
            // As a workaround we inline the (already sanitized) query string
            // as a SQL literal at the `?1` positions, and emit `?` for all
            // other positional placeholders (`?2`, `?3`, ...). Callers bind
            // the remaining parameters in order but skip the query literal.
            //
            // Safety: `plan.query` is produced by `sanitize_mysql_boolean_query`,
            // which only emits characters from the set [A-Za-z0-9_ +*] — none
            // of which require SQL-level escaping inside a double-quoted
            // string literal. We still escape `"` and `\` defensively.
            let literal = mysql_string_literal(&plan.query);

            let mut out = String::with_capacity(plan.sql.len());
            let mut chars = plan.sql.chars().peekable();

            while let Some(ch) = chars.next() {
                if ch == '?' {
                    let mut digits = String::new();
                    while matches!(chars.peek(), Some(next) if next.is_ascii_digit()) {
                        digits.push(chars.next().unwrap());
                    }
                    if digits == "1" {
                        out.push_str(&literal);
                    } else {
                        out.push('?');
                    }
                } else {
                    out.push(ch);
                }
            }

            out
        }
    }
}

fn mysql_string_literal(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
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
        (LexicalSearchBackend::MysqlFulltext, LexicalSearchMode::Dedup) => Some(0.0),
        (LexicalSearchBackend::MysqlFulltext, LexicalSearchMode::Contradiction) => Some(0.0),
        _ => None,
    };

    if let Some(threshold) = threshold
        && backend == LexicalSearchBackend::MysqlFulltext
    {
        validate_mysql_fulltext_threshold(threshold)?;
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

pub fn sanitize_mysql_boolean_query(raw: &str) -> Option<String> {
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

    Some(
        tokens
            .into_iter()
            .map(|term| {
                if term.len() >= 3 {
                    format!("+{term}*")
                } else {
                    format!("+{term}")
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
    )
}

pub fn build_lexical_search_plan(
    backend: LexicalSearchBackend,
    mode: LexicalSearchMode,
    raw_query: &str,
) -> Result<Option<LexicalSearchPlan>> {
    let query = match backend {
        LexicalSearchBackend::SqliteFts5 => sanitize_sqlite_fts5_query(raw_query),
        LexicalSearchBackend::MysqlFulltext => sanitize_mysql_boolean_query(raw_query),
    };

    let Some(query) = query else {
        return Ok(None);
    };

    Ok(Some(match (backend, mode) {
        (LexicalSearchBackend::SqliteFts5, LexicalSearchMode::Ranked) => LexicalSearchPlan {
            backend,
            mode,
            sql: "SELECT n.id, bm25(notes_fts, 3.0, 1.0, 2.0) as bm25_score\nFROM notes_fts\nJOIN notes n ON notes_fts.rowid = n.rowid\nWHERE notes_fts MATCH ?1\n  AND n.project_id = ?2\n  AND (?3 = '' OR n.folder = ?3)\n  AND (?4 = '' OR n.note_type = ?4)\nORDER BY bm25(notes_fts, 3.0, 1.0, 2.0)\nLIMIT ?5".to_owned(),
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
            sql: "SELECT n.id, n.permalink, n.title, n.folder, n.note_type, n.abstract, n.overview,\n       -bm25(notes_fts, 3.0, 1.0, 2.0) as score\nFROM notes_fts\nJOIN notes n ON notes_fts.rowid = n.rowid\nWHERE notes_fts MATCH ?1\n  AND n.project_id = ?2\n  AND n.folder = ?3\n  AND n.note_type = ?4\n  AND -bm25(notes_fts, 3.0, 1.0, 2.0) > ?5\nORDER BY bm25(notes_fts, 3.0, 1.0, 2.0)\nLIMIT ?6".to_owned(),
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
            sql: "SELECT n.id, n.permalink, n.title, n.folder, n.note_type,\n       -bm25(notes_fts, 3.0, 1.0, 2.0) as score\nFROM notes_fts\nJOIN notes n ON notes_fts.rowid = n.rowid\nWHERE notes_fts MATCH ?1\n  AND n.id != ?2\n  AND -bm25(notes_fts, 3.0, 1.0, 2.0) > 5.0\nORDER BY bm25(notes_fts, 3.0, 1.0, 2.0)\nLIMIT 3".to_owned(),
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
            sql: "SELECT n.id, bm25(notes_fts, 3.0, 1.0, 2.0) as bm25_score\nFROM notes_fts\nJOIN notes n ON notes_fts.rowid = n.rowid\nWHERE notes_fts MATCH ?1\n  AND n.project_id = ?2\nORDER BY bm25(notes_fts, 3.0, 1.0, 2.0)\nLIMIT ?3".to_owned(),
            query,
            score_alias: "bm25_score",
            score_descending: false,
            replacement_notes: vec![
                "Discovery is candidate generation only; RRF handles final ranking.",
                "MySQL FULLTEXT only needs stable lexical candidates, not bm25 parity.",
            ],
        },
        (LexicalSearchBackend::MysqlFulltext, LexicalSearchMode::Ranked) => LexicalSearchPlan {
            backend,
            mode,
            sql: "SELECT n.id, CAST(MATCH(n.title, n.content, n.tags) AGAINST (?1) AS DOUBLE) AS fulltext_score\nFROM notes n\nWHERE n.project_id = ?2\n  AND (?3 = '' OR n.folder = ?3)\n  AND (?4 = '' OR n.note_type = ?4)\n  AND MATCH(n.title, n.content, n.tags) AGAINST (?1)\nORDER BY fulltext_score DESC, n.id ASC\nLIMIT ?5".to_owned(),
            query,
            score_alias: "fulltext_score",
            score_descending: true,
            replacement_notes: vec![
                "Runs directly against notes instead of a shadow notes_fts table.",
                "Assumes FULLTEXT INDEX notes_ft (title, content, tags) exists.",
                "Uses natural language mode (Dolt does not yet support IN BOOLEAN MODE).",
            ],
        },
        (LexicalSearchBackend::MysqlFulltext, LexicalSearchMode::Dedup) => LexicalSearchPlan {
            backend,
            mode,
            sql: "SELECT n.id, n.permalink, n.title, n.folder, n.note_type, n.abstract, n.overview,\n       CAST(MATCH(n.title, n.content, n.tags) AGAINST (?1) AS DOUBLE) AS score\nFROM notes n\nWHERE n.project_id = ?2\n  AND n.folder = ?3\n  AND n.note_type = ?4\n  AND MATCH(n.title, n.content, n.tags) AGAINST (?1) > ?5\nORDER BY score DESC, n.id ASC\nLIMIT ?6".to_owned(),
            query,
            score_alias: "score",
            score_descending: true,
            replacement_notes: vec![
                "Dedup no longer depends on bm25 sign inversion.",
                "Threshold placeholder remains required but must be recalibrated for MATCH() scores.",
                "Uses natural language mode (Dolt does not yet support IN BOOLEAN MODE).",
            ],
        },
        (LexicalSearchBackend::MysqlFulltext, LexicalSearchMode::Contradiction) => LexicalSearchPlan {
            backend,
            mode,
            sql: "SELECT n.id, n.permalink, n.title, n.folder, n.note_type,\n       CAST(MATCH(n.title, n.content, n.tags) AGAINST (?1) AS DOUBLE) AS score\nFROM notes n\nWHERE n.id != ?2\n  AND MATCH(n.title, n.content, n.tags) AGAINST (?1) > ?3\nORDER BY score DESC, n.id ASC\nLIMIT 3".to_owned(),
            query,
            score_alias: "score",
            score_descending: true,
            replacement_notes: vec![
                "Contradiction search retains the same top-3 contract for downstream TypeRisk filtering.",
                "Threshold must be retuned because MATCH() score distributions differ from FTS5.",
                "Uses natural language mode (Dolt does not yet support IN BOOLEAN MODE).",
            ],
        },
        (LexicalSearchBackend::MysqlFulltext, LexicalSearchMode::Discovery) => LexicalSearchPlan {
            backend,
            mode,
            sql: "SELECT n.id, CAST(MATCH(n.title, n.content, n.tags) AGAINST (?1) AS DOUBLE) AS fulltext_score\nFROM notes n\nWHERE n.project_id = ?2\n  AND MATCH(n.title, n.content, n.tags) AGAINST (?1)\nORDER BY fulltext_score DESC, n.id ASC\nLIMIT ?3".to_owned(),
            query,
            score_alias: "fulltext_score",
            score_descending: true,
            replacement_notes: vec![
                "Discovery remains lexical candidate generation for the RRF pipeline.",
                "No shadow table or trigger maintenance is required on MySQL/Dolt.",
                "Uses natural language mode (Dolt does not yet support IN BOOLEAN MODE).",
            ],
        },
    }))
}

pub fn validate_mysql_fulltext_threshold(threshold: f64) -> Result<()> {
    if threshold.is_sign_negative() {
        return Err(Error::InvalidData(
            "MySQL FULLTEXT thresholds must be non-negative MATCH() scores".to_owned(),
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
    fn mysql_query_sanitizer_requires_positive_terms() {
        assert_eq!(
            sanitize_mysql_boolean_query("rust sqlite _ bm25"),
            Some("+rust* +sqlite* +bm25*".to_owned())
        );
    }

    #[test]
    fn mysql_plan_replaces_shadow_table_with_match_against() {
        let plan = build_lexical_search_plan(
            LexicalSearchBackend::MysqlFulltext,
            LexicalSearchMode::Ranked,
            "rust sqlite",
        )
        .unwrap()
        .unwrap();

        assert!(
            plan.sql
                .contains("MATCH(n.title, n.content, n.tags) AGAINST (?1)")
        );
        assert!(plan.sql.contains("CAST("));
        assert!(!plan.sql.contains("IN BOOLEAN MODE"));
        assert!(!plan.sql.contains("notes_fts"));
        assert_eq!(plan.query, "+rust* +sqlite*");
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
    fn mysql_thresholds_must_be_non_negative() {
        assert!(validate_mysql_fulltext_threshold(0.0).is_ok());
        assert!(validate_mysql_fulltext_threshold(-0.1).is_err());
    }

    #[test]
    fn mysql_execution_sql_uses_mysql_placeholders() {
        let plan = build_lexical_search_plan(
            LexicalSearchBackend::MysqlFulltext,
            LexicalSearchMode::Ranked,
            "rust sqlite",
        )
        .unwrap()
        .unwrap();

        let sql = executable_lexical_search_sql(&plan);
        // `?1` positions are replaced with the sanitized query literal
        // (Dolt workaround: prepared MATCH AGAINST bindings are unsupported).
        assert!(sql.contains("AGAINST (\"+rust* +sqlite*\")"));
        assert!(!sql.contains("IN BOOLEAN MODE"));
        assert!(!sql.contains("?1"));
        assert!(!sql.contains("?2"));
        assert!(!plan.needs_query_bind());
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
        let mysql_plan = build_lexical_search_plan(
            LexicalSearchBackend::MysqlFulltext,
            LexicalSearchMode::Ranked,
            "rust sqlite",
        )
        .unwrap()
        .unwrap();

        assert_eq!(normalize_lexical_score(&sqlite_plan, -2.5), 2.5);
        assert_eq!(normalize_lexical_score(&mysql_plan, 2.5), 2.5);
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
                LexicalSearchBackend::MysqlFulltext,
                LexicalSearchMode::Contradiction,
            )
            .unwrap(),
            Some(0.0)
        );
    }
}
