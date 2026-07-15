//! ykcg-specific parity adapter for synthetic extractor rollouts.
//!
//! This module layers extractor allowlists over the reusable `graph_parity`
//! diff API. It keeps the core invariant strict for pre-existing files, nodes,
//! edges, and communities while allowing callers to report intentional
//! synthetic node/edge additions owned by one extractor wave.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::graph_parity::{GraphParityDiff, diff_graph_parity};
use crate::repo_graph::{
    RepoDependencyGraph, RepoGraphEdgeKind, RepoGraphNodeKind,
    deserialize_repo_graph_artifact_bincode,
};

const DEFAULT_YKCG_SAMPLE_LIMIT: usize = 20;

/// Configuration for a ykcg synthetic-extractor parity assertion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YkcgExtractorParityConfig {
    pub extractor_label: String,
    pub allowed_added_node_kinds: BTreeSet<RepoGraphNodeKind>,
    pub allowed_added_edge_kinds: BTreeSet<RepoGraphEdgeKind>,
    pub sample_limit: usize,
}

impl YkcgExtractorParityConfig {
    /// Build a config with the default bounded sample size used in CI logs.
    pub fn new(
        extractor_label: impl Into<String>,
        allowed_added_node_kinds: impl IntoIterator<Item = RepoGraphNodeKind>,
        allowed_added_edge_kinds: impl IntoIterator<Item = RepoGraphEdgeKind>,
    ) -> Self {
        Self {
            extractor_label: extractor_label.into(),
            allowed_added_node_kinds: allowed_added_node_kinds.into_iter().collect(),
            allowed_added_edge_kinds: allowed_added_edge_kinds.into_iter().collect(),
            sample_limit: DEFAULT_YKCG_SAMPLE_LIMIT,
        }
    }

    /// Override the per-kind sample bound used in the rendered report.
    pub fn with_sample_limit(mut self, sample_limit: usize) -> Self {
        self.sample_limit = sample_limit;
        self
    }
}

/// Assert baseline-vs-live graph parity for one ykcg synthetic extractor.
///
/// Additions whose node/edge kind is explicitly allowlisted are reported as
/// allowed synthetic additions. Any file delta, removal, unexpected addition, or
/// core community/member drift fails the assertion.
pub fn assert_ykcg_extractor_graph_parity(
    baseline: &RepoDependencyGraph,
    live: &RepoDependencyGraph,
    config: &YkcgExtractorParityConfig,
) -> Result<YkcgExtractorParityReport, YkcgExtractorParityError> {
    let diff = diff_graph_parity(baseline, live, config.sample_limit);
    let report = YkcgExtractorParityReport::from_diff(config, diff);
    if report.passed {
        Ok(report)
    } else {
        Err(YkcgExtractorParityError::Diff(Box::new(report)))
    }
}

/// Assert parity for serialized repo-graph artifact blobs.
///
/// The blobs are deserialized through the same compatibility seam used by the
/// core graph parity API so v10 additive layout shims are honored.
pub fn assert_ykcg_extractor_artifact_blob_parity(
    baseline_blob: &[u8],
    live_blob: &[u8],
    config: &YkcgExtractorParityConfig,
) -> Result<YkcgExtractorParityReport, YkcgExtractorArtifactBlobParityError> {
    let baseline_artifact = deserialize_repo_graph_artifact_bincode(baseline_blob)
        .map_err(YkcgExtractorArtifactBlobParityError::DeserializeBaseline)?;
    let live_artifact = deserialize_repo_graph_artifact_bincode(live_blob)
        .map_err(YkcgExtractorArtifactBlobParityError::DeserializeLive)?;
    let baseline = RepoDependencyGraph::from_artifact(&baseline_artifact);
    let live = RepoDependencyGraph::from_artifact(&live_artifact);
    assert_ykcg_extractor_graph_parity(&baseline, &live, config)
        .map_err(|err| YkcgExtractorArtifactBlobParityError::Diff(Box::new(err)))
}

/// Structured report suitable for PR/CI logs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct YkcgExtractorParityReport {
    pub extractor_label: String,
    pub passed: bool,
    pub sample_limit: usize,
    pub node_counts_by_kind: PopulationCountsByKind<RepoGraphNodeKind>,
    pub edge_counts_by_kind: PopulationCountsByKind<RepoGraphEdgeKind>,
    pub allowed_added_nodes: BTreeMap<RepoGraphNodeKind, AllowedAdditionReport>,
    pub allowed_added_edges: BTreeMap<RepoGraphEdgeKind, AllowedAdditionReport>,
    pub failing_diff: Option<GraphParityDiff>,
}

impl YkcgExtractorParityReport {
    fn from_diff(config: &YkcgExtractorParityConfig, diff: GraphParityDiff) -> Self {
        let node_counts_by_kind = PopulationCountsByKind {
            baseline: diff.nodes.old_counts_by_kind.clone(),
            live: diff.nodes.new_counts_by_kind.clone(),
        };
        let edge_counts_by_kind = PopulationCountsByKind {
            baseline: diff.edges.old_counts_by_kind.clone(),
            live: diff.edges.new_counts_by_kind.clone(),
        };
        let allowed_added_nodes = allowed_additions(
            &diff.nodes.added_counts_by_kind,
            &diff.nodes.added_samples_by_kind,
            &config.allowed_added_node_kinds,
        );
        let allowed_added_edges = allowed_additions(
            &diff.edges.added_counts_by_kind,
            &diff.edges.added_samples_by_kind,
            &config.allowed_added_edge_kinds,
        );
        let passed = !has_core_file_drift(&diff)
            && !has_core_kind_drift(
                &diff.nodes.added_counts_by_kind,
                diff.nodes.removed_count,
                &config.allowed_added_node_kinds,
            )
            && !has_core_kind_drift(
                &diff.edges.added_counts_by_kind,
                diff.edges.removed_count,
                &config.allowed_added_edge_kinds,
            )
            && !has_core_community_drift(&diff, &config.allowed_added_node_kinds);
        let failing_diff = if passed { None } else { Some(diff) };
        Self {
            extractor_label: config.extractor_label.clone(),
            passed,
            sample_limit: config.sample_limit,
            node_counts_by_kind,
            edge_counts_by_kind,
            allowed_added_nodes,
            allowed_added_edges,
            failing_diff,
        }
    }

    /// Render the report as bounded, line-oriented text for CI/PR logs.
    pub fn render_for_ci(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for YkcgExtractorParityReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "ykcg extractor parity: {} ({})",
            self.extractor_label,
            if self.passed { "passed" } else { "failed" }
        )?;
        writeln!(f, "sample_limit: {}", self.sample_limit)?;
        writeln!(f, "node counts by kind:")?;
        write_population_counts(f, &self.node_counts_by_kind)?;
        writeln!(f, "edge counts by kind:")?;
        write_population_counts(f, &self.edge_counts_by_kind)?;
        writeln!(f, "allowed added nodes:")?;
        write_allowed_additions(f, &self.allowed_added_nodes)?;
        writeln!(f, "allowed added edges:")?;
        write_allowed_additions(f, &self.allowed_added_edges)?;
        if let Some(diff) = &self.failing_diff {
            writeln!(f, "failing diff samples:")?;
            writeln!(
                f,
                "  files: +{} {:?} / -{} {:?}",
                diff.files.added_count,
                diff.files.added_samples,
                diff.files.removed_count,
                diff.files.removed_samples
            )?;
            writeln!(
                f,
                "  nodes: +{} {:?} / -{} {:?}",
                diff.nodes.added_count,
                diff.nodes.added_samples,
                diff.nodes.removed_count,
                diff.nodes.removed_samples
            )?;
            writeln!(
                f,
                "  edges: +{} {:?} / -{} {:?}",
                diff.edges.added_count,
                diff.edges.added_samples,
                diff.edges.removed_count,
                diff.edges.removed_samples
            )?;
            writeln!(
                f,
                "  communities: +{} {:?} / -{} {:?}; membership_deltas={:?}",
                diff.communities.added_count,
                diff.communities.added_samples,
                diff.communities.removed_count,
                diff.communities.removed_samples,
                diff.communities.membership_deltas
            )?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PopulationCountsByKind<K>
where
    K: Ord,
{
    pub baseline: BTreeMap<K, usize>,
    pub live: BTreeMap<K, usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowedAdditionReport {
    pub count: usize,
    pub samples: Vec<String>,
}

/// Error returned by [`assert_ykcg_extractor_graph_parity`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum YkcgExtractorParityError {
    Diff(Box<YkcgExtractorParityReport>),
}

impl fmt::Display for YkcgExtractorParityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Diff(report) => write!(f, "{report}"),
        }
    }
}

impl std::error::Error for YkcgExtractorParityError {}

/// Error returned by [`assert_ykcg_extractor_artifact_blob_parity`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum YkcgExtractorArtifactBlobParityError {
    DeserializeBaseline(String),
    DeserializeLive(String),
    Diff(Box<YkcgExtractorParityError>),
}

impl fmt::Display for YkcgExtractorArtifactBlobParityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeserializeBaseline(err) => {
                write!(f, "deserialize baseline graph artifact: {err}")
            }
            Self::DeserializeLive(err) => write!(f, "deserialize live graph artifact: {err}"),
            Self::Diff(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for YkcgExtractorArtifactBlobParityError {}

fn allowed_additions<K>(
    added_counts_by_kind: &BTreeMap<K, usize>,
    added_samples_by_kind: &BTreeMap<K, Vec<String>>,
    allowlist: &BTreeSet<K>,
) -> BTreeMap<K, AllowedAdditionReport>
where
    K: Clone + Ord,
{
    allowlist
        .iter()
        .filter_map(|kind| {
            let count = added_counts_by_kind.get(kind).copied().unwrap_or_default();
            (count > 0).then(|| {
                (
                    kind.clone(),
                    AllowedAdditionReport {
                        count,
                        samples: added_samples_by_kind.get(kind).cloned().unwrap_or_default(),
                    },
                )
            })
        })
        .collect()
}

fn has_core_file_drift(diff: &GraphParityDiff) -> bool {
    !diff.files.is_empty()
}

fn has_core_kind_drift<K>(
    added_counts_by_kind: &BTreeMap<K, usize>,
    removed_count: usize,
    allowlist: &BTreeSet<K>,
) -> bool
where
    K: Ord,
{
    removed_count > 0
        || added_counts_by_kind
            .iter()
            .any(|(kind, count)| *count > 0 && !allowlist.contains(kind))
}

fn has_core_community_drift(
    diff: &GraphParityDiff,
    allowed_added_node_kinds: &BTreeSet<RepoGraphNodeKind>,
) -> bool {
    if diff.communities.added_count > 0 || diff.communities.removed_count > 0 {
        return true;
    }

    diff.communities.membership_deltas.values().any(|delta| {
        if delta.removed_count > 0 {
            return true;
        }
        if delta.added_count == 0 {
            return false;
        }
        if delta.added_samples.len() != delta.added_count {
            return true;
        }
        !delta
            .added_samples
            .iter()
            .all(|sample| is_allowed_node_uid(sample, allowed_added_node_kinds))
    })
}

fn is_allowed_node_uid(uid: &str, allowed_added_node_kinds: &BTreeSet<RepoGraphNodeKind>) -> bool {
    allowed_added_node_kinds
        .iter()
        .any(|kind| uid.starts_with(node_uid_prefix(*kind)))
}

fn node_uid_prefix(kind: RepoGraphNodeKind) -> &'static str {
    match kind {
        RepoGraphNodeKind::File => "file:",
        RepoGraphNodeKind::Symbol => "symbol:",
        RepoGraphNodeKind::Process => "process:",
        RepoGraphNodeKind::Table => "table:",
        RepoGraphNodeKind::Route => "route:",
        RepoGraphNodeKind::Tool => "tool:",
    }
}

fn write_population_counts<K>(
    f: &mut fmt::Formatter<'_>,
    counts: &PopulationCountsByKind<K>,
) -> fmt::Result
where
    K: fmt::Debug + Ord,
{
    for kind in counts.baseline.keys().chain(counts.live.keys()) {
        writeln!(
            f,
            "  {:?}: baseline={}, live={}",
            kind,
            counts.baseline.get(kind).copied().unwrap_or_default(),
            counts.live.get(kind).copied().unwrap_or_default()
        )?;
    }
    Ok(())
}

fn write_allowed_additions<K>(
    f: &mut fmt::Formatter<'_>,
    additions: &BTreeMap<K, AllowedAdditionReport>,
) -> fmt::Result
where
    K: fmt::Debug + Ord,
{
    if additions.is_empty() {
        writeln!(f, "  none")?;
        return Ok(());
    }
    for (kind, report) in additions {
        writeln!(f, "  {:?}: +{} {:?}", kind, report.count, report.samples)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::communities::Community;
    use crate::repo_graph::{
        REPO_GRAPH_ARTIFACT_VERSION, RepoGraphArtifact, RepoGraphArtifactEdge, RepoGraphNode,
        RepoNodeKey,
    };

    fn graph_from_artifact(artifact: RepoGraphArtifact) -> RepoDependencyGraph {
        RepoDependencyGraph::from_artifact(&artifact)
    }

    fn base_artifact() -> RepoGraphArtifact {
        RepoGraphArtifact {
            version: REPO_GRAPH_ARTIFACT_VERSION,
            nodes: vec![
                file_node("src/lib.rs"),
                symbol_node("pkg src/lib.rs `alpha`().", "alpha"),
            ],
            edges: vec![edge(0, 1, RepoGraphEdgeKind::ContainsDefinition)],
            symbol_ranges: BTreeMap::new(),
            communities: vec![community("community-alpha", vec![0, 1])],
            processes: Vec::new(),
            route_exclusion_config: Default::default(),
            layout_positions: BTreeMap::new(),
            galaxy_positions: BTreeMap::new(),
            galaxy_degrees: BTreeMap::new(),
        }
    }

    fn default_config() -> YkcgExtractorParityConfig {
        YkcgExtractorParityConfig::new(
            "route-extractor",
            [RepoGraphNodeKind::Route],
            [RepoGraphEdgeKind::HandlesRoute, RepoGraphEdgeKind::Fetches],
        )
    }

    fn file_node(path: &str) -> RepoGraphNode {
        RepoGraphNode {
            id: RepoNodeKey::File(path.into()),
            kind: RepoGraphNodeKind::File,
            display_name: path.to_string(),
            language: Some("rust".to_string()),
            file_path: Some(path.into()),
            symbol: None,
            symbol_kind: None,
            is_external: false,
            visibility: None,
            signature: None,
            documentation: Vec::new(),
            signature_parts: None,
            is_test: false,
            complexity: None,
            workspace: Some("root".to_string()),
            route_framework: None,
            route_handler_symbol: None,
        }
    }

    fn symbol_node(symbol: &str, display_name: &str) -> RepoGraphNode {
        RepoGraphNode {
            id: RepoNodeKey::Symbol(symbol.to_string()),
            kind: RepoGraphNodeKind::Symbol,
            display_name: display_name.to_string(),
            language: Some("rust".to_string()),
            file_path: Some("src/lib.rs".into()),
            symbol: Some(symbol.to_string()),
            symbol_kind: None,
            is_external: false,
            visibility: None,
            signature: None,
            documentation: Vec::new(),
            signature_parts: None,
            is_test: false,
            complexity: None,
            workspace: Some("root".to_string()),
            route_framework: None,
            route_handler_symbol: None,
        }
    }

    fn route_node(id: &str) -> RepoGraphNode {
        RepoGraphNode {
            id: RepoNodeKey::Route(id.to_string()),
            kind: RepoGraphNodeKind::Route,
            display_name: id.to_string(),
            language: None,
            file_path: None,
            symbol: None,
            symbol_kind: None,
            is_external: false,
            visibility: None,
            signature: None,
            documentation: Vec::new(),
            signature_parts: None,
            is_test: false,
            complexity: None,
            workspace: Some("root".to_string()),
            route_framework: Some("axum".to_string()),
            route_handler_symbol: Some("pkg src/lib.rs `alpha`().".to_string()),
        }
    }

    fn process_node(id: &str) -> RepoGraphNode {
        RepoGraphNode {
            id: RepoNodeKey::Process(id.to_string()),
            kind: RepoGraphNodeKind::Process,
            display_name: id.to_string(),
            language: None,
            file_path: None,
            symbol: None,
            symbol_kind: None,
            is_external: false,
            visibility: None,
            signature: None,
            documentation: Vec::new(),
            signature_parts: None,
            is_test: false,
            complexity: None,
            workspace: Some("root".to_string()),
            route_framework: None,
            route_handler_symbol: None,
        }
    }

    fn tool_node(id: &str) -> RepoGraphNode {
        RepoGraphNode {
            id: RepoNodeKey::Tool(id.to_string()),
            kind: RepoGraphNodeKind::Tool,
            display_name: id.to_string(),
            language: None,
            file_path: None,
            symbol: None,
            symbol_kind: None,
            is_external: false,
            visibility: None,
            signature: None,
            documentation: Vec::new(),
            signature_parts: None,
            is_test: false,
            complexity: None,
            workspace: Some("root".to_string()),
            route_framework: None,
            route_handler_symbol: None,
        }
    }

    fn edge(source: usize, target: usize, kind: RepoGraphEdgeKind) -> RepoGraphArtifactEdge {
        RepoGraphArtifactEdge {
            source,
            target,
            kind,
            weight: 1.0,
            evidence_count: 1,
            confidence: 0.95,
            reason: None,
            step: None,
        }
    }

    fn community(id: &str, member_ids: Vec<usize>) -> Community {
        Community {
            id: id.to_string(),
            label: id.to_string(),
            symbol_count: member_ids.len(),
            member_ids,
            cohesion: 1.0,
            keywords: Vec::new(),
        }
    }

    #[test]
    fn ykcg_parity_accepts_exact_parity() {
        let baseline = graph_from_artifact(base_artifact());
        let live = graph_from_artifact(base_artifact());

        let report = assert_ykcg_extractor_graph_parity(&baseline, &live, &default_config())
            .expect("exact parity should pass");

        assert!(report.passed);
        assert!(report.allowed_added_nodes.is_empty());
        assert!(report.failing_diff.is_none());
    }

    #[test]
    fn ykcg_parity_reports_allowlisted_route_additions() {
        let baseline = graph_from_artifact(base_artifact());
        let mut live_artifact = base_artifact();
        live_artifact
            .nodes
            .push(route_node("GET /api/agents (axum)"));
        live_artifact
            .edges
            .push(edge(2, 1, RepoGraphEdgeKind::HandlesRoute));
        live_artifact.communities = vec![community("community-alpha", vec![0, 1, 2])];
        let live = graph_from_artifact(live_artifact);

        let report = assert_ykcg_extractor_graph_parity(&baseline, &live, &default_config())
            .expect("allowlisted route additions should pass");

        assert!(report.passed);
        assert_eq!(
            report.allowed_added_nodes[&RepoGraphNodeKind::Route].count,
            1
        );
        assert_eq!(
            report.allowed_added_edges[&RepoGraphEdgeKind::HandlesRoute].count,
            1
        );
        assert!(report.render_for_ci().contains("route-extractor"));
    }

    #[test]
    fn ykcg_parity_reports_allowlisted_process_style_additions() {
        let baseline = graph_from_artifact(base_artifact());
        let mut live_artifact = base_artifact();
        live_artifact.nodes.push(process_node("process-alpha"));
        live_artifact
            .edges
            .push(edge(2, 1, RepoGraphEdgeKind::StepInProcess));
        let live = graph_from_artifact(live_artifact);
        let config = YkcgExtractorParityConfig::new(
            "process-extractor",
            [RepoGraphNodeKind::Process],
            [RepoGraphEdgeKind::StepInProcess],
        );

        let report = assert_ykcg_extractor_graph_parity(&baseline, &live, &config)
            .expect("allowlisted process additions should pass");

        assert!(report.passed);
        assert_eq!(
            report.allowed_added_nodes[&RepoGraphNodeKind::Process].count,
            1
        );
    }

    #[test]
    fn ykcg_parity_reports_allowlisted_tool_style_additions() {
        let baseline = graph_from_artifact(base_artifact());
        let mut live_artifact = base_artifact();
        live_artifact.nodes.push(tool_node("agents.list"));
        let live = graph_from_artifact(live_artifact);
        let config =
            YkcgExtractorParityConfig::new("tool-extractor", [RepoGraphNodeKind::Tool], []);

        let report = assert_ykcg_extractor_graph_parity(&baseline, &live, &config)
            .expect("allowlisted tool additions should pass");

        assert!(report.passed);
        assert_eq!(
            report.allowed_added_nodes[&RepoGraphNodeKind::Tool].count,
            1
        );
    }

    #[test]
    fn ykcg_parity_fails_unexpected_additions() {
        let baseline = graph_from_artifact(base_artifact());
        let mut live_artifact = base_artifact();
        live_artifact
            .nodes
            .push(symbol_node("pkg src/lib.rs `beta`().", "beta"));
        let live = graph_from_artifact(live_artifact);

        let err = assert_ykcg_extractor_graph_parity(&baseline, &live, &default_config())
            .expect_err("unexpected symbol addition should fail");

        let YkcgExtractorParityError::Diff(report) = err;
        assert!(!report.passed);
        assert!(report.failing_diff.is_some());
        assert!(report.render_for_ci().contains("failed"));
    }

    #[test]
    fn ykcg_parity_fails_removals() {
        let mut baseline_artifact = base_artifact();
        baseline_artifact
            .nodes
            .push(route_node("GET /api/agents (axum)"));
        let baseline = graph_from_artifact(baseline_artifact);
        let live = graph_from_artifact(base_artifact());

        let err = assert_ykcg_extractor_graph_parity(&baseline, &live, &default_config())
            .expect_err("removing a pre-existing route should fail");

        let YkcgExtractorParityError::Diff(report) = err;
        let diff = report.failing_diff.expect("failing diff");
        assert_eq!(diff.nodes.removed_count, 1);
    }

    #[test]
    fn ykcg_parity_fails_core_community_member_drift() {
        let baseline = graph_from_artifact(base_artifact());
        let mut live_artifact = base_artifact();
        live_artifact.communities = vec![community("community-alpha", vec![0])];
        let live = graph_from_artifact(live_artifact);

        let err = assert_ykcg_extractor_graph_parity(&baseline, &live, &default_config())
            .expect_err("core community membership change should fail");

        let YkcgExtractorParityError::Diff(report) = err;
        let diff = report.failing_diff.expect("failing diff");
        assert!(
            diff.communities
                .membership_deltas
                .contains_key("community-alpha")
        );
    }
}
