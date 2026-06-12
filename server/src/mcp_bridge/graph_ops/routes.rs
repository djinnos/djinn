use std::collections::{BTreeMap, BTreeSet, HashMap};

use djinn_control_plane::bridge::{
    ApiImpactEntry, ApiImpactResult, RelatedSymbol, RouteLanguageChain, RouteMapEntry,
    RouteMapResult, RouteRef, RouteShape, RouteSummary, ShapeCheckResult, ShapeDrift, ShapeField,
};
use djinn_graph::repo_graph::{
    RepoDependencyGraph, RepoGraphEdgeKind, RepoGraphNode, RepoGraphNodeKind, RepoNodeKey,
};
use petgraph::Direction;
use petgraph::graph::NodeIndex;
use petgraph::visit::EdgeRef;

use super::*;

#[derive(Debug, Clone)]
struct RouteSeed {
    route: NodeIndex,
    handler: Option<NodeIndex>,
    consumers: Vec<NodeIndex>,
}

impl RepoGraphBridge {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn route_map(
        &self,
        ctx: &ProjectCtx,
        route_id: Option<&str>,
        method: Option<&str>,
        path: Option<&str>,
        path_glob: Option<&str>,
        framework: Option<&str>,
        limit: usize,
    ) -> Result<RouteMapResult, String> {
        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        Ok(route_map_on_graph(
            &graph,
            route_id,
            method,
            path,
            path_glob,
            framework,
            limit.max(1),
        ))
    }

    pub(super) async fn shape_check(
        &self,
        ctx: &ProjectCtx,
        route_id: Option<&str>,
        method: Option<&str>,
        path: Option<&str>,
        include_optional: bool,
    ) -> Result<ShapeCheckResult, String> {
        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        Ok(shape_check_on_graph(
            &graph,
            route_id,
            method,
            path,
            include_optional,
        ))
    }

    pub(super) async fn api_impact(
        &self,
        ctx: &ProjectCtx,
        route_id: Option<&str>,
        method: Option<&str>,
        path: Option<&str>,
        min_confidence: f64,
        limit: usize,
    ) -> Result<ApiImpactResult, String> {
        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        Ok(api_impact_on_graph(
            &graph,
            route_id,
            method,
            path,
            min_confidence,
            limit.max(1),
        ))
    }
}

fn route_map_on_graph(
    graph: &RepoDependencyGraph,
    route_id: Option<&str>,
    method: Option<&str>,
    path: Option<&str>,
    path_glob: Option<&str>,
    framework: Option<&str>,
    limit: usize,
) -> RouteMapResult {
    let seeds = resolve_route_seeds(graph, route_id, method, path, path_glob, framework, limit);
    let routes = seeds
        .into_iter()
        .map(|seed| route_map_entry(graph, &seed))
        .collect::<Vec<_>>();
    RouteMapResult {
        routes,
        summary: route_summary(graph),
    }
}

fn route_map_entry(graph: &RepoDependencyGraph, seed: &RouteSeed) -> RouteMapEntry {
    let handler = seed.handler.map(|idx| symbol_ref(graph.node(idx), 1.0));
    let mut consumers = seed
        .consumers
        .iter()
        .map(|idx| route_consumer_ref(graph, seed.route, *idx))
        .collect::<Vec<_>>();
    sort_symbol_refs(&mut consumers);
    let mut middleware = middleware_for_route(graph, seed.route)
        .into_iter()
        .map(|idx| symbol_ref(graph.node(idx), 1.0))
        .collect::<Vec<_>>();
    sort_symbol_refs(&mut middleware);
    RouteMapEntry {
        route: route_ref(graph.node(seed.route)),
        handler,
        middleware,
        consumers,
    }
}

fn resolve_route_seeds(
    graph: &RepoDependencyGraph,
    route_id: Option<&str>,
    method: Option<&str>,
    path: Option<&str>,
    path_glob: Option<&str>,
    framework: Option<&str>,
    limit: usize,
) -> Vec<RouteSeed> {
    let mut seeds = graph
        .graph()
        .node_indices()
        .filter(|idx| graph.node(*idx).kind == RepoGraphNodeKind::Route)
        .filter(|idx| {
            route_matches(
                graph.node(*idx),
                route_id,
                method,
                path,
                path_glob,
                framework,
            )
        })
        .map(|route| {
            let handler = handler_for_route(graph, route);
            let mut consumers = consumers_for_route(graph, route);
            consumers.sort_by_key(|idx| format_node_key(&graph.node(*idx).id));
            consumers.dedup();
            RouteSeed {
                route,
                handler,
                consumers,
            }
        })
        .collect::<Vec<_>>();
    seeds.sort_by_key(|seed| format_node_key(&graph.node(seed.route).id));
    seeds.truncate(limit);
    seeds
}

fn route_matches(
    node: &RepoGraphNode,
    route_id: Option<&str>,
    method: Option<&str>,
    path: Option<&str>,
    path_glob: Option<&str>,
    framework: Option<&str>,
) -> bool {
    if let Some(framework) = non_empty(framework)
        && node.route_framework.as_deref() != Some(framework)
    {
        return false;
    }

    let rid = route_id_string(node);
    if let Some(route_id) = non_empty(route_id) {
        return rid == route_id || format_node_key(&node.id) == route_id;
    }

    let (route_method, route_path) = split_route_id(&rid);
    if let Some(method) = non_empty(method)
        && route_method.as_deref() != Some(&method.to_ascii_uppercase())
    {
        return false;
    }
    if let Some(path) = non_empty(path)
        && route_path.as_deref() != Some(path)
    {
        return false;
    }
    if let Some(glob) = non_empty(path_glob) {
        return route_path
            .as_deref()
            .is_some_and(|p| glob_match(glob, p) || glob_match(glob, &rid));
    }
    true
}

fn non_empty(input: Option<&str>) -> Option<&str> {
    input.map(str::trim).filter(|s| !s.is_empty())
}

fn handler_for_route(graph: &RepoDependencyGraph, route: NodeIndex) -> Option<NodeIndex> {
    graph
        .graph()
        .edges_directed(route, Direction::Outgoing)
        .filter(|edge| edge.weight().kind == RepoGraphEdgeKind::HandlesRoute)
        .map(|edge| edge.target())
        .find(|idx| graph.node(*idx).kind == RepoGraphNodeKind::Symbol)
        .or_else(|| {
            let route_node = graph.node(route);
            let symbol = route_node.route_handler_symbol.as_deref()?;
            graph.graph().node_indices().find(|idx| {
                matches!(&graph.node(*idx).id, RepoNodeKey::Symbol(s) if s == symbol)
                    || graph.node(*idx).symbol.as_deref() == Some(symbol)
            })
        })
}

/// PR s6ch / 92z7: list the `Symbol` nodes that fetch into the given
/// route. The route-exclusion helpers in [`consumer_exclusion_reason`]
/// and [`shared::first_exclusion_reason`] do the heavy lifting on
/// the per-consumer side; this just enumerates the candidates.
fn consumers_for_route(graph: &RepoDependencyGraph, route: NodeIndex) -> Vec<NodeIndex> {
    graph
        .graph()
        .edges_directed(route, Direction::Incoming)
        .filter(|edge| edge.weight().kind == RepoGraphEdgeKind::Fetches)
        .map(|edge| edge.source())
        .filter(|idx| graph.node(*idx).kind == RepoGraphNodeKind::Symbol)
        .collect()
}

/// PR s6ch / 92z7: compute the active route-exclusion reason for a
/// consumer node reached through a `Fetches` edge into the route
/// node we just resolved. Returns `None` when the edge is a hard
/// dependency under the policy, or the caller has disabled
/// `DJINN_ROUTE_PARITY` (the shadow path keeps every consumer as a
/// hard entry for diagnostic comparison).
///
/// For each `Fetches` edge that lands on the route from the queried
/// consumer, we compute the first reason the policy emits. When the
/// consumer has multiple `Fetches` edges (e.g. `ui/src/api.ts` calls
/// `/api/agents` from several symbols), we pick the highest-
/// confidence edge so the strongest signal wins.
fn consumer_exclusion_reason(graph: &RepoDependencyGraph, consumer: NodeIndex) -> Option<String> {
    if !djinn_graph::route_extraction::route_parity_enabled() {
        return None;
    }
    let cfg = graph.route_exclusion_config();
    let mut best: Option<f64> = None;
    let mut best_reason: Option<String> = None;
    for edge in graph.graph().edges_directed(consumer, Direction::Outgoing) {
        if edge.weight().kind != RepoGraphEdgeKind::Fetches {
            continue;
        }
        let target = edge.target();
        let target_node = graph.node(target);
        if target_node.kind != RepoGraphNodeKind::Route {
            continue;
        }
        let route_path = shared::route_node_path(target_node);
        let reason = shared::first_exclusion_reason(edge.weight(), route_path.as_deref(), cfg)
            .map(|s| s.to_string());
        if let Some(reason) = reason {
            match best {
                Some(prev) if edge.weight().confidence <= prev => {}
                _ => {
                    best = Some(edge.weight().confidence);
                    best_reason = Some(reason);
                }
            }
        }
    }
    best_reason
}

fn middleware_for_route(graph: &RepoDependencyGraph, route: NodeIndex) -> Vec<NodeIndex> {
    let mut out = BTreeSet::new();
    if let Some(handler) = handler_for_route(graph, route) {
        for edge in graph.graph().edges_directed(handler, Direction::Incoming) {
            if edge.weight().kind == RepoGraphEdgeKind::EntryPointOf {
                out.insert(edge.source());
            }
        }
    }
    for edge in graph.graph().edges_directed(route, Direction::Incoming) {
        if edge.weight().kind == RepoGraphEdgeKind::EntryPointOf {
            out.insert(edge.source());
        }
    }
    out.into_iter()
        .filter(|idx| graph.node(*idx).kind == RepoGraphNodeKind::Symbol)
        .collect()
}

fn route_summary(graph: &RepoDependencyGraph) -> RouteSummary {
    let mut total_routes = 0usize;
    let mut framework_counts = BTreeMap::new();
    let mut handler_counts = BTreeMap::new();
    for idx in graph.graph().node_indices() {
        let node = graph.node(idx);
        if node.kind != RepoGraphNodeKind::Route {
            continue;
        }
        total_routes += 1;
        let framework = node
            .route_framework
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        *framework_counts.entry(framework).or_insert(0) += 1;
        let handler = handler_for_route(graph, idx)
            .map(|h| graph.node(h).display_name.clone())
            .or_else(|| node.route_handler_symbol.clone())
            .unwrap_or_else(|| "unknown".to_string());
        *handler_counts.entry(handler).or_insert(0) += 1;
    }
    RouteSummary {
        total_routes,
        framework_counts,
        handler_counts,
    }
}

fn shape_check_on_graph(
    graph: &RepoDependencyGraph,
    route_id: Option<&str>,
    method: Option<&str>,
    path: Option<&str>,
    include_optional: bool,
) -> ShapeCheckResult {
    let Some(seed) = resolve_route_seeds(graph, route_id, method, path, None, None, 1)
        .into_iter()
        .next()
    else {
        return ShapeCheckResult::default();
    };

    let handler_shape = seed
        .handler
        .map(|idx| extract_shape(graph.node(idx), include_optional))
        .unwrap_or_default();
    let route_shape = RouteShape {
        route: route_ref(graph.node(seed.route)),
        response_fields: handler_shape.values().cloned().collect(),
    };
    let mut drifts = seed
        .consumers
        .iter()
        .filter_map(|consumer| {
            let consumer_shape = extract_shape(graph.node(*consumer), include_optional);
            drift_for_consumer(
                graph,
                seed.route,
                *consumer,
                &handler_shape,
                &consumer_shape,
            )
        })
        .collect::<Vec<_>>();
    drifts.sort_by(|a, b| a.consumer.uid.cmp(&b.consumer.uid));
    ShapeCheckResult {
        route_shape,
        drifts,
    }
}

fn api_impact_on_graph(
    graph: &RepoDependencyGraph,
    route_id: Option<&str>,
    method: Option<&str>,
    path: Option<&str>,
    min_confidence: f64,
    limit: usize,
) -> ApiImpactResult {
    let Some(seed) = resolve_route_seeds(graph, route_id, method, path, None, None, 1)
        .into_iter()
        .next()
    else {
        return ApiImpactResult::default();
    };
    let shape = shape_check_on_graph(graph, route_id, method, path, false);
    let drift_by_uid: HashMap<String, ShapeDrift> = shape
        .drifts
        .into_iter()
        .map(|d| (d.consumer.uid.clone(), d))
        .collect();
    let mut by_uid: BTreeMap<String, ApiImpactEntry> = BTreeMap::new();

    for consumer in &seed.consumers {
        let node = graph.node(*consumer);
        let uid = format_node_key(&node.id);
        let drift = drift_by_uid.get(&uid);
        let (risk_tier, reason) = risk_for(drift, 1);
        // PR s6ch / 92z7: stamp the route-exclusion policy on each
        // direct consumer so the UI can downgrade noisy inferred
        // routes (e.g. /health, /ping) to suggestions.
        let exclusion_reason = consumer_exclusion_reason(graph, *consumer);
        by_uid.insert(
            uid,
            ApiImpactEntry {
                consumer: route_consumer_ref(graph, seed.route, *consumer),
                risk_tier,
                reason,
                exclusion_reason,
            },
        );
    }

    for start in seed.handler.into_iter().chain(std::iter::once(seed.route)) {
        for (idx, impact) in shared::impact_bfs_with_policy(
            graph,
            start,
            3,
            Some(min_confidence),
            Some(graph.route_exclusion_config()),
        ) {
            let node = graph.node(idx);
            if node.is_external || node.kind != RepoGraphNodeKind::Symbol {
                continue;
            }
            let drift = drift_by_uid.get(&impact.key);
            let (risk_tier, reason) = risk_for(drift, impact.depth);
            // PR s6ch / 92z7: surface the policy reason on transitive
            // entries too, so a node only reachable via a `Fetches`
            // hop the policy excludes still gets flagged.
            let exclusion_reason = impact.exclusion_reason.clone();
            by_uid.entry(impact.key.clone()).or_insert(ApiImpactEntry {
                consumer: symbol_ref(node, 1.0),
                risk_tier,
                reason,
                exclusion_reason,
            });
        }
    }

    let mut impacts = by_uid.into_values().collect::<Vec<_>>();
    impacts.sort_by(|a, b| {
        risk_rank(&b.risk_tier)
            .cmp(&risk_rank(&a.risk_tier))
            .then_with(|| a.consumer.uid.cmp(&b.consumer.uid))
    });
    impacts.truncate(limit);
    ApiImpactResult { impacts }
}

fn risk_for(drift: Option<&ShapeDrift>, depth: usize) -> (String, String) {
    if let Some(drift) = drift {
        let drift_count =
            drift.missing_keys.len() + drift.extra_keys.len() + drift.type_mismatches.len();
        let tier = if !drift.missing_keys.is_empty() || !drift.type_mismatches.is_empty() {
            "high"
        } else {
            "med"
        };
        return (
            tier.to_string(),
            format!("shape drift ({drift_count} issue(s)) overlaps route consumer"),
        );
    }
    if depth <= 1 {
        (
            "med".to_string(),
            "direct route consumer in impact blast radius".to_string(),
        )
    } else {
        (
            "low".to_string(),
            format!("transitive impact at depth {depth} with no detected shape drift"),
        )
    }
}

fn risk_rank(tier: &str) -> u8 {
    match tier {
        "high" => 3,
        "med" | "medium" => 2,
        _ => 1,
    }
}

fn drift_for_consumer(
    graph: &RepoDependencyGraph,
    route: NodeIndex,
    consumer: NodeIndex,
    handler_shape: &BTreeMap<String, ShapeField>,
    consumer_shape: &BTreeMap<String, ShapeField>,
) -> Option<ShapeDrift> {
    let handler_keys = handler_shape.keys().cloned().collect::<BTreeSet<_>>();
    let consumer_keys = consumer_shape.keys().cloned().collect::<BTreeSet<_>>();
    let missing_keys = consumer_keys
        .difference(&handler_keys)
        .cloned()
        .collect::<Vec<_>>();
    let extra_keys = handler_keys
        .difference(&consumer_keys)
        .cloned()
        .collect::<Vec<_>>();
    let mut type_mismatches = Vec::new();
    for key in handler_keys.intersection(&consumer_keys) {
        let server = handler_shape.get(key).and_then(|f| f.type_name.as_deref());
        let client = consumer_shape.get(key).and_then(|f| f.type_name.as_deref());
        if let (Some(server), Some(client)) = (server, client)
            && !server.eq_ignore_ascii_case(client)
        {
            type_mismatches.push(format!("{key}: server {server}, consumer {client}"));
        }
    }
    if missing_keys.is_empty() && extra_keys.is_empty() && type_mismatches.is_empty() {
        None
    } else {
        Some(ShapeDrift {
            consumer: route_consumer_ref(graph, route, consumer),
            missing_keys,
            extra_keys,
            type_mismatches,
        })
    }
}

fn extract_shape(node: &RepoGraphNode, include_optional: bool) -> BTreeMap<String, ShapeField> {
    let mut fields = BTreeMap::new();
    if let Some(parts) = &node.signature_parts
        && let Some(ret) = &parts.return_type
    {
        merge_shape_text(&mut fields, ret, include_optional);
    }
    if let Some(sig) = &node.signature {
        merge_shape_text(&mut fields, sig, include_optional);
    }
    for doc in &node.documentation {
        merge_shape_text(&mut fields, doc, include_optional);
    }
    fields
}

fn merge_shape_text(fields: &mut BTreeMap<String, ShapeField>, text: &str, include_optional: bool) {
    for between in brace_segments(text) {
        for raw in between.split(',') {
            let Some((key, ty)) = raw.split_once(':') else {
                continue;
            };
            let key = clean_ident(key);
            if !is_shape_key(&key) {
                continue;
            }
            let optional = raw.contains('?');
            if optional && !include_optional {
                continue;
            }
            let ty = clean_type(ty);
            fields.entry(key.clone()).or_insert(ShapeField {
                name: key,
                type_name: (!ty.is_empty()).then_some(ty),
                optional,
            });
        }
    }

    for marker in ["response.", "body.", "json.", "data."] {
        let mut rest = text;
        while let Some(pos) = rest.find(marker) {
            let tail = &rest[pos + marker.len()..];
            let key = tail
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '?')
                .collect::<String>();
            let key = clean_ident(&key);
            if is_shape_key(&key) {
                fields.entry(key.clone()).or_insert(ShapeField {
                    name: key,
                    type_name: None,
                    optional: false,
                });
            }
            rest = &tail[tail.len().min(1)..];
        }
    }
}

fn brace_segments(text: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut start = None;
    for (idx, ch) in text.char_indices() {
        match ch {
            '{' => start = Some(idx + ch.len_utf8()),
            '}' => {
                if let Some(s) = start.take()
                    && s <= idx
                {
                    segments.push(&text[s..idx]);
                }
            }
            _ => {}
        }
    }
    segments
}

fn is_shape_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && !matches!(
            key,
            "Json" | "Result" | "Option" | "Vec" | "response_shape" | "uses" | "response"
        )
}

fn clean_ident(input: &str) -> String {
    input
        .trim()
        .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .trim_end_matches('?')
        .to_string()
}

fn clean_type(input: &str) -> String {
    input
        .trim()
        .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '<' && c != '>')
        .to_string()
}

fn route_ref(node: &RepoGraphNode) -> RouteRef {
    let id = route_id_string(node);
    let (method, path) = split_route_id(&id);
    RouteRef {
        uid: format_node_key(&node.id),
        id,
        method,
        path,
        framework: node.route_framework.clone(),
    }
}

fn route_id_string(node: &RepoGraphNode) -> String {
    match &node.id {
        RepoNodeKey::Route(id) => id.clone(),
        _ => node.display_name.clone(),
    }
}

fn split_route_id(id: &str) -> (Option<String>, Option<String>) {
    let without_framework = id.split_once(" (").map_or(id, |(left, _)| left);
    let mut parts = without_framework.split_whitespace();
    let method = parts.next().map(|m| m.to_ascii_uppercase());
    let path = parts.next().map(str::to_string);
    (method, path)
}

fn symbol_ref(node: &RepoGraphNode, confidence: f64) -> RelatedSymbol {
    RelatedSymbol {
        uid: format_node_key(&node.id),
        name: node.display_name.clone(),
        kind: kind_label_for_node(node).to_string(),
        file_path: shared::repo_graph_node_file_path(node),
        confidence,
        confidence_tier: "extracted".to_string(),
        route_language_chain: None,
    }
}

fn route_consumer_ref(
    graph: &RepoDependencyGraph,
    route: NodeIndex,
    consumer: NodeIndex,
) -> RelatedSymbol {
    let consumer_node = graph.node(consumer);
    let Some(fetches_edge) = graph
        .graph()
        .edges_connecting(consumer, route)
        .find(|edge| {
            edge.weight().kind == RepoGraphEdgeKind::Fetches
                && edge.source() == consumer
                && edge.target() == route
        })
    else {
        return symbol_ref(consumer_node, 1.0);
    };
    let edge = fetches_edge.weight();
    let route_language_chain = graph
        .route_edge_language_chain(consumer, route, edge.kind)
        .map(|chain| RouteLanguageChain {
            source_language: chain.source_language,
            target_language: chain.target_language,
            is_cross_language: chain.is_cross_language,
        });
    RelatedSymbol {
        uid: format_node_key(&consumer_node.id),
        name: consumer_node.display_name.clone(),
        kind: kind_label_for_node(consumer_node).to_string(),
        file_path: shared::repo_graph_node_file_path(consumer_node),
        confidence: edge.confidence,
        confidence_tier: format!("{:?}", edge.confidence_tier()).to_ascii_lowercase(),
        route_language_chain,
    }
}

fn sort_symbol_refs(symbols: &mut Vec<RelatedSymbol>) {
    symbols.sort_by(|a, b| a.uid.cmp(&b.uid));
    symbols.dedup_by(|a, b| a.uid == b.uid);
}

fn glob_match(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return pattern == value;
    }
    let mut rest = value;
    let mut parts = pattern.split('*').peekable();
    let anchored_start = !pattern.starts_with('*');
    let anchored_end = !pattern.ends_with('*');
    if let Some(first) = parts.next()
        && !first.is_empty()
    {
        if anchored_start {
            let Some(stripped) = rest.strip_prefix(first) else {
                return false;
            };
            rest = stripped;
        } else if let Some(pos) = rest.find(first) {
            rest = &rest[pos + first.len()..];
        } else {
            return false;
        }
    }
    let mut last = "";
    for part in parts {
        last = part;
        if part.is_empty() {
            continue;
        }
        if let Some(pos) = rest.find(part) {
            rest = &rest[pos + part.len()..];
        } else {
            return false;
        }
    }
    !anchored_end || last.is_empty() || value.ends_with(last)
}

#[cfg(test)]
pub(super) mod test_helpers {
    use super::*;

    pub(crate) fn route_map_for_graph(graph: &RepoDependencyGraph) -> RouteMapResult {
        route_map_on_graph(graph, None, None, None, None, None, 20)
    }

    pub(crate) fn shape_check_for_graph(graph: &RepoDependencyGraph) -> ShapeCheckResult {
        shape_check_on_graph(graph, Some("GET /api/agents (axum)"), None, None, false)
    }

    pub(crate) fn api_impact_for_graph(graph: &RepoDependencyGraph) -> ApiImpactResult {
        api_impact_on_graph(graph, Some("GET /api/agents (axum)"), None, None, 0.5, 20)
    }
}
