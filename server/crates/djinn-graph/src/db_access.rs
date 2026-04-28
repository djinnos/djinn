//! Detect raw-SQL database access in source bodies and materialize it
//! as `Symbol → Table` `Reads` / `Writes` edges in the canonical graph.
//!
//! This is the smallest viable cross-language data-layer signal: every
//! supported language ultimately compiles down to SQL strings somewhere
//! (sqlx macros, ORM raw escapes, query builders that interpolate the
//! verb), and a regex pass over symbol bodies catches them without per-
//! language ORM-specific intelligence. False positives (SQL inside doc
//! comments, string-builder fragments) are accepted as the cost of
//! coverage; the resulting edges are stamped with confidence 0.85
//! (matching the `Reads`/`Writes` floor) and a `reason` of `"raw-sql"`
//! so downstream consumers can filter.
//!
//! Off-by-default behind `DJINN_DB_ACCESS_DETECTION` — opt in once the
//! signal proves useful in practice.

use std::path::Path;

use crate::repo_graph::{
    RepoDependencyGraph, RepoGraphEdgeKind, RepoGraphNodeKind, SymbolRange,
};

/// Env-gate for [`detect_db_access`]. Set to `1` / `true` / `on` to
/// enable; any other value (or unset) keeps the pass off.
pub fn db_access_detection_enabled() -> bool {
    matches!(
        std::env::var("DJINN_DB_ACCESS_DETECTION")
            .ok()
            .as_deref()
            .map(|s| s.to_ascii_lowercase()),
        Some(ref v) if matches!(v.as_str(), "1" | "true" | "on" | "yes")
    )
}

/// Walk every symbol in the per-file `symbol_ranges` sidecar, slice the
/// source body, regex-match SQL, and stamp `Reads`/`Writes` edges from
/// the symbol to a synthetic `Table` node. Logs and skips files that
/// can't be read; never panics. Returns the number of `(symbol, table)`
/// edges materialized so the caller can include it in warm telemetry.
pub fn detect_db_access(graph: &mut RepoDependencyGraph, project_root: &Path) -> usize {
    let mut edges_added: usize = 0;

    let work: Vec<(std::path::PathBuf, Vec<SymbolRange>)> = graph
        .symbol_ranges_by_file()
        .map(|(path, ranges)| (path.to_path_buf(), ranges.to_vec()))
        .collect();

    for (rel_path, ranges) in work {
        let abs_path = project_root.join(&rel_path);
        let Ok(contents) = std::fs::read_to_string(&abs_path) else {
            continue;
        };
        let lines: Vec<&str> = contents.lines().collect();
        if lines.is_empty() {
            continue;
        }

        for range in ranges {
            // Only credit callable symbols — fields/types don't have
            // bodies that contain SQL. Skip nodes whose kind isn't
            // Symbol (no Process / File / Table cross-pollination).
            let node_kind = graph.node(range.node).kind;
            if node_kind != RepoGraphNodeKind::Symbol {
                continue;
            }

            let start = (range.start_line as usize).saturating_sub(1);
            let end = (range.end_line as usize).min(lines.len());
            if start >= end {
                continue;
            }
            let body = lines[start..end].join("\n");

            for hit in scan_sql(&body) {
                let table_idx = graph.ensure_table_node(&hit.table);
                let reason = format!("raw-sql:{}", hit.verb);
                graph.add_table_access_edge(range.node, table_idx, hit.kind, &reason);
                edges_added += 1;
            }
        }
    }

    edges_added
}

/// One SQL hit inside a symbol body. `table` is the (un-normalized)
/// table identifier as it appeared in the source — `ensure_table_node`
/// lowercases it before keying the synthetic node.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SqlHit {
    /// Lowercase verb (`"select"`, `"insert"`, `"update"`, `"delete"`).
    verb: &'static str,
    /// Whether the verb mutates state. `Reads` for `SELECT`, `Writes`
    /// for everything else.
    kind: RepoGraphEdgeKind,
    table: String,
}

/// Scan a function body for SQL access patterns. Recognizes:
///   - `SELECT ... FROM <table>`           → `Reads`
///   - `INSERT INTO <table>`               → `Writes`
///   - `UPDATE <table>`                    → `Writes`
///   - `DELETE FROM <table>`               → `Writes`
///
/// Quoted identifiers (`` `users` ``, `"users"`, `'users'`) and
/// schema-qualified names (`public.users`) are accepted. The parser
/// is intentionally lenient — it walks the body once and emits a hit
/// per detected verb. Multi-table joins capture only the first table
/// after `FROM`.
fn scan_sql(body: &str) -> Vec<SqlHit> {
    let mut out: Vec<SqlHit> = Vec::new();
    let bytes = body.as_bytes();
    let lower = body.to_ascii_lowercase();
    let mut i: usize = 0;

    while i < lower.len() {
        // Cheap prefix match against each verb's leading keyword. The
        // body's lowercase copy is the search target; offsets line up
        // with the original `body` byte-for-byte (lower/upper ASCII
        // share width).
        let rest = &lower[i..];

        if let Some(table) = match_after(rest, "select", "from") {
            out.push(SqlHit {
                verb: "select",
                kind: RepoGraphEdgeKind::Reads,
                table,
            });
            i += 1;
            continue;
        }
        if let Some(table) = match_after(rest, "insert", "into") {
            out.push(SqlHit {
                verb: "insert",
                kind: RepoGraphEdgeKind::Writes,
                table,
            });
            i += 1;
            continue;
        }
        if let Some(table) = match_keyword_then_name(rest, "update") {
            out.push(SqlHit {
                verb: "update",
                kind: RepoGraphEdgeKind::Writes,
                table,
            });
            i += 1;
            continue;
        }
        if let Some(table) = match_after(rest, "delete", "from") {
            out.push(SqlHit {
                verb: "delete",
                kind: RepoGraphEdgeKind::Writes,
                table,
            });
            i += 1;
            continue;
        }

        // Advance one byte; `bytes` is reachable but unused — the
        // ASCII-only assumption is good enough for keyword scanning.
        let _ = bytes;
        i += 1;
    }

    dedupe(out)
}

/// Match `<verb> ... <connector> <table>` starting at the head of `s`.
/// Returns the table identifier if the connector is found within
/// `MAX_GAP` chars of the verb; this bounds the scan so a `select`
/// keyword isolated in a comment doesn't drag in a `from` from much
/// later in the body.
fn match_after(s: &str, verb: &str, connector: &str) -> Option<String> {
    const MAX_GAP: usize = 256;

    if !starts_with_kw(s, verb) {
        return None;
    }
    let after_verb = &s[verb.len()..];
    let scan_window = &after_verb[..after_verb.len().min(MAX_GAP)];
    // Find connector as a standalone keyword.
    let conn_at = find_kw(scan_window, connector)?;
    let after_conn = &scan_window[conn_at + connector.len()..];
    parse_table(after_conn)
}

/// Match `<verb> <table>` where `<verb>` is immediately followed by
/// whitespace then the table identifier. Used for `UPDATE <table>`.
fn match_keyword_then_name(s: &str, verb: &str) -> Option<String> {
    if !starts_with_kw(s, verb) {
        return None;
    }
    parse_table(&s[verb.len()..])
}

/// True when `s` starts with `kw` and the next char is a word boundary.
fn starts_with_kw(s: &str, kw: &str) -> bool {
    if !s.starts_with(kw) {
        return false;
    }
    let rest = &s[kw.len()..];
    rest.is_empty() || !is_ident_char(rest.as_bytes()[0])
}

/// Find `kw` anywhere in `s`, treated as a standalone keyword (word
/// boundaries on both sides).
fn find_kw(s: &str, kw: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let kb = kw.as_bytes();
    let mut i: usize = 0;
    while i + kb.len() <= bytes.len() {
        if &bytes[i..i + kb.len()] == kb {
            let prev_ok = i == 0 || !is_ident_char(bytes[i - 1]);
            let next_ok = i + kb.len() == bytes.len() || !is_ident_char(bytes[i + kb.len()]);
            if prev_ok && next_ok {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Parse a table identifier from the head of `s`. Accepts:
///   - bare identifiers: `users`, `users_v2`
///   - schema-qualified: `public.users`
///   - quoted: `` `users` ``, `"users"`, `'users'` (quote stripped)
///
/// Skips leading whitespace. Returns `None` when the head doesn't
/// look like a real table name (parameter placeholder `?`, SQL
/// keyword, opening paren of a subquery).
fn parse_table(s: &str) -> Option<String> {
    let s = s.trim_start();
    if s.is_empty() {
        return None;
    }
    let bytes = s.as_bytes();
    // Strip a single leading quote, if any — and stop at the matching
    // close quote.
    let (quote, body): (Option<u8>, &str) = match bytes[0] {
        b'`' | b'"' | b'\'' => (Some(bytes[0]), &s[1..]),
        _ => (None, s),
    };
    if body.is_empty() {
        return None;
    }
    let body_bytes = body.as_bytes();
    let mut end: usize = 0;
    while end < body_bytes.len() {
        let c = body_bytes[end];
        if let Some(q) = quote {
            if c == q {
                break;
            }
        } else if !is_ident_char(c) && c != b'.' {
            break;
        }
        end += 1;
    }
    if end == 0 {
        return None;
    }
    let raw = &body[..end];
    // Reject obvious non-tables: keywords used as identifiers, the
    // single-char placeholder `(`, etc. The keyword check is cheap and
    // catches `select * from select ...` malformed inputs.
    if raw.eq_ignore_ascii_case("select")
        || raw.eq_ignore_ascii_case("where")
        || raw.eq_ignore_ascii_case("from")
        || raw.eq_ignore_ascii_case("set")
        || raw.eq_ignore_ascii_case("values")
    {
        return None;
    }
    Some(raw.to_string())
}

fn is_ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Dedupe `(verb, lowercased-table)` so a function with three
/// `SELECT ... FROM users` queries materializes just one edge.
fn dedupe(mut hits: Vec<SqlHit>) -> Vec<SqlHit> {
    hits.sort_by(|a, b| {
        a.verb
            .cmp(b.verb)
            .then_with(|| a.table.to_ascii_lowercase().cmp(&b.table.to_ascii_lowercase()))
    });
    hits.dedup_by(|a, b| {
        a.verb == b.verb && a.table.eq_ignore_ascii_case(&b.table)
    });
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_simple_select() {
        let hits = scan_sql("let q = \"SELECT id, name FROM users WHERE id = ?\";");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].verb, "select");
        assert_eq!(hits[0].table, "users");
        assert_eq!(hits[0].kind, RepoGraphEdgeKind::Reads);
    }

    #[test]
    fn detects_insert_into() {
        let hits = scan_sql("sqlx::query!(\"INSERT INTO orders (sku) VALUES (?)\", sku);");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].verb, "insert");
        assert_eq!(hits[0].table, "orders");
        assert_eq!(hits[0].kind, RepoGraphEdgeKind::Writes);
    }

    #[test]
    fn detects_update_and_delete() {
        let hits = scan_sql(
            "UPDATE accounts SET balance = balance + 10 WHERE id = ?; \
             DELETE FROM sessions WHERE expires < NOW();",
        );
        assert_eq!(hits.len(), 2);
        let verbs: Vec<&str> = hits.iter().map(|h| h.verb).collect();
        assert!(verbs.contains(&"update"));
        assert!(verbs.contains(&"delete"));
        for h in &hits {
            assert_eq!(h.kind, RepoGraphEdgeKind::Writes);
        }
    }

    #[test]
    fn handles_schema_qualified_and_quoted_names() {
        let hits = scan_sql("SELECT * FROM public.users; SELECT 1 FROM `events`;");
        let tables: Vec<String> = hits.iter().map(|h| h.table.clone()).collect();
        assert!(tables.iter().any(|t| t == "public.users"));
        assert!(tables.iter().any(|t| t == "events"));
    }

    #[test]
    fn dedupes_repeated_hits() {
        let body = r#"
            SELECT * FROM users;
            SELECT id FROM users;
            SELECT name FROM USERS;
        "#;
        let hits = scan_sql(body);
        assert_eq!(hits.len(), 1, "expected one deduped hit, got {hits:?}");
    }

    #[test]
    fn ignores_select_without_from() {
        // `SELECT 1` (constant select, no table) shouldn't emit
        // anything — `parse_table` rejects keywords and digits don't
        // match identifier semantics.
        let hits = scan_sql("let v = sqlx::query_scalar!(\"SELECT 1\");");
        assert!(
            hits.is_empty(),
            "constant SELECT should not produce a hit: {hits:?}"
        );
    }

    #[test]
    fn env_gate_default_off() {
        // SAFETY: tests run single-threaded by default within a module
        // unless cargo-nextest interleaves; the `#[cfg(test)]` env-var
        // mutation is acceptable for a flag-default smoke test.
        unsafe {
            std::env::remove_var("DJINN_DB_ACCESS_DETECTION");
        }
        assert!(!db_access_detection_enabled());
    }
}
