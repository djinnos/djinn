//! TypeScript / JavaScript `fetch(...)` consumer extraction.
//!
//! This module owns the UI-side route consumer scan for route extraction. It is
//! intentionally heuristic: it extracts static path literals from fetch call
//! arguments and only materializes `Fetches` edges when those paths resolve to
//! already-known `Route` nodes.

use std::collections::BTreeMap;
use std::path::Path;

use super::{
    RouteCandidate, RouteConsumerEdgeSuggestion, RouteExtractionReport,
    consumer_edge_exclusion_reasons, consumer_has_route_import_evidence, file_nodes,
    record_file_failure,
};
use crate::repo_graph::{
    RepoDependencyGraph, RepoGraphEdgeKind, RepoGraphNode, RepoGraphNodeKind, RouteExclusionConfig,
    promote_fetches_confidence_with_import_evidence,
};
use crate::scip_parser::ScipSymbolKind;
use petgraph::graph::NodeIndex;

/// One parsed TypeScript/JavaScript fetch call with a static path candidate.
#[derive(Debug, Clone, PartialEq)]
pub struct FetchHit {
    pub path: String,
    pub byte_offset: usize,
    pub confidence: f64,
    pub reason: &'static str,
}

/// Detect UI fetch consumers and add `Symbol -> Route` `Fetches` edges for
/// paths that match an already-materialized route node.
pub(super) fn detect_typescript_fetches(
    graph: &mut RepoDependencyGraph,
    project_root: &Path,
    routes_by_path: &BTreeMap<String, RouteCandidate>,
    exclusion_config: &RouteExclusionConfig,
    report: &mut RouteExtractionReport,
) {
    for (rel_path, file_node) in file_nodes(graph, is_typescript_fetch_candidate) {
        let source = match std::fs::read_to_string(project_root.join(&rel_path)) {
            Ok(source) => source,
            Err(error) => {
                record_file_failure(report, rel_path, error.to_string());
                continue;
            }
        };
        if !source.contains("fetch(") && !source.contains("request") {
            continue;
        }
        for fetch in scan_fetches(&source) {
            let Some(route) = resolve_fetch_route(&fetch.path, routes_by_path) else {
                report.unmatched_fetch_count += 1;
                continue;
            };
            let has_import_evidence = consumer_has_route_import_evidence(graph, file_node, route);
            let (confidence, reason) = if has_import_evidence {
                promote_fetches_confidence_with_import_evidence(
                    fetch.confidence,
                    Some(fetch.reason),
                )
            } else {
                (fetch.confidence, fetch.reason.to_string())
            };
            let exclusion_reasons = consumer_edge_exclusion_reasons(
                confidence,
                has_import_evidence,
                graph.node(file_node).language.as_deref(),
                graph.node(route.node).language.as_deref(),
                route,
                exclusion_config,
            );
            if !exclusion_reasons.is_empty() {
                report
                    .consumer_edge_suggestions
                    .push(RouteConsumerEdgeSuggestion {
                        consumer_file: rel_path.clone(),
                        fetch_path: fetch.path.clone(),
                        route_path: route.path.clone(),
                        framework: route.framework.clone(),
                        confidence,
                        reasons: exclusion_reasons,
                    });
                continue;
            }
            let line = byte_to_line(&source, fetch.byte_offset);
            if let Some(consumer) = enclosing_symbol(graph, &rel_path, line) {
                graph.add_route_edge(
                    consumer,
                    route.node,
                    RepoGraphEdgeKind::Fetches,
                    confidence,
                    &reason,
                );
                report.fetches_edges_added += 1;
            } else {
                report.unresolved_consumer_count += 1;
            }
        }
    }
}

pub(crate) fn is_typescript_fetch_candidate(lang: Option<&str>, path: &Path) -> bool {
    let language_matches = matches!(lang, Some("typescript" | "javascript"));
    let extension_matches = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| matches!(ext, "ts" | "js" | "tsx" | "jsx"));
    if !language_matches && !extension_matches {
        return false;
    }
    let path = path.to_string_lossy();
    path.starts_with("ui/src/api/") || path.starts_with("ui/src/components/")
}

fn resolve_fetch_route<'a>(
    fetch_path: &str,
    routes_by_path: &'a BTreeMap<String, RouteCandidate>,
) -> Option<&'a RouteCandidate> {
    routes_by_path
        .iter()
        .filter(|(route_path, _)| paths_match(fetch_path, route_path))
        .max_by_key(|(route_path, _)| match_quality(fetch_path, route_path))
        .map(|(_, route)| route)
}

fn match_quality(fetch_path: &str, route_path: &str) -> (u8, usize) {
    if fetch_path == route_path {
        (3, route_path.len())
    } else if path_matches_route_pattern(fetch_path, route_path) {
        (2, route_path.len())
    } else {
        (1, route_path.len())
    }
}

fn paths_match(fetch_path: &str, route_path: &str) -> bool {
    fetch_path == route_path
        || path_matches_route_pattern(fetch_path, route_path)
        || fetch_path.starts_with(route_path)
        || route_path.starts_with(fetch_path)
}

fn path_matches_route_pattern(fetch_path: &str, route_path: &str) -> bool {
    let fetch_segments: Vec<&str> = fetch_path.trim_matches('/').split('/').collect();
    let route_segments: Vec<&str> = route_path.trim_matches('/').split('/').collect();
    if fetch_segments.len() != route_segments.len() {
        return false;
    }
    fetch_segments
        .iter()
        .zip(route_segments.iter())
        .all(|(fetch, route)| fetch == route || is_route_param(fetch) || is_route_param(route))
}

fn is_route_param(segment: &str) -> bool {
    (segment.starts_with('{') && segment.ends_with('}')) || segment.starts_with(':')
}

pub(crate) fn scan_fetches(source: &str) -> Vec<FetchHit> {
    let mut out = Vec::new();
    scan_fetch_call_paths(source, &mut out);
    scan_named_call_paths(source, "request", &mut out);
    out.sort_by_key(|hit| hit.byte_offset);
    out
}

fn scan_fetch_call_paths(source: &str, out: &mut Vec<FetchHit>) {
    let mut cursor = 0;
    while let Some(pos) = source[cursor..].find("fetch(") {
        let start = cursor + pos + "fetch(".len();
        scan_call_argument_path(source, start, out);
        cursor = start;
    }
}

fn scan_named_call_paths(source: &str, name: &str, out: &mut Vec<FetchHit>) {
    let mut cursor = 0;
    while let Some(pos) = source[cursor..].find(name) {
        let ident_start = cursor + pos;
        let after_name = ident_start + name.len();
        if is_identifier_byte(source.as_bytes().get(ident_start.wrapping_sub(1)).copied())
            || is_identifier_byte(source.as_bytes().get(after_name).copied())
            || is_function_declaration(source, ident_start)
        {
            cursor = after_name;
            continue;
        }
        let mut call_start = skip_ws(source, after_name);
        if source.as_bytes().get(call_start) == Some(&b'<')
            && let Some(end) = find_matching_angle(source, call_start)
        {
            call_start = skip_ws(source, end + 1);
        }
        if source.as_bytes().get(call_start) == Some(&b'(') {
            scan_call_argument_path(source, call_start + 1, out);
        }
        cursor = after_name;
    }
}

fn scan_call_argument_path(source: &str, start: usize, out: &mut Vec<FetchHit>) {
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
}

fn is_identifier_byte(byte: Option<u8>) -> bool {
    byte.is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn is_function_declaration(source: &str, ident_start: usize) -> bool {
    source[..ident_start]
        .lines()
        .next_back()
        .is_some_and(|prefix| prefix.trim_end().ends_with("function"))
}

fn find_matching_angle(source: &str, start: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, byte) in source.as_bytes()[start..].iter().enumerate() {
        match byte {
            b'<' => depth += 1,
            b'>' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(start + offset);
                }
            }
            b'\n' | b';' => return None,
            _ => {}
        }
    }
    None
}

fn enclosing_symbol(graph: &RepoDependencyGraph, rel_path: &Path, line: u32) -> Option<NodeIndex> {
    graph
        .symbols_enclosing(rel_path, line, line)
        .into_iter()
        .filter(|node| is_enclosing_fetch_consumer_symbol(graph.node(*node)))
        .min_by_key(|node| {
            graph
                .range_for_node(*node, rel_path)
                .map(|(start, end)| end.saturating_sub(start))
                .unwrap_or(u32::MAX)
        })
}

pub(super) fn is_enclosing_fetch_consumer_symbol(node: &RepoGraphNode) -> bool {
    node.kind == RepoGraphNodeKind::Symbol
        && matches!(
            node.symbol_kind,
            Some(ScipSymbolKind::Function | ScipSymbolKind::Method | ScipSymbolKind::Constructor)
        )
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
            let mut path = String::new();
            let mut j = i;
            while j < bytes.len() {
                match bytes[j] {
                    b'`' | b'\'' | b'"' | b' ' | b'\n' | b'\r' | b'\t' | b'?' | b'#' => break,
                    b'$' if bytes.get(j + 1) == Some(&b'{') => {
                        path.push_str("{}");
                        j = skip_template_interpolation(bytes, j + 2).unwrap_or(bytes.len());
                    }
                    byte => {
                        path.push(byte as char);
                        j += 1;
                    }
                }
            }
            return (!path.is_empty()).then_some(path);
        }
        i += 1;
    }
    None
}

fn skip_template_interpolation(bytes: &[u8], mut i: usize) -> Option<usize> {
    let mut depth = 1usize;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
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

fn byte_to_line(source: &str, byte_offset: usize) -> u32 {
    1 + source[..source.len().min(byte_offset)]
        .bytes()
        .filter(|b| *b == b'\n')
        .count() as u32
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;

    use petgraph::visit::EdgeRef;

    use super::*;
    use crate::repo_graph::{
        REPO_GRAPH_ARTIFACT_VERSION, RepoGraphArtifact, RepoGraphArtifactSymbolRange,
        RepoGraphNode, RepoNodeKey,
    };
    use crate::scip_parser::ScipSymbolKind;

    fn fixture_node(
        id: RepoNodeKey,
        kind: RepoGraphNodeKind,
        display_name: &str,
        language: Option<&str>,
        file_path: Option<&str>,
        symbol: Option<&str>,
    ) -> RepoGraphNode {
        RepoGraphNode {
            id,
            kind,
            display_name: display_name.to_string(),
            language: language.map(str::to_string),
            file_path: file_path.map(PathBuf::from),
            symbol: symbol.map(str::to_string),
            symbol_kind: (kind == RepoGraphNodeKind::Symbol).then_some(ScipSymbolKind::Function),
            is_external: false,
            visibility: None,
            signature: None,
            documentation: Vec::new(),
            signature_parts: None,
            is_test: false,
            complexity: None,
            workspace: None,
            route_framework: None,
            route_handler_symbol: None,
        }
    }

    fn ts_symbol(path: &str, name: &str) -> RepoGraphNode {
        fixture_node(
            RepoNodeKey::Symbol(format!("ts {path}/{name}().")),
            RepoGraphNodeKind::Symbol,
            name,
            Some("typescript"),
            Some(path),
            Some(&format!("ts {path}/{name}().")),
        )
    }

    fn route_node(path: &str) -> RepoGraphNode {
        let label = format!("GET {path} (axum)");
        let mut node = fixture_node(
            RepoNodeKey::Route(label.clone()),
            RepoGraphNodeKind::Route,
            &label,
            Some("rust"),
            None,
            None,
        );
        node.route_framework = Some("axum".to_string());
        node
    }

    fn fixture_graph() -> RepoDependencyGraph {
        let project_tools = "ui/src/api/projectTools.ts";
        let chat_sessions = "ui/src/api/chatSessions.ts";
        let unknown = "ui/src/api/unknown.ts";
        let nodes = vec![
            fixture_node(
                RepoNodeKey::File(PathBuf::from(project_tools)),
                RepoGraphNodeKind::File,
                project_tools,
                Some("typescript"),
                Some(project_tools),
                None,
            ),
            ts_symbol(project_tools, "fetchMcpServers"),
            ts_symbol(project_tools, "createMcpServer"),
            ts_symbol(project_tools, "updateMcpServer"),
            ts_symbol(project_tools, "deleteMcpServer"),
            ts_symbol(project_tools, "fetchMcpDefaults"),
            ts_symbol(project_tools, "saveMcpDefaults"),
            fixture_node(
                RepoNodeKey::File(PathBuf::from(chat_sessions)),
                RepoGraphNodeKind::File,
                chat_sessions,
                Some("typescript"),
                Some(chat_sessions),
                None,
            ),
            ts_symbol(chat_sessions, "request"),
            ts_symbol(chat_sessions, "listChatSessions"),
            ts_symbol(chat_sessions, "getChatSessionMessages"),
            ts_symbol(chat_sessions, "deleteChatSession"),
            ts_symbol(chat_sessions, "renameChatSession"),
            fixture_node(
                RepoNodeKey::File(PathBuf::from(unknown)),
                RepoGraphNodeKind::File,
                unknown,
                Some("typescript"),
                Some(unknown),
                None,
            ),
            ts_symbol(unknown, "unknownFetch"),
            route_node("/project/mcp-servers"),
            route_node("/project/mcp-servers/update"),
            route_node("/project/mcp-servers/delete"),
            route_node("/project/mcp-defaults"),
            route_node("/api/chat/sessions"),
            route_node("/api/chat/sessions/{id}"),
            route_node("/api/chat/sessions/{id}/messages"),
        ];
        RepoDependencyGraph::from_artifact(&RepoGraphArtifact {
            version: REPO_GRAPH_ARTIFACT_VERSION,
            nodes,
            edges: Vec::new(),
            symbol_ranges: BTreeMap::from([
                (
                    PathBuf::from(project_tools),
                    vec![
                        RepoGraphArtifactSymbolRange {
                            start_line: 35,
                            end_line: 43,
                            node: 1,
                        },
                        RepoGraphArtifactSymbolRange {
                            start_line: 45,
                            end_line: 57,
                            node: 2,
                        },
                        RepoGraphArtifactSymbolRange {
                            start_line: 59,
                            end_line: 71,
                            node: 3,
                        },
                        RepoGraphArtifactSymbolRange {
                            start_line: 73,
                            end_line: 83,
                            node: 4,
                        },
                        RepoGraphArtifactSymbolRange {
                            start_line: 92,
                            end_line: 99,
                            node: 5,
                        },
                        RepoGraphArtifactSymbolRange {
                            start_line: 101,
                            end_line: 116,
                            node: 6,
                        },
                    ],
                ),
                (
                    PathBuf::from(chat_sessions),
                    vec![
                        RepoGraphArtifactSymbolRange {
                            start_line: 98,
                            end_line: 115,
                            node: 8,
                        },
                        RepoGraphArtifactSymbolRange {
                            start_line: 117,
                            end_line: 120,
                            node: 9,
                        },
                        RepoGraphArtifactSymbolRange {
                            start_line: 122,
                            end_line: 127,
                            node: 10,
                        },
                        RepoGraphArtifactSymbolRange {
                            start_line: 129,
                            end_line: 133,
                            node: 11,
                        },
                        RepoGraphArtifactSymbolRange {
                            start_line: 135,
                            end_line: 140,
                            node: 12,
                        },
                    ],
                ),
                (
                    PathBuf::from(unknown),
                    vec![RepoGraphArtifactSymbolRange {
                        start_line: 1,
                        end_line: 3,
                        node: 14,
                    }],
                ),
            ]),
            communities: Vec::new(),
            processes: Vec::new(),
            route_exclusion_config: Default::default(),
            layout_positions: BTreeMap::new(),
            galaxy_positions: BTreeMap::new(),
            galaxy_degrees: BTreeMap::new(),
        })
    }

    fn route_map(graph: &RepoDependencyGraph) -> BTreeMap<String, RouteCandidate> {
        graph
            .graph()
            .node_indices()
            .filter_map(|idx| {
                let node = graph.node(idx);
                if node.kind != RepoGraphNodeKind::Route {
                    return None;
                }
                let (_, rest) = node.display_name.split_once(' ')?;
                let path = rest.rsplit_once(" (").map_or(rest, |(path, _)| path);
                Some((
                    path.to_string(),
                    RouteCandidate {
                        path: path.to_string(),
                        node: idx,
                        framework: node.route_framework.clone(),
                    },
                ))
            })
            .collect()
    }

    #[test]
    fn scans_literal_and_template_fetch_shapes() {
        let fetches =
            scan_fetches("fetch(`${getServerBaseUrl()}/api/agents`, {})\nfetch('/api/missing')");
        assert_eq!(
            fetches
                .iter()
                .map(|f| (&f.path, f.reason, f.confidence))
                .collect::<Vec<_>>(),
            vec![
                (&"/api/agents".to_string(), "ts-fetch-template", 0.70),
                (&"/api/missing".to_string(), "ts-fetch-literal", 0.70),
            ]
        );
    }

    #[test]
    fn scans_real_ui_api_snapshots() {
        let project_tools = include_str!("tests/fixtures/projectTools.ts");
        let chat_sessions = include_str!("tests/fixtures/chatSessions.ts");

        let project_fetches = scan_fetches(project_tools);
        assert_eq!(project_fetches.len(), 6);
        assert!(
            project_fetches
                .iter()
                .all(|hit| hit.reason == "ts-fetch-template" && hit.confidence == 0.70)
        );
        assert_eq!(
            project_fetches
                .iter()
                .map(|hit| hit.path.as_str())
                .collect::<Vec<_>>(),
            vec![
                "/project/mcp-servers",
                "/project/mcp-servers",
                "/project/mcp-servers/update",
                "/project/mcp-servers/delete",
                "/project/mcp-defaults",
                "/project/mcp-defaults",
            ]
        );

        let chat_fetches = scan_fetches(chat_sessions);
        assert_eq!(chat_fetches.len(), 4);
        assert_eq!(
            chat_fetches
                .iter()
                .map(|hit| (hit.path.as_str(), hit.reason, hit.confidence))
                .collect::<Vec<_>>(),
            vec![
                ("/api/chat/sessions", "ts-fetch-literal", 0.70),
                ("/api/chat/sessions/{}/messages", "ts-fetch-template", 0.70),
                ("/api/chat/sessions/{}", "ts-fetch-template", 0.70),
                ("/api/chat/sessions/{}", "ts-fetch-template", 0.70),
            ]
        );
    }

    #[test]
    fn golden_snapshots_add_fetch_edges_without_synthetic_routes_for_unknowns() {
        let temp = tempfile::tempdir().expect("create temp fixture dir");
        let root = temp.path();
        std::fs::create_dir_all(root.join("ui/src/api")).unwrap();
        std::fs::write(
            root.join("ui/src/api/projectTools.ts"),
            include_str!("tests/fixtures/projectTools.ts"),
        )
        .unwrap();
        std::fs::write(
            root.join("ui/src/api/chatSessions.ts"),
            include_str!("tests/fixtures/chatSessions.ts"),
        )
        .unwrap();
        std::fs::write(
            root.join("ui/src/api/unknown.ts"),
            "export function unknownFetch() {\n  return fetch('/api/unknown-only');\n}\n",
        )
        .unwrap();

        let mut graph = fixture_graph();
        let routes_by_path = route_map(&graph);
        let route_nodes_before = graph
            .graph()
            .node_weights()
            .filter(|node| node.kind == RepoGraphNodeKind::Route)
            .count();
        let mut report = RouteExtractionReport::default();

        detect_typescript_fetches(
            &mut graph,
            root,
            &routes_by_path,
            &RouteExclusionConfig::default(),
            &mut report,
        );

        assert_eq!(report.fetches_edges_added, 10);
        assert_eq!(report.unmatched_fetch_count, 1);
        assert_eq!(report.unresolved_consumer_count, 0);
        assert_eq!(
            graph
                .graph()
                .node_weights()
                .filter(|node| node.kind == RepoGraphNodeKind::Route)
                .count(),
            route_nodes_before,
            "TS-only unknown paths must not synthesize Route nodes"
        );

        let fetch_edges: Vec<_> = graph
            .graph()
            .edge_references()
            .filter(|edge| edge.weight().kind == RepoGraphEdgeKind::Fetches)
            .collect();
        assert_eq!(fetch_edges.len(), 10);
        assert!(
            fetch_edges
                .iter()
                .all(|edge| edge.weight().confidence == 0.70)
        );
        let reasons: BTreeSet<&str> = fetch_edges
            .iter()
            .filter_map(|edge| edge.weight().reason.as_deref())
            .collect();
        assert_eq!(
            reasons,
            BTreeSet::from(["ts-fetch-literal", "ts-fetch-template"])
        );

        let edge_targets: BTreeSet<String> = fetch_edges
            .iter()
            .map(|edge| graph.node(edge.target()).display_name.clone())
            .collect();
        for expected in [
            "GET /project/mcp-servers (axum)",
            "GET /project/mcp-servers/update (axum)",
            "GET /project/mcp-servers/delete (axum)",
            "GET /project/mcp-defaults (axum)",
            "GET /api/chat/sessions (axum)",
        ] {
            assert!(
                edge_targets.contains(expected),
                "missing fetch edge to {expected}"
            );
        }
    }

    #[test]
    fn unknown_fetches_increment_counter_even_without_known_routes() {
        let temp = tempfile::tempdir().expect("create temp fixture dir");
        let root = temp.path();
        std::fs::create_dir_all(root.join("ui/src/api")).unwrap();
        std::fs::write(
            root.join("ui/src/api/unknown.ts"),
            "export function unknownFetch() {\n  return fetch('/api/unknown-only');\n}\n",
        )
        .unwrap();

        let mut graph = fixture_graph();
        let route_nodes_before = graph
            .graph()
            .node_weights()
            .filter(|node| node.kind == RepoGraphNodeKind::Route)
            .count();
        let mut report = RouteExtractionReport::default();

        detect_typescript_fetches(
            &mut graph,
            root,
            &BTreeMap::new(),
            &RouteExclusionConfig::default(),
            &mut report,
        );

        assert_eq!(report.fetches_edges_added, 0);
        assert_eq!(report.unmatched_fetch_count, 1);
        assert_eq!(
            graph
                .graph()
                .node_weights()
                .filter(|node| node.kind == RepoGraphNodeKind::Route)
                .count(),
            route_nodes_before,
            "TS-only unknown paths must not synthesize Route nodes"
        );
        assert_eq!(
            graph
                .graph()
                .edge_references()
                .filter(|edge| edge.weight().kind == RepoGraphEdgeKind::Fetches)
                .count(),
            0
        );
    }
}
