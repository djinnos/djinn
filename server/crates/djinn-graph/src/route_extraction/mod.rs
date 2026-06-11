//! Best-effort HTTP route extraction wired into the canonical graph warm path.
//!
//! Framework-specific extractors materialize synthetic `Route` nodes and typed
//! route edges without changing the SCIP-derived symbol/file graph. The pass is
//! intentionally conservative and non-fatal: per-file read/parse failures are
//! reported in [`RouteExtractionReport`] and logged by the caller, while the
//! canonical graph continues to build from the SCIP-derived graph.

pub mod axum;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use petgraph::graph::NodeIndex;
use petgraph::visit::EdgeRef;

pub use axum::{AxumRouteHit, detect_axum_routes};

use crate::repo_graph::{
    RepoDependencyGraph, RepoGraphEdgeKind, RepoGraphNode, RepoGraphNodeKind, RepoNodeKey,
    RouteExclusionConfig, promote_fetches_confidence_with_import_evidence,
};
use crate::scip_parser::ScipSymbolKind;

/// Environment flag that disables route extraction when set to `0` / `false`.
/// Default = on.
pub const ROUTE_DETECTION_FLAG: &str = "DJINN_ROUTE_DETECTION";

/// Environment flag for the route-parity rollout gate. Default = on.
pub const ROUTE_PARITY_FLAG: &str = "DJINN_ROUTE_PARITY";

fn env_flag_enabled(value: Option<&str>) -> bool {
    match value {
        Some(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        None => true,
    }
}

/// Summary emitted by [`detect_routes`] for rollout observability.
#[derive(Debug, Clone, Default, PartialEq)]
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
    /// Inferred consumer matches that were intentionally left as suggestions
    /// rather than hard `Fetches` edges. `reasons` contains stable,
    /// machine-readable strings such as `health-path` and
    /// `below-confidence-floor`.
    pub consumer_edge_suggestions: Vec<RouteConsumerEdgeSuggestion>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RouteConsumerEdgeSuggestion {
    pub consumer_file: PathBuf,
    pub fetch_path: String,
    pub route_path: String,
    pub framework: Option<String>,
    pub confidence: f64,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct RouteExtractionOptions<'a> {
    pub exclusion_config: &'a RouteExclusionConfig,
}

impl<'a> RouteExtractionOptions<'a> {
    pub fn new(exclusion_config: &'a RouteExclusionConfig) -> Self {
        Self { exclusion_config }
    }
}

/// Returns `true` when route extraction should run.
pub fn route_detection_enabled() -> bool {
    env_flag_enabled(std::env::var(ROUTE_DETECTION_FLAG).ok().as_deref())
}

/// Returns `true` when route parity behavior should be active.
pub fn route_parity_enabled() -> bool {
    route_parity_enabled_from_var(std::env::var(ROUTE_PARITY_FLAG).ok().as_deref())
}

/// Pure helper for tests/callers that already resolved the env var.
pub fn route_parity_enabled_from_var(value: Option<&str>) -> bool {
    env_flag_enabled(value)
}

#[cfg(test)]
pub(crate) static ROUTE_DETECTION_ENV_LOCK: std::sync::LazyLock<std::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

/// Run server-side axum extraction first, then TypeScript fetch consumers so
/// consumer edges can resolve against the already-materialized Route nodes.
pub fn detect_routes(
    graph: &mut RepoDependencyGraph,
    project_root: &Path,
) -> RouteExtractionReport {
    let config = graph.route_exclusion_config().clone();
    detect_routes_with_options(graph, project_root, RouteExtractionOptions::new(&config))
}

pub fn detect_routes_with_options(
    graph: &mut RepoDependencyGraph,
    project_root: &Path,
    options: RouteExtractionOptions<'_>,
) -> RouteExtractionReport {
    let mut report = RouteExtractionReport::default();

    preflight_readable_candidate_files(graph, project_root, &mut report);

    let route_nodes_before = count_nodes(graph, RepoGraphNodeKind::Route);
    let handles_edges_before = count_edges(graph, RepoGraphEdgeKind::HandlesRoute);
    let axum_report = axum::detect_axum_routes(graph, project_root);
    let route_nodes_after = count_nodes(graph, RepoGraphNodeKind::Route);
    let handles_edges_after = count_edges(graph, RepoGraphEdgeKind::HandlesRoute);
    report.route_nodes_added = route_nodes_after.saturating_sub(route_nodes_before);
    report.handles_route_edges_added = handles_edges_after.saturating_sub(handles_edges_before);

    let mut routes_by_path = BTreeMap::new();
    for hit in axum_report.hits {
        if let Some(route_node) = hit.route_node {
            routes_by_path
                .entry(hit.path.clone())
                .or_insert_with(|| RouteCandidate::from_graph(graph, hit.path, route_node));
        }
    }
    if routes_by_path.is_empty() {
        collect_existing_route_nodes(graph, &mut routes_by_path);
    }

    detect_typescript_fetches(
        graph,
        project_root,
        &routes_by_path,
        options.exclusion_config,
        &mut report,
    );
    report
}

#[derive(Debug, Clone)]
struct RouteCandidate {
    path: String,
    node: NodeIndex,
    framework: Option<String>,
}

impl RouteCandidate {
    fn from_graph(graph: &RepoDependencyGraph, path: String, node: NodeIndex) -> Self {
        Self {
            path,
            node,
            framework: graph.node(node).route_framework.clone(),
        }
    }
}

fn preflight_readable_candidate_files(
    graph: &RepoDependencyGraph,
    project_root: &Path,
    report: &mut RouteExtractionReport,
) {
    for (rel_path, _file_node) in file_nodes(graph, |lang, path| {
        is_rust_file(lang, path) || is_typescript_fetch_candidate(lang, path)
    }) {
        if let Err(error) = std::fs::read_to_string(project_root.join(&rel_path)) {
            record_file_failure(report, rel_path, error.to_string());
        }
    }
}

fn detect_typescript_fetches(
    graph: &mut RepoDependencyGraph,
    project_root: &Path,
    routes_by_path: &BTreeMap<String, RouteCandidate>,
    exclusion_config: &RouteExclusionConfig,
    report: &mut RouteExtractionReport,
) {
    if routes_by_path.is_empty() {
        return;
    }

    for (rel_path, file_node) in file_nodes(graph, is_typescript_fetch_candidate) {
        let source = match std::fs::read_to_string(project_root.join(&rel_path)) {
            Ok(source) => source,
            Err(error) => {
                record_file_failure(report, rel_path, error.to_string());
                continue;
            }
        };
        if !source.contains("fetch(") {
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

fn resolve_fetch_route<'a>(
    fetch_path: &str,
    routes_by_path: &'a BTreeMap<String, RouteCandidate>,
) -> Option<&'a RouteCandidate> {
    routes_by_path
        .iter()
        .find(|(route_path, _)| {
            fetch_path == route_path.as_str()
                || fetch_path.starts_with(route_path.as_str())
                || route_path.starts_with(fetch_path)
        })
        .map(|(_, route)| route)
}

fn collect_existing_route_nodes(
    graph: &RepoDependencyGraph,
    routes_by_path: &mut BTreeMap<String, RouteCandidate>,
) {
    for idx in graph.graph().node_indices() {
        let node = graph.node(idx);
        if node.kind != RepoGraphNodeKind::Route {
            continue;
        }
        if let Some(path) = route_path_from_display_name(&node.display_name) {
            routes_by_path
                .entry(path.clone())
                .or_insert_with(|| RouteCandidate::from_graph(graph, path, idx));
        }
    }
}

fn consumer_edge_exclusion_reasons(
    confidence: f64,
    has_import_evidence: bool,
    consumer_language: Option<&str>,
    route_language: Option<&str>,
    route: &RouteCandidate,
    config: &RouteExclusionConfig,
) -> Vec<String> {
    let mut reasons = Vec::new();
    if health_path_matches(&route.path, &config.health_path_globs) {
        reasons.push("health-path".to_string());
    }
    if config.param_only_paths && is_param_only_path(&route.path) {
        reasons.push("param-only-path".to_string());
    }
    if confidence + f64::EPSILON < config.min_confidence_for_consumer_edge {
        reasons.push("below-confidence-floor".to_string());
    }
    if !has_import_evidence
        && consumer_language.is_some()
        && route_language.is_some()
        && consumer_language == route_language
    {
        reasons.push("same-language-inferred-collision".to_string());
    }
    if let Some(framework) = route.framework.as_deref()
        && config
            .excluded_frameworks
            .iter()
            .any(|excluded| excluded.eq_ignore_ascii_case(framework))
    {
        reasons.push("excluded-framework".to_string());
    }
    reasons
}

fn consumer_has_route_import_evidence(
    graph: &RepoDependencyGraph,
    consumer_file: NodeIndex,
    route: &RouteCandidate,
) -> bool {
    let route_node = graph.node(route.node);
    let route_handler_symbol = route_node.route_handler_symbol.as_deref();
    graph
        .graph()
        .edges(consumer_file)
        .filter(|edge| {
            matches!(
                edge.weight().kind,
                RepoGraphEdgeKind::FileReference | RepoGraphEdgeKind::SymbolReference
            )
        })
        .any(|edge| {
            let target = graph.node(edge.target());
            let references_handler = route_handler_symbol
                .zip(target.symbol.as_deref())
                .is_some_and(|(handler, target_symbol)| handler == target_symbol);
            references_handler || is_server_route_context_node(target)
        })
}

fn is_server_route_context_node(node: &RepoGraphNode) -> bool {
    node.language.as_deref() == Some("rust")
        && node.file_path.as_ref().is_some_and(|path| {
            let path = path.to_string_lossy();
            path.starts_with("server/") || path.contains("/server/")
        })
}

fn health_path_matches(path: &str, globs: &[String]) -> bool {
    let path = path.to_ascii_lowercase();
    globs
        .iter()
        .any(|glob| glob_match(&path, &glob.to_ascii_lowercase()))
}

fn glob_match(value: &str, glob: &str) -> bool {
    if glob == "*" {
        return true;
    }
    let Some((prefix, suffix)) = glob.split_once('*') else {
        return value == glob || value.ends_with(glob);
    };
    value.starts_with(prefix) && value.ends_with(suffix)
}

fn is_param_only_path(path: &str) -> bool {
    let trimmed = path.trim_matches('/');
    !trimmed.is_empty()
        && trimmed.split('/').all(|segment| {
            segment.starts_with(':') || (segment.starts_with('{') && segment.ends_with('}'))
        })
}

fn route_path_from_display_name(display_name: &str) -> Option<String> {
    let (_, rest) = display_name.split_once(' ')?;
    let path = rest.rsplit_once(" (").map_or(rest, |(path, _)| path);
    (!path.is_empty()).then(|| path.to_string())
}

fn count_nodes(graph: &RepoDependencyGraph, kind: RepoGraphNodeKind) -> usize {
    graph
        .graph()
        .node_weights()
        .filter(|node| node.kind == kind)
        .count()
}

fn count_edges(graph: &RepoDependencyGraph, kind: RepoGraphEdgeKind) -> usize {
    graph
        .graph()
        .edge_weights()
        .filter(|edge| edge.kind == kind)
        .count()
}

fn record_file_failure(report: &mut RouteExtractionReport, rel_path: PathBuf, message: String) {
    if !report.skipped_files.contains(&rel_path) {
        report.skipped_files.push(rel_path.clone());
    }
    if let Some((_, messages)) = report
        .file_failures
        .iter_mut()
        .find(|(path, _)| path == &rel_path)
    {
        if !messages.contains(&message) {
            messages.push(message);
        }
    } else {
        report.file_failures.push((rel_path, vec![message]));
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
            let path = node.file_path.clone().or_else(|| match &node.id {
                RepoNodeKey::File(path) => Some(path.clone()),
                _ => None,
            })?;
            if include(node.language.as_deref(), &path) {
                Some((path, idx))
            } else {
                None
            }
        })
        .collect()
}

fn is_rust_file(lang: Option<&str>, path: &Path) -> bool {
    lang == Some("rust") || path.extension().is_some_and(|ext| ext == "rs")
}

fn is_typescript_fetch_candidate(lang: Option<&str>, path: &Path) -> bool {
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

fn is_enclosing_fetch_consumer_symbol(node: &RepoGraphNode) -> bool {
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

fn byte_to_line(source: &str, byte_offset: usize) -> u32 {
    1 + source[..source.len().min(byte_offset)]
        .bytes()
        .filter(|b| *b == b'\n')
        .count() as u32
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::repo_graph::{
        EdgeConfidenceTier, RepoGraphArtifact, RepoGraphArtifactEdge, RepoGraphArtifactSymbolRange,
        RepoGraphEdge, RepoGraphNode, edge_confidence_floor, edge_weight_for,
    };
    use crate::scip_parser::ScipSymbolKind;

    fn fixture_node(
        id: RepoNodeKey,
        kind: RepoGraphNodeKind,
        display_name: &str,
        language: Option<&str>,
        file_path: Option<&str>,
        symbol: Option<&str>,
        is_external: bool,
    ) -> RepoGraphNode {
        RepoGraphNode {
            id,
            kind,
            display_name: display_name.to_string(),
            language: language.map(str::to_string),
            file_path: file_path.map(PathBuf::from),
            symbol: symbol.map(str::to_string),
            symbol_kind: (kind == RepoGraphNodeKind::Symbol).then_some(ScipSymbolKind::Function),
            is_external,
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

    fn fixture_edge(
        source: usize,
        target: usize,
        kind: RepoGraphEdgeKind,
    ) -> RepoGraphArtifactEdge {
        RepoGraphArtifactEdge {
            source,
            target,
            kind,
            weight: edge_weight_for(kind),
            evidence_count: 1,
            confidence: edge_confidence_floor(kind),
            reason: None,
            step: None,
        }
    }

    fn fixture_graph() -> RepoDependencyGraph {
        let nodes = vec![
            fixture_node(
                RepoNodeKey::File(PathBuf::from("server/src/routes.rs")),
                RepoGraphNodeKind::File,
                "server/src/routes.rs",
                Some("rust"),
                Some("server/src/routes.rs"),
                None,
                false,
            ),
            fixture_node(
                RepoNodeKey::Symbol("axum::Router".to_string()),
                RepoGraphNodeKind::Symbol,
                "Router",
                Some("rust"),
                None,
                Some("axum::Router"),
                true,
            ),
            fixture_node(
                RepoNodeKey::Symbol("test server/src/routes.rs/list_agents().".to_string()),
                RepoGraphNodeKind::Symbol,
                "list_agents",
                Some("rust"),
                Some("server/src/routes.rs"),
                Some("test server/src/routes.rs/list_agents()."),
                false,
            ),
            fixture_node(
                RepoNodeKey::File(PathBuf::from("ui/src/api/agents.ts")),
                RepoGraphNodeKind::File,
                "ui/src/api/agents.ts",
                Some("typescript"),
                Some("ui/src/api/agents.ts"),
                None,
                false,
            ),
            fixture_node(
                RepoNodeKey::Symbol("ts ui/src/api/agents.ts fetchAgents().".to_string()),
                RepoGraphNodeKind::Symbol,
                "fetchAgents",
                Some("typescript"),
                Some("ui/src/api/agents.ts"),
                Some("ts ui/src/api/agents.ts fetchAgents()."),
                false,
            ),
            fixture_node(
                RepoNodeKey::File(PathBuf::from("server/src/missing.rs")),
                RepoGraphNodeKind::File,
                "server/src/missing.rs",
                Some("rust"),
                Some("server/src/missing.rs"),
                None,
                false,
            ),
        ];
        RepoDependencyGraph::from_artifact(&RepoGraphArtifact {
            version: crate::repo_graph::REPO_GRAPH_ARTIFACT_VERSION,
            nodes,
            edges: vec![fixture_edge(0, 1, RepoGraphEdgeKind::FileReference)],
            symbol_ranges: BTreeMap::from([(
                PathBuf::from("ui/src/api/agents.ts"),
                vec![RepoGraphArtifactSymbolRange {
                    start_line: 1,
                    end_line: 3,
                    node: 4,
                }],
            )]),
            communities: Vec::new(),
            processes: Vec::new(),
            route_exclusion_config: Default::default(),
        })
    }

    #[test]
    fn route_detection_env_defaults_on_and_can_be_disabled() {
        let _guard = ROUTE_DETECTION_ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var(ROUTE_DETECTION_FLAG) };
        assert!(route_detection_enabled());
        unsafe { std::env::set_var(ROUTE_DETECTION_FLAG, "0") };
        assert!(!route_detection_enabled());
        unsafe { std::env::set_var(ROUTE_DETECTION_FLAG, "true") };
        assert!(route_detection_enabled());
        unsafe { std::env::remove_var(ROUTE_DETECTION_FLAG) };
    }

    #[test]
    fn route_parity_defaults_enabled() {
        assert!(route_parity_enabled_from_var(None));
    }

    #[test]
    fn route_parity_accepts_on_values() {
        for value in ["1", "true", "yes", "on", "anything"] {
            assert!(route_parity_enabled_from_var(Some(value)));
        }
    }

    #[test]
    fn route_parity_accepts_off_values() {
        for value in ["0", "false", "no", "off", " OFF "] {
            assert!(!route_parity_enabled_from_var(Some(value)));
        }
    }

    #[test]
    fn scans_typescript_fetch_shapes() {
        let fetches =
            scan_fetches("fetch(`${getServerBaseUrl()}/api/agents`, {})\nfetch('/api/missing')");
        assert_eq!(
            fetches.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
            vec!["/api/agents", "/api/missing"]
        );
    }

    #[test]
    fn detect_routes_runs_axum_then_typescript_and_skips_broken_files() {
        let temp = tempfile::tempdir().expect("create temp fixture dir");
        let root = temp.path();
        std::fs::create_dir_all(root.join("server/src")).unwrap();
        std::fs::create_dir_all(root.join("ui/src/api")).unwrap();
        std::fs::write(
            root.join("server/src/routes.rs"),
            "use axum::{Router, routing::get};\nfn router() -> Router<()> { Router::new().route(\"/api/agents\", get(list_agents)) }\nasync fn list_agents() {}",
        )
        .unwrap();
        std::fs::write(
            root.join("ui/src/api/agents.ts"),
            "export function fetchAgents() {\n  return fetch(`${getServerBaseUrl()}/api/agents`, {});\n}",
        )
        .unwrap();

        let mut graph = fixture_graph();
        let report = detect_routes(&mut graph, root);

        assert_eq!(report.route_nodes_added, 1);
        assert_eq!(report.handles_route_edges_added, 1);
        assert_eq!(report.fetches_edges_added, 1);
        assert_eq!(report.unmatched_fetch_count, 0);
        assert_eq!(report.unresolved_consumer_count, 0);
        assert_eq!(
            report.skipped_files,
            vec![PathBuf::from("server/src/missing.rs")]
        );
        assert_eq!(report.file_failures.len(), 1);
        assert!(
            graph
                .graph()
                .node_weights()
                .any(|node| node.kind == RepoGraphNodeKind::Route)
        );
    }

    #[test]
    fn route_parity_enabled_live_counts_do_not_exceed_disabled_shadow_counts() {
        let _guard = ROUTE_DETECTION_ENV_LOCK.lock().unwrap();
        let old = std::env::var(ROUTE_PARITY_FLAG).ok();

        let (disabled_nodes, disabled_edges) = route_parity_fixture_counts(Some("0"));
        let (enabled_nodes, enabled_edges) = route_parity_fixture_counts(Some("1"));

        unsafe {
            if let Some(old) = old {
                std::env::set_var(ROUTE_PARITY_FLAG, old);
            } else {
                std::env::remove_var(ROUTE_PARITY_FLAG);
            }
        }

        assert!(
            enabled_nodes <= disabled_nodes,
            "route parity live node count ({enabled_nodes}) must not exceed shadow baseline ({disabled_nodes})"
        );
        assert!(
            enabled_edges <= disabled_edges,
            "route parity live edge count ({enabled_edges}) must not exceed shadow baseline ({disabled_edges})"
        );
    }

    fn route_parity_fixture_counts(value: Option<&str>) -> (usize, usize) {
        unsafe {
            if let Some(value) = value {
                std::env::set_var(ROUTE_PARITY_FLAG, value);
            } else {
                std::env::remove_var(ROUTE_PARITY_FLAG);
            }
        }

        let temp = tempfile::tempdir().expect("create temp fixture dir");
        let root = temp.path();
        std::fs::create_dir_all(root.join("server/src")).unwrap();
        std::fs::create_dir_all(root.join("ui/src/api")).unwrap();
        std::fs::write(
            root.join("server/src/routes.rs"),
            "use axum::{Router, routing::get};\nfn router() -> Router<()> { Router::new().route(\"/api/agents\", get(list_agents)) }\nasync fn list_agents() {}",
        )
        .unwrap();
        std::fs::write(
            root.join("ui/src/api/agents.ts"),
            "export function fetchAgents() {\n  return fetch('/api/agents', {});\n}",
        )
        .unwrap();

        let mut graph = fixture_graph();
        let _report = detect_routes(&mut graph, root);
        (graph.node_count(), graph.edge_count())
    }

    #[test]
    fn default_exclusion_config_suggests_health_and_param_only_fetches() {
        let temp = tempfile::tempdir().expect("create temp fixture dir");
        let root = temp.path();
        std::fs::create_dir_all(root.join("server/src")).unwrap();
        std::fs::create_dir_all(root.join("ui/src/api")).unwrap();
        std::fs::write(
            root.join("server/src/routes.rs"),
            "use axum::{Router, routing::get};\nfn router() -> Router<()> { Router::new().route(\"/health\", get(health)).route(\"/:id\", get(show)) }\nasync fn health() {}\nasync fn show() {}",
        )
        .unwrap();
        std::fs::write(
            root.join("ui/src/api/agents.ts"),
            "export function fetchAgents() {\n  fetch('/health');\n  fetch('/:id');\n}",
        )
        .unwrap();

        let mut graph = fixture_graph();
        let report = detect_routes(&mut graph, root);

        assert_eq!(report.fetches_edges_added, 0);
        let reasons = report
            .consumer_edge_suggestions
            .iter()
            .map(|suggestion| suggestion.reasons.as_slice())
            .collect::<Vec<_>>();
        assert!(reasons.iter().any(|r| *r == ["health-path"]));
        assert!(reasons.iter().any(|r| *r == ["param-only-path"]));
    }

    #[test]
    fn import_evidence_promotes_fetches_to_extracted_confidence() {
        let temp = tempfile::tempdir().expect("create temp fixture dir");
        let root = temp.path();
        std::fs::create_dir_all(root.join("server/src")).unwrap();
        std::fs::create_dir_all(root.join("ui/src/api")).unwrap();
        std::fs::write(
            root.join("server/src/routes.rs"),
            "use axum::{Router, routing::get};\nfn router() -> Router<()> { Router::new().route(\"/api/agents\", get(list_agents)) }\nasync fn list_agents() {}",
        )
        .unwrap();
        std::fs::write(
            root.join("ui/src/api/agents.ts"),
            "export function fetchAgents() {\n  return fetch('/api/agents');\n}",
        )
        .unwrap();

        let mut graph = fixture_graph();
        graph.graph_mut_unchecked().add_edge(
            NodeIndex::new(3),
            NodeIndex::new(2),
            RepoGraphEdge {
                kind: RepoGraphEdgeKind::FileReference,
                weight: edge_weight_for(RepoGraphEdgeKind::FileReference),
                evidence_count: 1,
                confidence: 0.95,
                reason: Some("test-import".to_string()),
                step: None,
            },
        );
        let report = detect_routes(&mut graph, root);

        assert_eq!(report.fetches_edges_added, 1);
        let fetches = graph
            .graph()
            .edge_weights()
            .find(|edge| edge.kind == RepoGraphEdgeKind::Fetches)
            .expect("fetches edge");
        assert!(fetches.confidence > 0.9);
        assert_eq!(fetches.confidence_tier(), EdgeConfidenceTier::Extracted);
        assert!(
            fetches
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("import-evidence"))
        );
    }

    #[test]
    fn same_language_inferred_fetches_are_suggestions_without_import_evidence() {
        let temp = tempfile::tempdir().expect("create temp fixture dir");
        let root = temp.path();
        std::fs::create_dir_all(root.join("ui/src/api")).unwrap();
        std::fs::write(
            root.join("ui/src/api/agents.ts"),
            "export function fetchAgents() {\n  return fetch('/api/agents');\n}",
        )
        .unwrap();

        let mut graph = fixture_graph();
        graph.ensure_route_node(
            "GET /api/agents (nextjs)",
            "GET /api/agents (nextjs)",
            Some("typescript"),
            Some("root"),
            Some("nextjs"),
            None,
        );
        let report = detect_routes(&mut graph, root);

        assert_eq!(report.fetches_edges_added, 0);
        assert!(report.consumer_edge_suggestions.iter().any(|suggestion| {
            suggestion
                .reasons
                .iter()
                .any(|reason| reason == "same-language-inferred-collision")
        }));
    }

    #[test]
    fn fetches_consumer_resolution_rejects_non_function_symbols() {
        let mut graph = fixture_graph();
        let consumer = graph.node(NodeIndex::new(4)).clone();
        assert!(is_enclosing_fetch_consumer_symbol(&consumer));

        graph.graph_mut_unchecked()[NodeIndex::new(4)].kind = RepoGraphNodeKind::Table;
        assert!(!is_enclosing_fetch_consumer_symbol(
            graph.node(NodeIndex::new(4))
        ));

        graph.graph_mut_unchecked()[NodeIndex::new(4)].kind = RepoGraphNodeKind::Symbol;
        graph.graph_mut_unchecked()[NodeIndex::new(4)].symbol_kind = Some(ScipSymbolKind::Property);
        assert!(!is_enclosing_fetch_consumer_symbol(
            graph.node(NodeIndex::new(4))
        ));
    }

    #[test]
    fn exclusion_options_record_below_floor_and_framework_reasons() {
        let fetch = FetchHit {
            path: "/api/agents".to_string(),
            byte_offset: 0,
            confidence: 0.70,
            reason: "ts-fetch-literal",
        };
        let route = RouteCandidate {
            path: "/api/agents".to_string(),
            node: NodeIndex::new(0),
            framework: Some("axum".to_string()),
        };
        let config = RouteExclusionConfig {
            min_confidence_for_consumer_edge: 0.75,
            excluded_frameworks: vec!["Axum".to_string()],
            ..RouteExclusionConfig::default()
        };

        assert_eq!(
            consumer_edge_exclusion_reasons(
                fetch.confidence,
                false,
                Some("typescript"),
                Some("rust"),
                &route,
                &config,
            ),
            vec!["below-confidence-floor", "excluded-framework"]
        );
    }
}
