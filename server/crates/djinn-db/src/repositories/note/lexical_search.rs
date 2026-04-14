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
            sql: "SELECT n.id, MATCH(n.title, n.content, n.tags) AGAINST (?1 IN BOOLEAN MODE) AS fulltext_score\nFROM notes n\nWHERE n.project_id = ?2\n  AND (?3 = '' OR n.folder = ?3)\n  AND (?4 = '' OR n.note_type = ?4)\n  AND MATCH(n.title, n.content, n.tags) AGAINST (?1 IN BOOLEAN MODE)\nORDER BY fulltext_score DESC, n.id ASC\nLIMIT ?5".to_owned(),
            query,
            score_alias: "fulltext_score",
            score_descending: true,
            replacement_notes: vec![
                "Runs directly against notes instead of a shadow notes_fts table.",
                "Assumes FULLTEXT INDEX notes_ft (title, content, tags) exists.",
            ],
        },
        (LexicalSearchBackend::MysqlFulltext, LexicalSearchMode::Dedup) => LexicalSearchPlan {
            backend,
            mode,
            sql: "SELECT n.id, n.permalink, n.title, n.folder, n.note_type, n.abstract, n.overview,\n       MATCH(n.title, n.content, n.tags) AGAINST (?1 IN BOOLEAN MODE) AS score\nFROM notes n\nWHERE n.project_id = ?2\n  AND n.folder = ?3\n  AND n.note_type = ?4\n  AND MATCH(n.title, n.content, n.tags) AGAINST (?1 IN BOOLEAN MODE) > ?5\nORDER BY score DESC, n.id ASC\nLIMIT ?6".to_owned(),
            query,
            score_alias: "score",
            score_descending: true,
            replacement_notes: vec![
                "Dedup no longer depends on bm25 sign inversion.",
                "Threshold placeholder remains required but must be recalibrated for MATCH() scores.",
            ],
        },
        (LexicalSearchBackend::MysqlFulltext, LexicalSearchMode::Contradiction) => LexicalSearchPlan {
            backend,
            mode,
            sql: "SELECT n.id, n.permalink, n.title, n.folder, n.note_type,\n       MATCH(n.title, n.content, n.tags) AGAINST (?1 IN BOOLEAN MODE) AS score\nFROM notes n\nWHERE n.id != ?2\n  AND MATCH(n.title, n.content, n.tags) AGAINST (?1 IN BOOLEAN MODE) > ?3\nORDER BY score DESC, n.id ASC\nLIMIT 3".to_owned(),
            query,
            score_alias: "score",
            score_descending: true,
            replacement_notes: vec![
                "Contradiction search retains the same top-3 contract for downstream TypeRisk filtering.",
                "Threshold must be retuned because MATCH() score distributions differ from FTS5.",
            ],
        },
        (LexicalSearchBackend::MysqlFulltext, LexicalSearchMode::Discovery) => LexicalSearchPlan {
            backend,
            mode,
            sql: "SELECT n.id, MATCH(n.title, n.content, n.tags) AGAINST (?1 IN BOOLEAN MODE) AS fulltext_score\nFROM notes n\nWHERE n.project_id = ?2\n  AND MATCH(n.title, n.content, n.tags) AGAINST (?1 IN BOOLEAN MODE)\nORDER BY fulltext_score DESC, n.id ASC\nLIMIT ?3".to_owned(),
            query,
            score_alias: "fulltext_score",
            score_descending: true,
            replacement_notes: vec![
                "Discovery remains lexical candidate generation for the RRF pipeline.",
                "No shadow table or trigger maintenance is required on MySQL/Dolt.",
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
                .contains("MATCH(n.title, n.content, n.tags) AGAINST (?1 IN BOOLEAN MODE)")
        );
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
}
