//! Axum server-side route extraction.
//!
//! The extractor is deliberately lightweight: SCIP already tells us which
//! Rust files reference axum, then this module scans the source for the common
//! `Router::new().route("/path", get(handler).post(other))` registration shape.

use std::path::{Path, PathBuf};

use petgraph::graph::NodeIndex;
use petgraph::visit::EdgeRef;

use crate::repo_graph::{RepoDependencyGraph, RepoGraphEdgeKind, RepoGraphNodeKind};

const METHODS: &[&str] = &[
    "get", "post", "put", "delete", "patch", "head", "options", "trace", "any",
];

/// One parsed axum route registration.
///
/// Dedup discipline mirrors graphify's inferred-edge guardrails: identical
/// labels are only eligible to merge when they come from the same `file`, Tool
/// extraction must apply the same same-file rule plus an absolute
/// cross-project/cross-repo merge ban, and low-entropy labels (health/ping/root
/// style affordances) are never merged. This keeps inferred Route/Tool nodes
/// from manufacturing phantom architecture across files or repositories.
#[derive(Debug, Clone, PartialEq)]
pub struct AxumRouteHit {
    pub file: PathBuf,
    pub path: String,
    pub method: String,
    pub handler: String,
    pub route_node: Option<NodeIndex>,
    pub handler_node: Option<NodeIndex>,
    pub confidence: f64,
    pub reason: String,
}

/// Summary of an axum route extraction pass.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct RouteExtractionReport {
    pub hits: Vec<AxumRouteHit>,
    pub routes_added: usize,
    pub handles_route_edges_added: usize,
    pub skipped_files: usize,
}

/// Detect axum `Router::new().route(...)` registrations and materialize
/// `Route` nodes plus `Route -> Symbol` `HandlesRoute` edges.
pub fn detect_axum_routes(
    graph: &mut RepoDependencyGraph,
    project_root: &Path,
) -> RouteExtractionReport {
    let mut report = RouteExtractionReport::default();
    let candidates: Vec<(PathBuf, Option<String>)> = graph
        .graph()
        .node_indices()
        .filter_map(|idx| {
            let node = graph.node(idx);
            if node.kind != RepoGraphNodeKind::File {
                return None;
            }
            if node.language.as_deref() != Some("rust") {
                return None;
            }
            let path = node.file_path.clone()?;
            let workspace = node.workspace.clone();
            has_axum_router_reference(graph, idx).then_some((path, workspace))
        })
        .collect();

    for (rel_path, workspace) in candidates {
        let source = match std::fs::read_to_string(project_root.join(&rel_path)) {
            Ok(source) => source,
            Err(_) => {
                report.skipped_files += 1;
                continue;
            }
        };

        for (hit_ordinal, mut hit) in parse_axum_routes_in_source(&source, &rel_path)
            .into_iter()
            .enumerate()
        {
            let resolved_handler_node = resolve_handler_symbol(graph, &rel_path, &hit.handler);
            let reason = route_reason(&hit.path, resolved_handler_node.is_some());
            let confidence = if resolved_handler_node.is_some() {
                0.90
            } else {
                0.45
            };
            let handler_source_file =
                resolved_handler_node.and_then(|idx| graph.node(idx).file_path.clone());
            let handler_symbol =
                resolved_handler_node.and_then(|idx| graph.node(idx).symbol.clone());
            let handler_node = resolved_handler_node.unwrap_or_else(|| {
                graph.ensure_unresolved_route_handler_node(&rel_path, &hit.handler)
            });
            let label = format!("{} {} (axum)", hit.method.to_uppercase(), hit.path);
            let route_id = route_node_id(&label, &rel_path, hit_ordinal, &hit.path);
            let route_node = graph.ensure_route_node(
                &route_id,
                &label,
                Some("rust"),
                workspace.as_deref(),
                handler_source_file.as_deref(),
                Some("axum"),
                handler_symbol.as_deref(),
            );
            report.routes_added += 1;
            graph.add_handles_route_edge(route_node, handler_node, &reason, Some(confidence));
            report.handles_route_edges_added += 1;
            hit.route_node = Some(route_node);
            hit.handler_node = Some(handler_node);
            hit.confidence = confidence;
            hit.reason = reason;
            report.hits.push(hit);
        }
    }

    report
}

fn has_axum_router_reference(graph: &RepoDependencyGraph, file_idx: NodeIndex) -> bool {
    graph
        .graph()
        .edges_directed(file_idx, petgraph::Direction::Outgoing)
        .any(|edge| {
            if edge.weight().kind != RepoGraphEdgeKind::FileReference {
                return false;
            }
            let target = graph.node(edge.target());
            let symbol = target.symbol.as_deref().unwrap_or_default();
            let display_name = target.display_name.as_str();

            // rust-analyzer SCIP symbols are not Rust paths; they usually look
            // like package-qualified ids containing ` axum ` or `/axum/`, with
            // the final item (`Router`, `get`, `post`, ...) carried separately
            // as display_name. Keep accepting literal Rust paths for compact
            // test fixtures and hand-built graphs.
            is_axum_router_symbol(symbol, display_name)
                || is_axum_routing_symbol(symbol, display_name)
        })
}

fn is_axum_router_symbol(symbol: &str, display_name: &str) -> bool {
    (symbol.contains("axum::Router")
        || symbol.ends_with("/axum/Router#")
        || symbol.contains(" axum ")
        || symbol.contains("/axum/")
        || symbol.contains("`axum`"))
        && display_name == "Router"
}

fn is_axum_routing_symbol(symbol: &str, display_name: &str) -> bool {
    (symbol.contains("axum::routing")
        || symbol.ends_with("/axum/routing/")
        || symbol.contains(" axum ")
        || symbol.contains("/axum/")
        || symbol.contains("`axum`"))
        && METHODS.contains(&display_name)
}

fn resolve_handler_symbol(
    graph: &RepoDependencyGraph,
    rel_path: &Path,
    handler: &str,
) -> Option<NodeIndex> {
    let needle = handler.rsplit("::").next().unwrap_or(handler);
    graph.graph().node_indices().find(|idx| {
        let node = graph.node(*idx);
        node.kind == RepoGraphNodeKind::Symbol
            && node.file_path.as_deref() == Some(rel_path)
            && (node.display_name == needle
                || node.display_name == handler
                || node.symbol.as_deref().is_some_and(|symbol| {
                    symbol.ends_with(&format!("{needle}()."))
                        || symbol.ends_with(&format!("{needle}#"))
                        || symbol == handler
                }))
    })
}

fn route_reason(path: &str, resolved: bool) -> String {
    if is_health_route(path) {
        "axum-health".to_string()
    } else if is_param_only_route(path) {
        "axum-param-only".to_string()
    } else if resolved {
        "axum-router-new".to_string()
    } else {
        "axum-unresolved-handler".to_string()
    }
}

fn is_health_route(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p == "/health" || p == "/ping" || p.ends_with("/health") || p.ends_with("/ping")
}

fn is_param_only_route(path: &str) -> bool {
    let trimmed = path.trim_matches('/');
    !trimmed.is_empty()
        && trimmed.split('/').all(|segment| {
            segment.starts_with(':') || (segment.starts_with('{') && segment.ends_with('}'))
        })
}

fn parse_axum_routes_in_source(source: &str, rel_path: &Path) -> Vec<AxumRouteHit> {
    let mut hits = Vec::new();
    let mut offset = 0;
    while let Some(found) = source[offset..].find(".route") {
        let route_start = offset + found;
        let Some(open_rel) = source[route_start..].find('(') else {
            break;
        };
        let open = route_start + open_rel;
        let Some(close) = find_matching_paren(source, open) else {
            offset = open + 1;
            continue;
        };
        let args = &source[open + 1..close];
        if let Some((path, handler_expr)) = split_route_args(args) {
            for (method, handler) in parse_method_handlers(handler_expr) {
                hits.push(AxumRouteHit {
                    file: rel_path.to_path_buf(),
                    path: path.clone(),
                    method,
                    handler,
                    route_node: None,
                    handler_node: None,
                    confidence: 0.0,
                    reason: String::new(),
                });
            }
        }
        offset = close + 1;
    }
    dedupe_hits(hits)
}

fn split_route_args(args: &str) -> Option<(String, &str)> {
    let args = args.trim_start();
    let (path, after_path) = parse_rust_string(args)?;
    let comma = after_path.find(',')?;
    Some((path, after_path[comma + 1..].trim()))
}

fn parse_method_handlers(expr: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < expr.len() {
        if i > 0 && !is_ident_boundary(expr, i - 1) {
            i += 1;
            continue;
        }
        let tail = &expr[i..];
        let Some(method) = METHODS.iter().find(|m| tail.starts_with(**m)) else {
            i += 1;
            continue;
        };
        let after_method = i + method.len();
        if !is_ident_boundary(expr, after_method) {
            i += 1;
            continue;
        }
        let after_ws = skip_ws(expr, after_method);
        if expr.as_bytes().get(after_ws) != Some(&b'(') {
            i += 1;
            continue;
        }
        if let Some(close) = find_matching_paren(expr, after_ws) {
            let inner = expr[after_ws + 1..close].trim();
            if let Some(handler) = parse_handler(inner) {
                out.push(((*method).to_string(), handler));
            }
            i = close + 1;
        } else {
            break;
        }
    }
    out
}

fn parse_handler(inner: &str) -> Option<String> {
    let candidate = inner.split(',').next()?.trim();
    if candidate.is_empty() {
        return None;
    }
    if candidate
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b':' | b'<' | b'>' | b'-'))
    {
        Some(candidate.trim_end_matches("::<>").to_string())
    } else {
        None
    }
}

fn parse_rust_string(s: &str) -> Option<(String, &str)> {
    if let Some(rest) = s.strip_prefix('"') {
        let mut escaped = false;
        for (idx, ch) in rest.char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                '"' => return Some((rest[..idx].to_string(), &rest[idx + 1..])),
                _ => {}
            }
        }
        return None;
    }
    if let Some(hash_start) = s.strip_prefix('r') {
        let hashes = hash_start.bytes().take_while(|b| *b == b'#').count();
        if hash_start.as_bytes().get(hashes) != Some(&b'"') {
            return None;
        }
        let body = &hash_start[hashes + 1..];
        let terminator = format!("\"{}", "#".repeat(hashes));
        let end = body.find(&terminator)?;
        return Some((body[..end].to_string(), &body[end + terminator.len()..]));
    }
    None
}

fn find_matching_paren(s: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (idx, ch) in s[open..].char_indices() {
        let abs = open + idx;
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(abs);
                }
            }
            _ => {}
        }
    }
    None
}

fn skip_ws(s: &str, mut i: usize) -> usize {
    while s.as_bytes().get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    i
}

fn is_ident_boundary(s: &str, i: usize) -> bool {
    if i >= s.len() {
        return true;
    }
    !s.as_bytes()[i].is_ascii_alphanumeric() && s.as_bytes()[i] != b'_'
}

fn dedupe_hits(mut hits: Vec<AxumRouteHit>) -> Vec<AxumRouteHit> {
    hits.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then_with(|| a.method.cmp(&b.method))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.handler.cmp(&b.handler))
    });
    hits.dedup_by(|a, b| {
        a.file == b.file
            && a.method == b.method
            && a.path == b.path
            && a.handler == b.handler
            && !is_low_entropy_route_label(&a.path)
            && !is_low_entropy_route_label(&b.path)
    });
    hits
}

fn is_low_entropy_route_label(path: &str) -> bool {
    let trimmed = path.trim().trim_matches('/');
    trimmed.is_empty()
        || matches!(
            trimmed.to_ascii_lowercase().as_str(),
            "health" | "ping" | "status"
        )
        || is_param_only_route(path)
}

fn route_node_id(label: &str, file: &Path, ordinal: usize, path: &str) -> String {
    if is_low_entropy_route_label(path) {
        format!("{label} [{}#{}]", file.display(), ordinal + 1)
    } else {
        format!("{label} [{}]", file.display())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::repo_graph::{
        RepoGraphArtifact, RepoGraphArtifactEdge, RepoGraphNode, RepoNodeKey, edge_confidence_floor,
    };
    use crate::scip_parser::ScipSymbolKind;

    fn fixture_graph(files: &[(&str, &str)]) -> RepoDependencyGraph {
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        let mut symbol_positions: BTreeMap<String, usize> = BTreeMap::new();

        let router_pos = push_symbol(&mut nodes, &mut symbol_positions, "axum::Router", None);
        let routing_pos = push_symbol(
            &mut nodes,
            &mut symbol_positions,
            "axum::routing::get",
            None,
        );

        for (path, source) in files {
            let file_pos = nodes.len();
            nodes.push(RepoGraphNode {
                id: RepoNodeKey::File(PathBuf::from(path)),
                kind: RepoGraphNodeKind::File,
                display_name: (*path).to_string(),
                language: Some("rust".to_string()),
                file_path: Some(PathBuf::from(path)),
                symbol: None,
                symbol_kind: None,
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
            });
            for target in [router_pos, routing_pos] {
                edges.push(artifact_edge(
                    file_pos,
                    target,
                    RepoGraphEdgeKind::FileReference,
                    None,
                ));
            }
            for name in function_names(source) {
                push_function_symbol(&mut nodes, &mut symbol_positions, &name, path);
            }
        }

        RepoDependencyGraph::from_artifact(&RepoGraphArtifact {
            version: crate::repo_graph::REPO_GRAPH_ARTIFACT_VERSION,
            nodes,
            edges,
            symbol_ranges: BTreeMap::new(),
            communities: Vec::new(),
            processes: Vec::new(),
            route_exclusion_config: Default::default(),
            layout_positions: BTreeMap::new(),
            galaxy_positions: BTreeMap::new(),
            galaxy_degrees: BTreeMap::new(),
        })
    }

    fn push_symbol(
        nodes: &mut Vec<RepoGraphNode>,
        positions: &mut BTreeMap<String, usize>,
        symbol: &str,
        file_path: Option<&str>,
    ) -> usize {
        if let Some(pos) = positions.get(symbol) {
            return *pos;
        }
        let display_name = symbol.rsplit("::").next().unwrap_or(symbol).to_string();
        let pos = nodes.len();
        nodes.push(RepoGraphNode {
            id: RepoNodeKey::Symbol(symbol.to_string()),
            kind: RepoGraphNodeKind::Symbol,
            display_name,
            language: Some("rust".to_string()),
            file_path: file_path.map(PathBuf::from),
            symbol: Some(symbol.to_string()),
            symbol_kind: Some(ScipSymbolKind::Function),
            is_external: file_path.is_none(),
            visibility: None,
            signature: None,
            documentation: Vec::new(),
            signature_parts: None,
            is_test: false,
            complexity: None,
            workspace: None,
            route_framework: None,
            route_handler_symbol: None,
        });
        positions.insert(symbol.to_string(), pos);
        pos
    }

    fn push_function_symbol(
        nodes: &mut Vec<RepoGraphNode>,
        positions: &mut BTreeMap<String, usize>,
        name: &str,
        file_path: &str,
    ) -> usize {
        let symbol = format!("test {file_path}/{name}().");
        push_symbol(nodes, positions, &symbol, Some(file_path))
    }

    fn artifact_edge(
        source: usize,
        target: usize,
        kind: RepoGraphEdgeKind,
        reason: Option<String>,
    ) -> RepoGraphArtifactEdge {
        RepoGraphArtifactEdge {
            source,
            target,
            kind,
            weight: crate::repo_graph::edge_weight(kind),
            evidence_count: 1,
            confidence: edge_confidence_floor(kind),
            reason,
            step: None,
        }
    }

    fn function_names(source: &str) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        for line in source.lines() {
            let trimmed = line.trim_start();
            let candidate = trimmed
                .strip_prefix("async fn ")
                .or_else(|| trimmed.strip_prefix("fn "));
            if let Some(rest) = candidate
                && let Some(name) = rest
                    .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
                    .next()
                    .filter(|name| !name.is_empty())
            {
                names.insert(name.to_string());
            }
        }
        names
    }

    #[test]
    fn parses_agents_and_oauth_fixture_routes() {
        let root =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("src/route_extraction/tests/fixtures");
        let agents = include_str!("tests/fixtures/agents.rs");
        let oauth = include_str!("tests/fixtures/oauth.rs");
        let mut graph = fixture_graph(&[("agents.rs", agents), ("oauth.rs", oauth)]);

        let report = detect_axum_routes(&mut graph, &root);

        assert_eq!(report.skipped_files, 0);
        assert_eq!(report.hits.len(), 13);
        let pairs: BTreeSet<(String, String)> = report
            .hits
            .iter()
            .map(|hit| (hit.method.to_uppercase(), hit.path.clone()))
            .collect();
        for expected in [
            ("GET", "/api/agents"),
            ("POST", "/api/agents"),
            ("GET", "/api/agents/metrics"),
            ("GET", "/api/agents/available-mcp-servers"),
            ("GET", "/api/agents/available-skills"),
            ("PUT", "/api/agents/{id}"),
            ("DELETE", "/api/agents/{id}"),
            ("GET", "/.well-known/oauth-protected-resource"),
            ("GET", "/.well-known/oauth-protected-resource/mcp"),
            ("GET", "/.well-known/oauth-authorization-server"),
            ("POST", "/oauth/register"),
            ("GET", "/oauth/authorize"),
            ("POST", "/oauth/token"),
        ] {
            assert!(
                pairs.contains(&(expected.0.to_string(), expected.1.to_string())),
                "missing {expected:?}; got {pairs:?}"
            );
        }
        for handler in [
            "list_agents",
            "create_agent",
            "update_agent",
            "delete_agent",
            "protected_resource_metadata",
            "authorization_server_metadata",
            "register",
            "authorize",
            "token",
        ] {
            let hit = report
                .hits
                .iter()
                .find(|hit| hit.handler == handler)
                .unwrap_or_else(|| panic!("missing handler {handler}"));
            assert!(hit.handler_node.is_some(), "unresolved handler {handler}");
            assert_eq!(hit.reason, "axum-router-new");
            assert_eq!(hit.confidence, 0.90);
        }
        let route_nodes = graph
            .graph()
            .node_indices()
            .filter(|idx| graph.node(*idx).kind == RepoGraphNodeKind::Route)
            .count();
        assert_eq!(route_nodes, 13);
        let route_edges = graph
            .graph()
            .edge_references()
            .filter(|edge| edge.weight().kind == RepoGraphEdgeKind::HandlesRoute)
            .count();
        assert_eq!(route_edges, 13);
    }

    #[test]
    fn stamps_health_ping_and_param_only_reasons() {
        let source = r#"
            use axum::{Router, routing::get};
            fn router() -> Router<()> {
                Router::new()
                    .route("/health", get(health))
                    .route("/ping", get(ping))
                    .route("/{id}", get(by_id))
            }
            async fn health() {}
            async fn ping() {}
            async fn by_id() {}
        "#;
        let temp = tempfile::tempdir().expect("create temp fixture dir");
        let root = temp.path();
        let fixture_path = root.join("health_fixture.rs");
        std::fs::write(&fixture_path, source).expect("write temporary health fixture");
        let mut graph = fixture_graph(&[("health_fixture.rs", source)]);

        let report = detect_axum_routes(&mut graph, root);
        let reasons: BTreeMap<String, String> = report
            .hits
            .iter()
            .map(|hit| (hit.path.clone(), hit.reason.clone()))
            .collect();
        assert_eq!(reasons["/health"], "axum-health");
        assert_eq!(reasons["/ping"], "axum-health");
        assert_eq!(reasons["/{id}"], "axum-param-only");
    }

    #[test]
    fn dedupe_is_same_file_only_and_keeps_low_entropy_labels() {
        let source = r#"
            use axum::{Router, routing::get};
            fn router() -> Router<()> {
                Router::new()
                    .route("/api/agents", get(list_agents))
                    .route("/api/agents", get(list_agents))
                    .route("/health", get(health))
                    .route("/health", get(health))
            }
        "#;

        let hits = [
            parse_axum_routes_in_source(source, Path::new("server/src/routes_a.rs")),
            parse_axum_routes_in_source(source, Path::new("server/src/routes_b.rs")),
        ]
        .concat();

        let agent_hits = hits
            .iter()
            .filter(|hit| hit.path == "/api/agents")
            .collect::<Vec<_>>();
        assert_eq!(
            agent_hits.len(),
            2,
            "identical route labels may dedupe within one source_file but must not merge across files",
        );
        assert_ne!(agent_hits[0].file, agent_hits[1].file);

        let health_hits = hits
            .iter()
            .filter(|hit| hit.path == "/health")
            .collect::<Vec<_>>();
        assert_eq!(
            health_hits.len(),
            4,
            "low-entropy route labels are suggestions/noise and must not be merged even within one file",
        );
        assert_ne!(
            route_node_id(
                "GET /api/agents (axum)",
                Path::new("server/src/routes_a.rs"),
                0,
                "/api/agents",
            ),
            route_node_id(
                "GET /api/agents (axum)",
                Path::new("server/src/routes_b.rs"),
                0,
                "/api/agents",
            ),
            "route node ids include source_file",
        );
        assert_ne!(
            route_node_id(
                "GET /health (axum)",
                Path::new("server/src/routes_a.rs"),
                0,
                "/health",
            ),
            route_node_id(
                "GET /health (axum)",
                Path::new("server/src/routes_a.rs"),
                1,
                "/health",
            ),
            "low-entropy route node ids include occurrence ordinal to avoid merging",
        );
    }

    #[test]
    fn route_parity_live_graph_reports_allowlisted_axum_additions() {
        assert!(crate::route_extraction::route_parity_enabled_from_var(
            Some("1")
        ));

        let source = r#"
            use axum::{Router, routing::get};
            fn router() -> Router<()> {
                Router::new()
                    .route("/api/agents", get(list_agents))
                    .route("/api/agents", get(list_agents))
            }
            async fn list_agents() {}
        "#;
        let temp = tempfile::tempdir().expect("create temp fixture dir");
        let root = temp.path();
        std::fs::write(root.join("routes.rs"), source).expect("write temporary route fixture");
        let baseline = fixture_graph(&[("routes.rs", source)]);
        let mut live = baseline.clone();

        let report = detect_axum_routes(&mut live, root);
        let parity =
            crate::route_extraction::assert_route_extraction_graph_parity(&baseline, &live)
                .expect("axum extractor may add only Route nodes and HandlesRoute edges");

        assert_eq!(
            report.hits.len(),
            1,
            "live extractor applies same-file dedup"
        );
        assert!(parity.passed);
        assert_eq!(
            parity.allowed_added_nodes[&RepoGraphNodeKind::Route].count,
            1
        );
        assert_eq!(
            parity.allowed_added_edges[&RepoGraphEdgeKind::HandlesRoute].count,
            1
        );
        assert!(
            !parity
                .allowed_added_edges
                .contains_key(&RepoGraphEdgeKind::Fetches)
        );
        assert!(parity.render_for_ci().contains("allowed added edges"));
    }

    #[test]
    fn unresolved_handlers_get_low_confidence_placeholder_symbols() {
        let source = r#"
            use axum::{Router, routing::get};
            fn router() -> Router<()> {
                Router::new().route("/external", get(external_handlers::show))
            }
        "#;
        let temp = tempfile::tempdir().expect("create temp fixture dir");
        let root = temp.path();
        std::fs::write(root.join("unresolved_fixture.rs"), source)
            .expect("write temporary unresolved fixture");
        let mut graph = fixture_graph(&[("unresolved_fixture.rs", source)]);

        let report = detect_axum_routes(&mut graph, root);

        assert_eq!(report.hits.len(), 1);
        let hit = &report.hits[0];
        assert_eq!(hit.path, "/external");
        assert_eq!(hit.method, "get");
        assert_eq!(hit.handler, "external_handlers::show");
        assert_eq!(hit.reason, "axum-unresolved-handler");
        assert_eq!(hit.confidence, 0.45);
        let handler_node = hit.handler_node.expect("placeholder handler node");
        assert_eq!(graph.node(handler_node).kind, RepoGraphNodeKind::Symbol);
        assert_eq!(
            graph.node(handler_node).display_name,
            "external_handlers::show"
        );
    }

    #[test]
    fn env_gate_defaults_on_and_accepts_false_values() {
        let old = std::env::var(crate::route_extraction::ROUTE_DETECTION_FLAG).ok();

        unsafe {
            std::env::remove_var(crate::route_extraction::ROUTE_DETECTION_FLAG);
        }
        assert!(crate::route_extraction::route_detection_enabled());

        for value in ["0", "false", "no", "off"] {
            unsafe {
                std::env::set_var(crate::route_extraction::ROUTE_DETECTION_FLAG, value);
            }
            assert!(
                !crate::route_extraction::route_detection_enabled(),
                "{value:?} should disable route detection"
            );
        }

        for value in ["1", "true", "yes", "on", "anything-else"] {
            unsafe {
                std::env::set_var(crate::route_extraction::ROUTE_DETECTION_FLAG, value);
            }
            assert!(
                crate::route_extraction::route_detection_enabled(),
                "{value:?} should keep route detection enabled"
            );
        }

        unsafe {
            if let Some(old) = old {
                std::env::set_var(crate::route_extraction::ROUTE_DETECTION_FLAG, old);
            } else {
                std::env::remove_var(crate::route_extraction::ROUTE_DETECTION_FLAG);
            }
        }
    }
}
