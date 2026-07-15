//! Best-effort HTTP route extraction wired into the canonical graph warm path.
//!
//! Framework-specific extractors materialize synthetic `Route` nodes and typed
//! route edges without changing the SCIP-derived symbol/file graph. The pass is
//! intentionally conservative and non-fatal: per-file read/parse failures are
//! reported in [`RouteExtractionReport`] and logged by the caller, while the
//! canonical graph continues to build from the SCIP-derived graph.

pub mod axum;
pub mod typescript_fetch;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use petgraph::graph::NodeIndex;
use petgraph::visit::EdgeRef;

pub use axum::{AxumRouteHit, detect_axum_routes};
pub use typescript_fetch::FetchHit;

use crate::repo_graph::{
    RepoDependencyGraph, RepoGraphEdgeKind, RepoGraphNode, RepoGraphNodeKind, RepoNodeKey,
    RouteExclusionConfig,
};
use crate::ykcg_parity::{
    YkcgExtractorParityConfig, YkcgExtractorParityError, YkcgExtractorParityReport,
    assert_ykcg_extractor_graph_parity,
};

/// Temporary rollout flag for route extraction.
///
/// This disables the extractor when set to `0` / `false` / `no` / `off`
/// (case-insensitive). Default = on. Keep this seam rollout-only: it is not a
/// second permanent graph pipeline and should be deleted once Route extraction
/// ships broadly.
pub const ROUTE_DETECTION_FLAG: &str = "DJINN_ROUTE_DETECTION";

/// Temporary rollout flag for the route graph-parity gate. Default = on.
///
/// The gate compares the pre-extractor graph to the route-enabled graph through
/// the reusable ykcg parity adapter, allowing only Route nodes plus
/// HandlesRoute/Fetches edges as intentional extractor output. Keep this seam
/// rollout-only and remove it with [`ROUTE_DETECTION_FLAG`] after rollout.
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
    route_detection_enabled_from_var(std::env::var(ROUTE_DETECTION_FLAG).ok().as_deref())
}

/// Pure helper for tests/callers that already resolved the env var.
pub fn route_detection_enabled_from_var(value: Option<&str>) -> bool {
    env_flag_enabled(value)
}

/// Returns `true` when route parity behavior should be active.
pub fn route_parity_enabled() -> bool {
    route_parity_enabled_from_var(std::env::var(ROUTE_PARITY_FLAG).ok().as_deref())
}

/// Pure helper for tests/callers that already resolved the env var.
pub fn route_parity_enabled_from_var(value: Option<&str>) -> bool {
    env_flag_enabled(value)
}

/// ykcg parity config for the Route extractor rollout gate.
pub fn route_extraction_parity_config() -> YkcgExtractorParityConfig {
    YkcgExtractorParityConfig::new(
        "route-extraction",
        [RepoGraphNodeKind::Route],
        [RepoGraphEdgeKind::HandlesRoute, RepoGraphEdgeKind::Fetches],
    )
}

/// Assert route-extractor parity between a disabled baseline and enabled graph.
///
/// The baseline must be the canonical graph before route extraction runs (the
/// `DJINN_ROUTE_DETECTION=0` shape); `live` must be the graph after the enabled
/// extractor pass. This delegates to the ykcg adapter so core file/symbol/table
/// process/tool populations remain strict while Route/HandlesRoute/Fetches
/// additions are reported as allowed rollout output.
pub fn assert_route_extraction_graph_parity(
    baseline: &RepoDependencyGraph,
    live: &RepoDependencyGraph,
) -> Result<YkcgExtractorParityReport, YkcgExtractorParityError> {
    assert_ykcg_extractor_graph_parity(baseline, live, &route_extraction_parity_config())
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

    typescript_fetch::detect_typescript_fetches(
        graph,
        project_root,
        &routes_by_path,
        options.exclusion_config,
        &mut report,
    );
    report
}

#[derive(Debug, Clone)]
pub(super) struct RouteCandidate {
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
        is_rust_file(lang, path) || typescript_fetch::is_typescript_fetch_candidate(lang, path)
    }) {
        if let Err(error) = std::fs::read_to_string(project_root.join(&rel_path)) {
            record_file_failure(report, rel_path, error.to_string());
        }
    }
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

pub(super) fn consumer_edge_exclusion_reasons(
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

pub(super) fn consumer_has_route_import_evidence(
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

pub(super) fn record_file_failure(
    report: &mut RouteExtractionReport,
    rel_path: PathBuf,
    message: String,
) {
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

pub(super) fn file_nodes<F>(
    graph: &RepoDependencyGraph,
    mut include: F,
) -> Vec<(PathBuf, NodeIndex)>
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

#[cfg(test)]
mod tests {
    mod e2e;

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
            layout_positions: BTreeMap::new(),
            galaxy_positions: BTreeMap::new(),
            galaxy_degrees: BTreeMap::new(),
        })
    }

    fn route_fixture_graph(include_axum: bool, include_ts: bool) -> RepoDependencyGraph {
        route_fixture_graph_with_ts_path(
            include_axum,
            include_ts,
            Path::new("ui/src/api/agents.ts"),
        )
    }

    fn route_fixture_graph_with_ts_path(
        include_axum: bool,
        include_ts: bool,
        ts_path: &Path,
    ) -> RepoDependencyGraph {
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        let mut symbol_ranges = BTreeMap::new();

        if include_axum {
            let server_file = nodes.len();
            nodes.push(fixture_node(
                RepoNodeKey::File(PathBuf::from("server/src/routes.rs")),
                RepoGraphNodeKind::File,
                "server/src/routes.rs",
                Some("rust"),
                Some("server/src/routes.rs"),
                None,
                false,
            ));
            let router = nodes.len();
            nodes.push(fixture_node(
                RepoNodeKey::Symbol("axum::Router".to_string()),
                RepoGraphNodeKind::Symbol,
                "Router",
                Some("rust"),
                None,
                Some("axum::Router"),
                true,
            ));
            nodes.push(fixture_node(
                RepoNodeKey::Symbol("test server/src/routes.rs/list_agents().".to_string()),
                RepoGraphNodeKind::Symbol,
                "list_agents",
                Some("rust"),
                Some("server/src/routes.rs"),
                Some("test server/src/routes.rs/list_agents()."),
                false,
            ));
            edges.push(fixture_edge(
                server_file,
                router,
                RepoGraphEdgeKind::FileReference,
            ));
        }

        if include_ts {
            let ts_path_string = ts_path.to_string_lossy().to_string();
            let ts_symbol_key = format!("ts {ts_path_string} fetchAgents().");
            nodes.push(fixture_node(
                RepoNodeKey::File(PathBuf::from(&ts_path_string)),
                RepoGraphNodeKind::File,
                &ts_path_string,
                Some("typescript"),
                Some(&ts_path_string),
                None,
                false,
            ));
            let ts_symbol = nodes.len();
            nodes.push(fixture_node(
                RepoNodeKey::Symbol(ts_symbol_key.clone()),
                RepoGraphNodeKind::Symbol,
                "fetchAgents",
                Some("typescript"),
                Some(&ts_path_string),
                Some(&ts_symbol_key),
                false,
            ));
            symbol_ranges.insert(
                PathBuf::from(&ts_path_string),
                vec![RepoGraphArtifactSymbolRange {
                    start_line: 1,
                    end_line: 3,
                    node: ts_symbol,
                }],
            );
        }

        RepoDependencyGraph::from_artifact(&RepoGraphArtifact {
            version: crate::repo_graph::REPO_GRAPH_ARTIFACT_VERSION,
            nodes,
            edges,
            symbol_ranges,
            communities: Vec::new(),
            processes: Vec::new(),
            route_exclusion_config: Default::default(),
            layout_positions: BTreeMap::new(),
            galaxy_positions: BTreeMap::new(),
            galaxy_degrees: BTreeMap::new(),
        })
    }

    fn write_route_fixture(root: &Path, axum_source: Option<&str>, ts_source: Option<&str>) {
        if let Some(source) = axum_source {
            std::fs::create_dir_all(root.join("server/src")).unwrap();
            std::fs::write(root.join("server/src/routes.rs"), source).unwrap();
        }
        if let Some(source) = ts_source {
            std::fs::create_dir_all(root.join("ui/src/api")).unwrap();
            std::fs::write(root.join("ui/src/api/agents.ts"), source).unwrap();
        }
    }

    fn route_extraction_counts(
        graph: &RepoDependencyGraph,
    ) -> (usize, Vec<RepoGraphEdge>, Vec<RepoGraphEdge>) {
        let route_nodes = graph
            .graph()
            .node_weights()
            .filter(|node| node.kind == RepoGraphNodeKind::Route)
            .count();
        let handles = graph
            .graph()
            .edge_weights()
            .filter(|edge| edge.kind == RepoGraphEdgeKind::HandlesRoute)
            .cloned()
            .collect();
        let fetches = graph
            .graph()
            .edge_weights()
            .filter(|edge| edge.kind == RepoGraphEdgeKind::Fetches)
            .cloned()
            .collect();
        (route_nodes, handles, fetches)
    }

    #[test]
    fn route_detection_env_defaults_on_and_can_be_disabled() {
        let _guard = ROUTE_DETECTION_ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var(ROUTE_DETECTION_FLAG) };
        assert!(route_detection_enabled());
        for value in ["0", "false", "no", "off", " OFF "] {
            unsafe { std::env::set_var(ROUTE_DETECTION_FLAG, value) };
            assert!(
                !route_detection_enabled(),
                "{ROUTE_DETECTION_FLAG}={value:?} must disable route extraction"
            );
        }
        unsafe { std::env::set_var(ROUTE_DETECTION_FLAG, "true") };
        assert!(route_detection_enabled());
        unsafe { std::env::remove_var(ROUTE_DETECTION_FLAG) };
    }

    #[test]
    fn route_detection_pure_env_helper_matches_process_gate_shape() {
        assert!(route_detection_enabled_from_var(None));
        for value in ["1", "true", "yes", "on", "anything"] {
            assert!(route_detection_enabled_from_var(Some(value)));
        }
        for value in ["0", "false", "no", "off", " OFF "] {
            assert!(!route_detection_enabled_from_var(Some(value)));
        }
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
    fn matched_axum_and_typescript_fixture_emits_single_route_and_edges() {
        let temp = tempfile::tempdir().expect("create temp fixture dir");
        let root = temp.path();
        write_route_fixture(
            root,
            Some(
                "use axum::{Router, routing::get};\nfn router() -> Router<()> { Router::new().route(\"/api/agents\", get(list_agents)) }\nasync fn list_agents() {}",
            ),
            Some("export function fetchAgents() {\n  return fetch('/api/agents');\n}"),
        );

        let mut graph = route_fixture_graph(true, true);
        let report = detect_routes(&mut graph, root);
        let (route_nodes, handles, fetches) = route_extraction_counts(&graph);

        assert_eq!(report.route_nodes_added, 1);
        assert_eq!(report.handles_route_edges_added, 1);
        assert_eq!(report.fetches_edges_added, 1);
        assert_eq!(report.unmatched_fetch_count, 0);
        assert_eq!(route_nodes, 1);
        assert_eq!(handles.len(), 1);
        assert_eq!(fetches.len(), 1);
        assert_eq!(handles[0].confidence, 0.90);
        assert_eq!(handles[0].reason.as_deref(), Some("axum-router-new"));
        assert_eq!(fetches[0].confidence, 0.70);
        assert_eq!(fetches[0].reason.as_deref(), Some("ts-fetch-literal"));
    }

    #[test]
    fn axum_only_fixture_emits_route_and_handler_without_fetches() {
        let temp = tempfile::tempdir().expect("create temp fixture dir");
        let root = temp.path();
        write_route_fixture(
            root,
            Some(
                "use axum::{Router, routing::get};\nfn router() -> Router<()> { Router::new().route(\"/api/agents\", get(list_agents)) }\nasync fn list_agents() {}",
            ),
            None,
        );

        let mut graph = route_fixture_graph(true, false);
        let report = detect_routes(&mut graph, root);
        let (route_nodes, handles, fetches) = route_extraction_counts(&graph);

        assert_eq!(report.route_nodes_added, 1);
        assert_eq!(report.handles_route_edges_added, 1);
        assert_eq!(report.fetches_edges_added, 0);
        assert_eq!(report.unmatched_fetch_count, 0);
        assert_eq!(route_nodes, 1);
        assert_eq!(handles.len(), 1);
        assert!(fetches.is_empty());
    }

    #[test]
    fn typescript_only_unknown_fixture_counts_unmatched_without_graph_pollution() {
        let temp = tempfile::tempdir().expect("create temp fixture dir");
        let root = temp.path();
        write_route_fixture(
            root,
            None,
            Some("export function fetchAgents() {\n  return fetch('/api/unknown');\n}"),
        );

        let mut graph = route_fixture_graph(false, true);
        let report = detect_routes(&mut graph, root);
        let (route_nodes, handles, fetches) = route_extraction_counts(&graph);

        assert_eq!(report.route_nodes_added, 0);
        assert_eq!(report.handles_route_edges_added, 0);
        assert_eq!(report.fetches_edges_added, 0);
        assert_eq!(report.unmatched_fetch_count, 1);
        assert_eq!(route_nodes, 0);
        assert!(handles.is_empty());
        assert!(fetches.is_empty());
    }

    #[test]
    fn empty_no_candidate_fixture_leaves_symbol_file_graph_unchanged() {
        let temp = tempfile::tempdir().expect("create temp fixture dir");
        let root = temp.path();
        std::fs::create_dir_all(root.join("server/src")).unwrap();
        std::fs::create_dir_all(root.join("ui/src/api")).unwrap();
        std::fs::write(root.join("server/src/routes.rs"), "fn helper() {}").unwrap();
        std::fs::write(
            root.join("ui/src/api/agents.ts"),
            "export const value = 1;\n",
        )
        .unwrap();

        let mut graph = route_fixture_graph(false, true);
        graph.graph_mut_unchecked().add_node(fixture_node(
            RepoNodeKey::File(PathBuf::from("server/src/routes.rs")),
            RepoGraphNodeKind::File,
            "server/src/routes.rs",
            Some("rust"),
            Some("server/src/routes.rs"),
            None,
            false,
        ));
        let before = graph.to_artifact();

        let report = detect_routes(&mut graph, root);
        let after = graph.to_artifact();

        assert_eq!(report, RouteExtractionReport::default());
        assert_eq!(after.nodes, before.nodes);
        assert_eq!(after.edges, before.edges);
        assert_eq!(after.symbol_ranges, before.symbol_ranges);
    }

    #[test]
    fn route_parity_enabled_reports_route_additions_and_core_counts() {
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

        let baseline = fixture_graph();
        let mut live = baseline.clone();
        let extraction = detect_routes(&mut live, root);
        let parity = assert_route_extraction_graph_parity(&baseline, &live)
            .expect("Route/HandlesRoute/Fetches additions should be allowlisted");

        assert_eq!(extraction.route_nodes_added, 1);
        assert_eq!(extraction.handles_route_edges_added, 1);
        assert_eq!(extraction.fetches_edges_added, 1);
        assert!(parity.passed);
        assert_eq!(
            parity.node_counts_by_kind.baseline[&RepoGraphNodeKind::File],
            parity.node_counts_by_kind.live[&RepoGraphNodeKind::File]
        );
        assert_eq!(
            parity.node_counts_by_kind.baseline[&RepoGraphNodeKind::Symbol],
            parity.node_counts_by_kind.live[&RepoGraphNodeKind::Symbol]
        );
        assert_eq!(
            parity.allowed_added_nodes[&RepoGraphNodeKind::Route].count,
            1
        );
        assert_eq!(
            parity.allowed_added_edges[&RepoGraphEdgeKind::HandlesRoute].count,
            1
        );
        assert_eq!(
            parity.allowed_added_edges[&RepoGraphEdgeKind::Fetches].count,
            1
        );
        let rendered = parity.render_for_ci();
        assert!(rendered.contains("route-extraction"));
        assert!(rendered.contains("allowed added nodes"));
    }

    #[test]
    fn route_parity_gate_fails_core_symbol_drift() {
        let baseline = fixture_graph();
        let mut live = baseline.clone();
        live.graph_mut_unchecked().add_node(fixture_node(
            RepoNodeKey::Symbol("rust server/src/routes.rs `unexpected`().".to_string()),
            RepoGraphNodeKind::Symbol,
            "unexpected",
            Some("rust"),
            Some("server/src/routes.rs"),
            Some("rust server/src/routes.rs `unexpected`()."),
            false,
        ));

        let err = assert_route_extraction_graph_parity(&baseline, &live)
            .expect_err("core symbol additions are not route extractor output");
        let YkcgExtractorParityError::Diff(report) = err;

        assert!(!report.passed);
        let diff = report.failing_diff.as_ref().expect("failing diff");
        assert_eq!(
            diff.nodes
                .added_counts_by_kind
                .get(&RepoGraphNodeKind::Symbol)
                .copied(),
            Some(1)
        );
        assert!(report.render_for_ci().contains("failing diff samples"));
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
            Some(Path::new("ui/src/routes.ts")),
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
        assert!(typescript_fetch::is_enclosing_fetch_consumer_symbol(
            &consumer
        ));

        graph.graph_mut_unchecked()[NodeIndex::new(4)].kind = RepoGraphNodeKind::Table;
        assert!(!typescript_fetch::is_enclosing_fetch_consumer_symbol(
            graph.node(NodeIndex::new(4))
        ));

        graph.graph_mut_unchecked()[NodeIndex::new(4)].kind = RepoGraphNodeKind::Symbol;
        graph.graph_mut_unchecked()[NodeIndex::new(4)].symbol_kind = Some(ScipSymbolKind::Property);
        assert!(!typescript_fetch::is_enclosing_fetch_consumer_symbol(
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
