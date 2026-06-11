//! Best-effort HTTP route extraction wired into the canonical graph warm path.
//!
//! The pass is intentionally conservative and non-fatal: per-file read/parse
//! failures are reported in [`RouteExtractionReport`] and logged by the caller,
//! while the canonical graph continues to build from the SCIP-derived graph.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use petgraph::graph::NodeIndex;

use crate::repo_graph::{RepoDependencyGraph, RepoGraphEdgeKind, RepoGraphNodeKind, RepoNodeKey};

/// Summary emitted by [`detect_routes`] for rollout observability.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouteExtractionReport {
    pub route_nodes_added: usize,
    pub handles_route_edges_added: usize,
    pub fetches_edges_added: usize,
    pub unmatched_fetch_count: usize,
    pub unresolved_consumer_count: usize,
    pub skipped_files: Vec<PathBuf>,
    /// Per-file extraction failure messages. Multiple entries for the same
    /// file are allowed so callers can log each failure without losing order.
    pub file_failures: Vec<(PathBuf, Vec<String>)>,
}

/// Env-gate for route extraction. Default is on; set to `0`/`false`/`off`/`no`
/// to skip the pass.
pub fn route_detection_enabled() -> bool {
    !matches!(
        std::env::var("DJINN_ROUTE_DETECTION")
            .ok()
            .as_deref()
            .map(|s| s.to_ascii_lowercase()),
        Some(ref v) if matches!(v.as_str(), "0" | "false" | "off" | "no")
    )
}

/// Run server-side axum extraction first, then TypeScript fetch consumers so
/// consumer edges can resolve against the already-materialized Route nodes.
pub fn detect_routes(
    graph: &mut RepoDependencyGraph,
    project_root: &Path,
) -> RouteExtractionReport {
    let mut report = RouteExtractionReport::default();
    let mut routes_by_path = BTreeMap::new();
    detect_axum_routes(graph, project_root, &mut routes_by_path, &mut report);
    detect_typescript_fetches(graph, project_root, &routes_by_path, &mut report);
    report
}

fn detect_axum_routes(
    graph: &mut RepoDependencyGraph,
    project_root: &Path,
    routes_by_path: &mut BTreeMap<String, NodeIndex>,
    report: &mut RouteExtractionReport,
) {
    for (rel_path, _file_node) in file_nodes(graph, |lang, path| {
        lang == Some("rust") || path.extension().is_some_and(|e| e == "rs")
    }) {
        let source = match read_source(project_root, &rel_path, report) {
            Some(source) => source,
            None => continue,
        };
        if !source.contains(".route(") && !source.contains("route(") {
            continue;
        }
        for route in scan_axum_routes(&source) {
            let key = format!("{} {} (axum)", route.method, route.path);
            let handler = resolve_symbol_in_file(graph, &rel_path, &route.handler);
            let handler_symbol = handler.and_then(|idx| graph.node(idx).symbol.clone());
            let before_nodes = graph.node_count();
            let route_node = graph.ensure_route_node(
                &key,
                &key,
                Some("rust"),
                None,
                Some("axum"),
                handler_symbol.as_deref(),
            );
            if graph.node_count() > before_nodes {
                report.route_nodes_added += 1;
            }
            routes_by_path
                .entry(route.path.clone())
                .or_insert(route_node);
            if let Some(handler) = handler {
                graph.add_route_edge(
                    route_node,
                    handler,
                    RepoGraphEdgeKind::HandlesRoute,
                    0.90,
                    route.reason,
                );
                report.handles_route_edges_added += 1;
            } else {
                report.file_failures.push((
                    rel_path.clone(),
                    vec![format!("unresolved axum handler '{}'", route.handler)],
                ));
            }
        }
    }
}

fn detect_typescript_fetches(
    graph: &mut RepoDependencyGraph,
    project_root: &Path,
    routes_by_path: &BTreeMap<String, NodeIndex>,
    report: &mut RouteExtractionReport,
) {
    for (rel_path, _file_node) in file_nodes(graph, |lang, path| {
        matches!(lang, Some("typescript" | "javascript"))
            || path
                .extension()
                .is_some_and(|e| e == "ts" || e == "js" || e == "tsx" || e == "jsx")
    }) {
        let source = match read_source(project_root, &rel_path, report) {
            Some(source) => source,
            None => continue,
        };
        if !source.contains("fetch(") {
            continue;
        }
        for fetch in scan_fetches(&source) {
            let Some((_, &route_node)) = routes_by_path.iter().find(|(route_path, _)| {
                fetch.path == **route_path
                    || fetch.path.starts_with(route_path.as_str())
                    || route_path.starts_with(fetch.path.as_str())
            }) else {
                report.unmatched_fetch_count += 1;
                continue;
            };
            let line = byte_to_line(&source, fetch.byte_offset);
            if let Some(consumer) = enclosing_symbol(graph, &rel_path, line) {
                graph.add_route_edge(
                    consumer,
                    route_node,
                    RepoGraphEdgeKind::Fetches,
                    fetch.confidence,
                    fetch.reason,
                );
                report.fetches_edges_added += 1;
            } else {
                report.unresolved_consumer_count += 1;
            }
        }
    }
}

fn file_nodes<F>(graph: &RepoDependencyGraph, mut include: F) -> Vec<(PathBuf, NodeIndex)>
where
    F: FnMut(Option<&str>, &Path) -> bool,
{
    graph
        .graph()
        .node_indices()
        .filter_map(|idx| {
            let node = graph.node(idx);
            if node.kind != RepoGraphNodeKind::File {
                return None;
            }
            let path = match &node.id {
                RepoNodeKey::File(path) => path.clone(),
                _ => return None,
            };
            if include(node.language.as_deref(), &path) {
                Some((path, idx))
            } else {
                None
            }
        })
        .collect()
}

fn read_source(
    project_root: &Path,
    rel_path: &Path,
    report: &mut RouteExtractionReport,
) -> Option<String> {
    match std::fs::read_to_string(project_root.join(rel_path)) {
        Ok(source) => Some(source),
        Err(e) => {
            report.skipped_files.push(rel_path.to_path_buf());
            report
                .file_failures
                .push((rel_path.to_path_buf(), vec![e.to_string()]));
            None
        }
    }
}

#[derive(Debug, Clone)]
struct AxumRoute {
    path: String,
    method: String,
    handler: String,
    reason: &'static str,
}

fn scan_axum_routes(source: &str) -> Vec<AxumRoute> {
    let methods: BTreeSet<&'static str> = [
        "get", "post", "put", "delete", "patch", "head", "options", "trace", "any",
    ]
    .into_iter()
    .collect();
    let mut out = Vec::new();
    let mut cursor = 0;
    while let Some(pos) = source[cursor..].find(".route(") {
        let start = cursor + pos + ".route(".len();
        let Some((path, after_path)) = parse_quoted(source, start) else {
            cursor = start;
            continue;
        };
        let rest = &source[after_path..];
        let Some(comma) = rest.find(',') else {
            cursor = after_path;
            continue;
        };
        let call_start = after_path + comma + 1;
        let method_start = skip_ws(source, call_start);
        let method_end = read_ident_end(source, method_start);
        if method_end <= method_start {
            cursor = method_start.saturating_add(1);
            continue;
        }
        let method = &source[method_start..method_end];
        if !methods.contains(method) {
            cursor = method_end;
            continue;
        }
        let paren = skip_ws(source, method_end);
        if source.as_bytes().get(paren) != Some(&b'(') {
            cursor = method_end;
            continue;
        }
        let handler_start = skip_ws(source, paren + 1);
        let handler_end = read_path_ident_end(source, handler_start);
        if handler_end > handler_start {
            let reason = if path == "/health" || path == "/ping" {
                "axum-health"
            } else if path.contains('{') || path.contains(':') {
                "axum-param-only"
            } else {
                "axum-router-new"
            };
            out.push(AxumRoute {
                path,
                method: method.to_ascii_uppercase(),
                handler: source[handler_start..handler_end].to_string(),
                reason,
            });
        }
        cursor = method_end;
    }
    out
}

#[derive(Debug, Clone)]
struct FetchHit {
    path: String,
    byte_offset: usize,
    confidence: f64,
    reason: &'static str,
}

fn scan_fetches(source: &str) -> Vec<FetchHit> {
    let mut out = Vec::new();
    let mut cursor = 0;
    while let Some(pos) = source[cursor..].find("fetch(") {
        let start = cursor + pos + "fetch(".len();
        let arg_start = skip_ws(source, start);
        if source.as_bytes().get(arg_start) == Some(&b'`') {
            if let Some((template, _end)) = parse_until(source, arg_start + 1, '`')
                && let Some(path) = first_path_literal(&template)
            {
                out.push(FetchHit {
                    path,
                    byte_offset: arg_start,
                    confidence: 0.70,
                    reason: "ts-fetch-template",
                });
            }
        } else if let Some((literal, _end)) = parse_quoted(source, arg_start)
            && literal.starts_with('/')
        {
            out.push(FetchHit {
                path: literal,
                byte_offset: arg_start,
                confidence: 0.70,
                reason: "ts-fetch-literal",
            });
        } else if let Some(window) = source.get(arg_start..source.len().min(arg_start + 256))
            && let Some(path) = first_path_literal(window)
        {
            out.push(FetchHit {
                path,
                byte_offset: arg_start,
                confidence: 0.70,
                reason: "ts-fetch-template",
            });
        }
        cursor = start;
    }
    out
}

fn resolve_symbol_in_file(
    graph: &RepoDependencyGraph,
    rel_path: &Path,
    handler: &str,
) -> Option<NodeIndex> {
    let short = handler.rsplit("::").next().unwrap_or(handler);
    graph.symbol_ranges_by_file().find_map(|(path, ranges)| {
        if path != rel_path {
            return None;
        }
        ranges.iter().find_map(|range| {
            let node = graph.node(range.node);
            (node.kind == RepoGraphNodeKind::Symbol
                && (node.display_name == short
                    || node.display_name.ends_with(&format!("::{short}"))
                    || node.symbol.as_deref().is_some_and(|s| s.contains(short))))
            .then_some(range.node)
        })
    })
}

fn enclosing_symbol(graph: &RepoDependencyGraph, rel_path: &Path, line: u32) -> Option<NodeIndex> {
    graph
        .symbols_enclosing(rel_path, line, line)
        .into_iter()
        .max_by_key(|node| {
            graph
                .range_for_node(*node, rel_path)
                .map(|(start, end)| end.saturating_sub(start))
                .unwrap_or(0)
        })
}

fn parse_quoted(source: &str, start: usize) -> Option<(String, usize)> {
    let start = skip_ws(source, start);
    let quote = *source.as_bytes().get(start)?;
    if quote != b'\'' && quote != b'"' {
        return None;
    }
    parse_until(source, start + 1, quote as char)
}

fn parse_until(source: &str, start: usize, term: char) -> Option<(String, usize)> {
    let mut escaped = false;
    for (off, ch) in source[start..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == term {
            return Some((
                source[start..start + off].to_string(),
                start + off + ch.len_utf8(),
            ));
        }
    }
    None
}

fn first_path_literal(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'/' && bytes[i + 1].is_ascii_alphanumeric() {
            let end = (i + 1..bytes.len())
                .find(|&j| {
                    matches!(
                        bytes[j],
                        b'`' | b'\'' | b'"' | b' ' | b'\n' | b'\r' | b'\t' | b'?' | b'#' | b'$'
                    )
                })
                .unwrap_or(bytes.len());
            return Some(s[i..end].to_string());
        }
        i += 1;
    }
    None
}

fn skip_ws(source: &str, mut i: usize) -> usize {
    while source
        .as_bytes()
        .get(i)
        .is_some_and(u8::is_ascii_whitespace)
    {
        i += 1;
    }
    i
}

fn read_ident_end(source: &str, mut i: usize) -> usize {
    while source
        .as_bytes()
        .get(i)
        .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
    {
        i += 1;
    }
    i
}

fn read_path_ident_end(source: &str, mut i: usize) -> usize {
    while source
        .as_bytes()
        .get(i)
        .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b':')
    {
        i += 1;
    }
    i
}

fn byte_to_line(source: &str, byte_offset: usize) -> u32 {
    1 + source[..source.len().min(byte_offset)]
        .bytes()
        .filter(|b| *b == b'\n')
        .count() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_detection_env_defaults_on_and_can_be_disabled() {
        // SAFETY: test-only env mutation is scoped to this assertion.
        unsafe { std::env::remove_var("DJINN_ROUTE_DETECTION") };
        assert!(route_detection_enabled());
        unsafe { std::env::set_var("DJINN_ROUTE_DETECTION", "0") };
        assert!(!route_detection_enabled());
        unsafe { std::env::set_var("DJINN_ROUTE_DETECTION", "true") };
        assert!(route_detection_enabled());
        unsafe { std::env::remove_var("DJINN_ROUTE_DETECTION") };
    }

    #[test]
    fn scans_axum_and_typescript_shapes() {
        let routes = scan_axum_routes("Router::new().route(\"/api/agents\", get(list_agents))");
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].method, "GET");
        assert_eq!(routes[0].handler, "list_agents");

        let fetches =
            scan_fetches("fetch(`${getServerBaseUrl()}/api/agents`, {})\nfetch('/api/missing')");
        assert_eq!(
            fetches.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
            vec!["/api/agents", "/api/missing"]
        );
    }

    #[test]
    fn broken_file_is_skipped_without_poisoning_rest() {
        use crate::repo_graph::{RepoDependencyGraph, RepoGraphNodeKind};
        use crate::scip_parser::{ParsedScipIndex, ScipFile, ScipMetadata};

        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join("test-tmp")
            .join(format!("route-extraction-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/routes.rs"),
            "Router::new().route(\"/api/agents\", get(list_agents))",
        )
        .unwrap();

        let index = ParsedScipIndex {
            workspace_slug: "root".to_string(),
            metadata: ScipMetadata::default(),
            files: vec![
                ScipFile {
                    language: "rust".to_string(),
                    relative_path: PathBuf::from("src/routes.rs"),
                    definitions: vec![],
                    references: vec![],
                    occurrences: vec![],
                    symbols: vec![],
                },
                ScipFile {
                    language: "rust".to_string(),
                    relative_path: PathBuf::from("src/missing.rs"),
                    definitions: vec![],
                    references: vec![],
                    occurrences: vec![],
                    symbols: vec![],
                },
            ],
            external_symbols: vec![],
        };
        let mut graph = RepoDependencyGraph::build(&[index]);
        let report = detect_routes(&mut graph, &root);

        assert_eq!(report.route_nodes_added, 1);
        assert_eq!(report.skipped_files, vec![PathBuf::from("src/missing.rs")]);
        assert!(
            graph
                .graph()
                .node_weights()
                .any(|node| node.kind == RepoGraphNodeKind::Route)
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
