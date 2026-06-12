use std::collections::BTreeMap;

use djinn_control_plane::bridge::{FlowHit, RelatedSymbol};
use djinn_graph::repo_graph::{RepoDependencyGraph, RepoGraphNode, RepoGraphNodeKind};
use petgraph::graph::NodeIndex;

use super::*;

impl RepoGraphBridge {
    pub(super) async fn flow(
        &self,
        ctx: &ProjectCtx,
        query: &str,
        kind_filter: Option<&str>,
        limit: usize,
    ) -> Result<FlowResult, String> {
        let query = query.trim();
        if query.is_empty() {
            return Err("query is required for flow".to_string());
        }
        let mode = FlowKindFilter::parse(kind_filter)?;
        let limit = limit.max(1);

        let search_kind_filter = match mode {
            // Step hits are symbol hits. Process/omitted queries must stay
            // unfiltered so the existing hybrid pipeline can surface either
            // process nodes (structural) or symbol/chunk hits (lexical,
            // semantic, structural) without reimplementing any signal.
            FlowKindFilter::Step => Some("symbol"),
            FlowKindFilter::Any | FlowKindFilter::Process => None,
        };
        let hits = self
            .hybrid_search(
                ctx,
                query,
                search_kind_filter,
                limit.saturating_mul(4).max(limit),
            )
            .await?;
        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        Ok(flow_on_graph_from_hits(&graph, hits, mode, limit))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlowKindFilter {
    Any,
    Process,
    Step,
}

impl FlowKindFilter {
    fn parse(kind_filter: Option<&str>) -> Result<Self, String> {
        match kind_filter.map(str::trim).filter(|s| !s.is_empty()) {
            None => Ok(Self::Any),
            Some("process") => Ok(Self::Process),
            Some("step") => Ok(Self::Step),
            Some(other) => Err(format!(
                "invalid kind_filter '{other}' for flow: expected 'process' or 'step'"
            )),
        }
    }
}

fn flow_on_graph_from_hits(
    graph: &RepoDependencyGraph,
    hits: Vec<SearchHit>,
    mode: FlowKindFilter,
    limit: usize,
) -> FlowResult {
    let mut by_key: BTreeMap<(String, String), FlowHit> = BTreeMap::new();
    for hit in hits {
        if let Some(process_id) = hit.key.strip_prefix("process:") {
            if mode != FlowKindFilter::Step {
                add_process_id_hit(graph, process_id, hit.score, &mut by_key);
            }
            continue;
        }
        let Ok(node_index) = resolve_node_or_err(graph, &hit.key) else {
            // Lexical/semantic chunk hits without symbol affinity use
            // `chunk:<id>` keys. They are legitimate hybrid results but cannot
            // be mapped to process membership, so flow treats them as misses.
            continue;
        };
        let node = graph.node(node_index);
        if node.kind == RepoGraphNodeKind::Process {
            if mode != FlowKindFilter::Step {
                add_process_node_hit(graph, node_index, hit.score, &mut by_key);
            }
            continue;
        }
        if mode == FlowKindFilter::Process {
            continue;
        }
        for process in graph.processes_for_node(node_index) {
            let step_index = process_step_index(&process.steps, node_index);
            let flow_hit = FlowHit {
                process: ProcessRef {
                    id: process.id.clone(),
                    uid: process.id.clone(),
                    label: process.label.clone(),
                    role: "step".to_string(),
                },
                matched_step: related_symbol(graph.node(node_index), hit.score),
                matched_step_index: step_index,
                rrf_score: hit.score,
            };
            insert_best(&mut by_key, flow_hit);
        }
    }
    let mut out = by_key.into_values().collect::<Vec<_>>();
    sort_flow_hits(&mut out);
    out.truncate(limit.max(1));
    FlowResult { hits: out }
}

fn add_process_id_hit(
    graph: &RepoDependencyGraph,
    process_id: &str,
    score: f64,
    by_key: &mut BTreeMap<(String, String), FlowHit>,
) {
    for process in graph
        .processes()
        .iter()
        .filter(|process| process.id == process_id)
    {
        add_process_hit(graph, process, score, by_key);
    }
}

fn add_process_node_hit(
    graph: &RepoDependencyGraph,
    node_index: NodeIndex,
    score: f64,
    by_key: &mut BTreeMap<(String, String), FlowHit>,
) {
    for process in graph
        .processes()
        .iter()
        .filter(|process| process.process_node_id == node_index)
    {
        add_process_hit(graph, process, score, by_key);
    }
}

fn add_process_hit(
    graph: &RepoDependencyGraph,
    process: &djinn_graph::processes::Process,
    score: f64,
    by_key: &mut BTreeMap<(String, String), FlowHit>,
) {
    let Some(&entry_step) = process.steps.first() else {
        return;
    };
    let flow_hit = FlowHit {
        process: ProcessRef {
            id: process.id.clone(),
            uid: process.id.clone(),
            label: process.label.clone(),
            role: "process".to_string(),
        },
        matched_step: related_symbol(graph.node(entry_step), score),
        matched_step_index: 0,
        rrf_score: score,
    };
    insert_best(by_key, flow_hit);
}

fn process_step_index(steps: &[NodeIndex], node_index: NodeIndex) -> i32 {
    steps
        .iter()
        .position(|step| *step == node_index)
        .map(|idx| idx as i32)
        .unwrap_or(-1)
}

fn insert_best(by_key: &mut BTreeMap<(String, String), FlowHit>, hit: FlowHit) {
    let key = (hit.process.id.clone(), hit.matched_step.uid.clone());
    by_key
        .entry(key)
        .and_modify(|existing| {
            if is_better_duplicate(&hit, existing) {
                *existing = hit.clone();
            }
        })
        .or_insert(hit);
}

fn is_better_duplicate(candidate: &FlowHit, existing: &FlowHit) -> bool {
    candidate.rrf_score > existing.rrf_score
        || (candidate.rrf_score == existing.rrf_score
            && candidate.matched_step_index < existing.matched_step_index)
}

fn sort_flow_hits(hits: &mut [FlowHit]) {
    hits.sort_by(|a, b| {
        b.rrf_score
            .partial_cmp(&a.rrf_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.matched_step_index.cmp(&b.matched_step_index))
            .then_with(|| a.process.id.cmp(&b.process.id))
            .then_with(|| a.matched_step.uid.cmp(&b.matched_step.uid))
    });
}

fn related_symbol(node: &RepoGraphNode, confidence: f64) -> RelatedSymbol {
    RelatedSymbol {
        uid: format_node_key(&node.id),
        name: node.display_name.clone(),
        kind: kind_label_for_node(node),
        file_path: shared::repo_graph_node_file_path(node),
        confidence,
        confidence_tier: "extracted".to_string(),
        confidence_reason: None,
        excluded_reason: None,
        route_language_chain: None,
    }
}

#[cfg(test)]
pub(super) mod test_helpers {
    use super::*;

    pub(crate) fn flow_for_graph_from_hits(
        graph: &RepoDependencyGraph,
        hits: Vec<SearchHit>,
        kind_filter: Option<&str>,
        limit: usize,
    ) -> Result<FlowResult, String> {
        let mode = FlowKindFilter::parse(kind_filter)?;
        Ok(flow_on_graph_from_hits(graph, hits, mode, limit.max(1)))
    }
}
