//! Reusable parity diffing for repository dependency graphs.
//!
//! The helpers in this module compare normalized graph identities rather than
//! petgraph node indices so callers can use them across independent graph
//! builds and cache-artifact round trips.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use petgraph::visit::EdgeRef;
use serde::{Deserialize, Serialize};

use crate::communities::Community;
use crate::repo_graph::{
    RepoDependencyGraph, RepoGraphEdgeKind, RepoGraphNodeKind,
    deserialize_repo_graph_artifact_bincode,
};

const DEFAULT_SAMPLE_LIMIT: usize = 20;

/// Compare two in-memory repo dependency graphs for normalized parity.
///
/// Returns `Ok(())` when files, nodes, edges, and community memberships match.
/// Otherwise returns a structured [`GraphParityDiff`] with bounded samples.
pub fn assert_graph_parity(
    old: &RepoDependencyGraph,
    new: &RepoDependencyGraph,
) -> Result<(), GraphParityDiff> {
    let diff = diff_graph_parity(old, new, DEFAULT_SAMPLE_LIMIT);
    if diff.is_empty() { Ok(()) } else { Err(diff) }
}

/// Build a structured graph parity diff with a caller-provided sample bound.
///
/// This is the reusable API used by rollout-specific adapters that need to
/// classify a subset of additions without changing the core graph parity
/// semantics.
pub fn diff_graph_parity(
    old: &RepoDependencyGraph,
    new: &RepoDependencyGraph,
    sample_limit: usize,
) -> GraphParityDiff {
    let old_snapshot = NormalizedGraph::from_graph(old);
    let new_snapshot = NormalizedGraph::from_graph(new);
    GraphParityDiff::between(&old_snapshot, &new_snapshot, sample_limit)
}

/// Compare two serialized repo-graph artifact blobs for normalized parity.
///
/// This intentionally deserializes through
/// [`deserialize_repo_graph_artifact_bincode`] rather than raw bincode so the
/// additive v10 compatibility shims remain honored on parity paths.
pub fn assert_graph_artifact_blob_parity(
    old_blob: &[u8],
    new_blob: &[u8],
) -> Result<(), GraphArtifactBlobParityError> {
    let old_artifact = deserialize_repo_graph_artifact_bincode(old_blob)
        .map_err(GraphArtifactBlobParityError::DeserializeOld)?;
    let new_artifact = deserialize_repo_graph_artifact_bincode(new_blob)
        .map_err(GraphArtifactBlobParityError::DeserializeNew)?;
    let old_graph = RepoDependencyGraph::from_artifact(&old_artifact);
    let new_graph = RepoDependencyGraph::from_artifact(&new_artifact);
    assert_graph_parity(&old_graph, &new_graph).map_err(GraphArtifactBlobParityError::Diff)
}

/// Error returned by [`assert_graph_artifact_blob_parity`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphArtifactBlobParityError {
    DeserializeOld(String),
    DeserializeNew(String),
    Diff(GraphParityDiff),
}

impl fmt::Display for GraphArtifactBlobParityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeserializeOld(err) => write!(f, "deserialize old graph artifact: {err}"),
            Self::DeserializeNew(err) => write!(f, "deserialize new graph artifact: {err}"),
            Self::Diff(diff) => write!(f, "graph parity diff: {diff:?}"),
        }
    }
}

impl std::error::Error for GraphArtifactBlobParityError {}

/// Structured diff for graph parity comparisons.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphParityDiff {
    pub files: Box<SetParityDiff>,
    pub nodes: Box<KindedSetParityDiff<RepoGraphNodeKind>>,
    pub edges: Box<KindedSetParityDiff<RepoGraphEdgeKind>>,
    pub communities: Box<CommunityParityDiff>,
}

impl GraphParityDiff {
    fn between(old: &NormalizedGraph, new: &NormalizedGraph, sample_limit: usize) -> Self {
        Self {
            files: Box::new(SetParityDiff::between(&old.files, &new.files, sample_limit)),
            nodes: Box::new(KindedSetParityDiff::between(
                &old.nodes,
                &new.nodes,
                &old.node_kind_by_id,
                &new.node_kind_by_id,
                sample_limit,
            )),
            edges: Box::new(KindedSetParityDiff::between(
                &old.edges,
                &new.edges,
                &old.edge_kind_by_id,
                &new.edge_kind_by_id,
                sample_limit,
            )),
            communities: Box::new(CommunityParityDiff::between(old, new, sample_limit)),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
            && self.nodes.is_empty()
            && self.edges.is_empty()
            && self.communities.is_empty()
    }
}

/// Added/removed set diff with total counts and bounded samples.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetParityDiff {
    pub old_total: usize,
    pub new_total: usize,
    pub added_count: usize,
    pub removed_count: usize,
    pub added_samples: Vec<String>,
    pub removed_samples: Vec<String>,
}

impl SetParityDiff {
    fn between(old: &BTreeSet<String>, new: &BTreeSet<String>, sample_limit: usize) -> Self {
        let added: Vec<String> = new.difference(old).cloned().collect();
        let removed: Vec<String> = old.difference(new).cloned().collect();
        Self {
            old_total: old.len(),
            new_total: new.len(),
            added_count: added.len(),
            removed_count: removed.len(),
            added_samples: sample(added, sample_limit),
            removed_samples: sample(removed, sample_limit),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.added_count == 0 && self.removed_count == 0
    }
}

/// Added/removed set diff plus counts grouped by node/edge kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindedSetParityDiff<K>
where
    K: Ord,
{
    pub old_total: usize,
    pub new_total: usize,
    pub old_counts_by_kind: BTreeMap<K, usize>,
    pub new_counts_by_kind: BTreeMap<K, usize>,
    pub added_counts_by_kind: BTreeMap<K, usize>,
    pub removed_counts_by_kind: BTreeMap<K, usize>,
    pub added_count: usize,
    pub removed_count: usize,
    pub added_samples: Vec<String>,
    pub removed_samples: Vec<String>,
    pub added_samples_by_kind: BTreeMap<K, Vec<String>>,
    pub removed_samples_by_kind: BTreeMap<K, Vec<String>>,
}

impl<K> KindedSetParityDiff<K>
where
    K: Clone + Ord,
{
    fn between(
        old: &BTreeSet<String>,
        new: &BTreeSet<String>,
        old_kind_by_id: &BTreeMap<String, K>,
        new_kind_by_id: &BTreeMap<String, K>,
        sample_limit: usize,
    ) -> Self {
        let added: Vec<String> = new.difference(old).cloned().collect();
        let removed: Vec<String> = old.difference(new).cloned().collect();
        let added_counts_by_kind = delta_counts_by_kind(&added, new_kind_by_id);
        let removed_counts_by_kind = delta_counts_by_kind(&removed, old_kind_by_id);
        let added_samples_by_kind = samples_by_kind(&added, new_kind_by_id, sample_limit);
        let removed_samples_by_kind = samples_by_kind(&removed, old_kind_by_id, sample_limit);
        Self {
            old_total: old.len(),
            new_total: new.len(),
            old_counts_by_kind: counts_by_kind(old_kind_by_id),
            new_counts_by_kind: counts_by_kind(new_kind_by_id),
            added_counts_by_kind,
            removed_counts_by_kind,
            added_count: added.len(),
            removed_count: removed.len(),
            added_samples: sample(added, sample_limit),
            removed_samples: sample(removed, sample_limit),
            added_samples_by_kind,
            removed_samples_by_kind,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.added_count == 0
            && self.removed_count == 0
            && self.old_counts_by_kind == self.new_counts_by_kind
    }
}

/// Community sidecar diff keyed by existing stable community ids.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunityParityDiff {
    pub old_total: usize,
    pub new_total: usize,
    pub added_count: usize,
    pub removed_count: usize,
    pub added_samples: Vec<String>,
    pub removed_samples: Vec<String>,
    pub membership_deltas: BTreeMap<String, CommunityMembershipDelta>,
}

impl CommunityParityDiff {
    fn between(old: &NormalizedGraph, new: &NormalizedGraph, sample_limit: usize) -> Self {
        let old_ids: BTreeSet<String> = old.community_members_by_id.keys().cloned().collect();
        let new_ids: BTreeSet<String> = new.community_members_by_id.keys().cloned().collect();
        let added: Vec<String> = new_ids.difference(&old_ids).cloned().collect();
        let removed: Vec<String> = old_ids.difference(&new_ids).cloned().collect();
        let mut membership_deltas = BTreeMap::new();
        for community_id in old_ids.intersection(&new_ids) {
            let old_members = &old.community_members_by_id[community_id];
            let new_members = &new.community_members_by_id[community_id];
            let delta = CommunityMembershipDelta::between(old_members, new_members, sample_limit);
            if !delta.is_empty() {
                membership_deltas.insert(community_id.clone(), delta);
            }
        }
        Self {
            old_total: old_ids.len(),
            new_total: new_ids.len(),
            added_count: added.len(),
            removed_count: removed.len(),
            added_samples: sample(added, sample_limit),
            removed_samples: sample(removed, sample_limit),
            membership_deltas,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.added_count == 0 && self.removed_count == 0 && self.membership_deltas.is_empty()
    }
}

/// Added/removed members for one stable community id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunityMembershipDelta {
    pub old_total: usize,
    pub new_total: usize,
    pub added_count: usize,
    pub removed_count: usize,
    pub added_samples: Vec<String>,
    pub removed_samples: Vec<String>,
}

impl CommunityMembershipDelta {
    fn between(old: &BTreeSet<String>, new: &BTreeSet<String>, sample_limit: usize) -> Self {
        let set = SetParityDiff::between(old, new, sample_limit);
        Self {
            old_total: set.old_total,
            new_total: set.new_total,
            added_count: set.added_count,
            removed_count: set.removed_count,
            added_samples: set.added_samples,
            removed_samples: set.removed_samples,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.added_count == 0 && self.removed_count == 0
    }
}

#[derive(Debug)]
struct NormalizedGraph {
    files: BTreeSet<String>,
    nodes: BTreeSet<String>,
    node_kind_by_id: BTreeMap<String, RepoGraphNodeKind>,
    edges: BTreeSet<String>,
    edge_kind_by_id: BTreeMap<String, RepoGraphEdgeKind>,
    community_members_by_id: BTreeMap<String, BTreeSet<String>>,
}

impl NormalizedGraph {
    fn from_graph(graph: &RepoDependencyGraph) -> Self {
        let artifact = graph.to_artifact();
        let mut files = BTreeSet::new();
        let mut nodes = BTreeSet::new();
        let mut node_kind_by_id = BTreeMap::new();
        let mut node_uid_by_index = Vec::with_capacity(artifact.nodes.len());

        for (index, node) in artifact.nodes.iter().enumerate() {
            let uid = node.stable_uid();
            if node.kind == RepoGraphNodeKind::File {
                files.insert(uid.clone());
            }
            nodes.insert(uid.clone());
            node_kind_by_id.insert(uid.clone(), node.kind);
            if node_uid_by_index.len() == index {
                node_uid_by_index.push(uid);
            }
        }

        for file in artifact.symbol_ranges.keys() {
            files.insert(format!("file:{}", file.display()));
        }

        let mut edges = BTreeSet::new();
        let mut edge_kind_by_id = BTreeMap::new();
        for edge_ref in graph.graph().edge_references() {
            let source = graph.node(edge_ref.source()).stable_uid();
            let target = graph.node(edge_ref.target()).stable_uid();
            let edge = edge_ref.weight();
            let id = format!(
                "{} -{:?}[step={:?}]-> {}",
                source, edge.kind, edge.step, target
            );
            edges.insert(id.clone());
            edge_kind_by_id.insert(id, edge.kind);
        }

        let community_members_by_id =
            normalize_communities(&artifact.communities, &node_uid_by_index);

        Self {
            files,
            nodes,
            node_kind_by_id,
            edges,
            edge_kind_by_id,
            community_members_by_id,
        }
    }
}

fn normalize_communities(
    communities: &[Community],
    node_uid_by_index: &[String],
) -> BTreeMap<String, BTreeSet<String>> {
    let mut out = BTreeMap::new();
    for community in communities {
        let members = community
            .member_ids
            .iter()
            .filter_map(|&index| node_uid_by_index.get(index).cloned())
            .collect();
        out.insert(community.id.clone(), members);
    }
    out
}

fn counts_by_kind<K>(kind_by_id: &BTreeMap<String, K>) -> BTreeMap<K, usize>
where
    K: Clone + Ord,
{
    let mut counts = BTreeMap::new();
    for kind in kind_by_id.values() {
        *counts.entry(kind.clone()).or_default() += 1;
    }
    counts
}

fn delta_counts_by_kind<K>(ids: &[String], kind_by_id: &BTreeMap<String, K>) -> BTreeMap<K, usize>
where
    K: Clone + Ord,
{
    let mut counts = BTreeMap::new();
    for id in ids {
        if let Some(kind) = kind_by_id.get(id) {
            *counts.entry(kind.clone()).or_default() += 1;
        }
    }
    counts
}

fn samples_by_kind<K>(
    ids: &[String],
    kind_by_id: &BTreeMap<String, K>,
    sample_limit: usize,
) -> BTreeMap<K, Vec<String>>
where
    K: Clone + Ord,
{
    let mut samples: BTreeMap<K, Vec<String>> = BTreeMap::new();
    for id in ids {
        if let Some(kind) = kind_by_id.get(id) {
            let values = samples.entry(kind.clone()).or_default();
            if values.len() < sample_limit {
                values.push(id.clone());
            }
        }
    }
    samples
}

fn sample(mut values: Vec<String>, sample_limit: usize) -> Vec<String> {
    values.truncate(sample_limit);
    values
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
        }
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
    fn graph_parity_accepts_exact_match() {
        let old = graph_from_artifact(base_artifact());
        let new = graph_from_artifact(base_artifact());

        assert_graph_parity(&old, &new).expect("identical graphs should match");
    }

    #[test]
    fn graph_parity_reports_added_and_removed_files_and_nodes() {
        let old = graph_from_artifact(base_artifact());
        let mut new_artifact = base_artifact();
        new_artifact.nodes.push(file_node("src/extra.rs"));
        let new = graph_from_artifact(new_artifact);

        let diff = assert_graph_parity(&old, &new).expect_err("extra node should differ");
        assert_eq!(diff.files.added_count, 1);
        assert_eq!(diff.nodes.added_count, 1);
        assert_eq!(
            diff.nodes.new_counts_by_kind.get(&RepoGraphNodeKind::File),
            Some(&2)
        );
        assert!(
            diff.files
                .added_samples
                .contains(&"file:src/extra.rs".to_string())
        );
    }

    #[test]
    fn graph_parity_reports_edge_and_edge_kind_count_deltas() {
        let old = graph_from_artifact(base_artifact());
        let mut new_artifact = base_artifact();
        new_artifact
            .edges
            .push(edge(1, 0, RepoGraphEdgeKind::DeclaredInFile));
        let new = graph_from_artifact(new_artifact);

        let diff = assert_graph_parity(&old, &new).expect_err("extra edge should differ");
        assert_eq!(diff.edges.added_count, 1);
        assert_eq!(
            diff.edges
                .new_counts_by_kind
                .get(&RepoGraphEdgeKind::DeclaredInFile),
            Some(&1)
        );
        assert!(
            diff.edges
                .added_samples
                .iter()
                .any(|sample| sample.contains("DeclaredInFile"))
        );
    }

    #[test]
    fn graph_parity_reports_community_membership_changes_by_stable_id() {
        let old = graph_from_artifact(base_artifact());
        let mut new_artifact = base_artifact();
        new_artifact
            .nodes
            .push(symbol_node("pkg src/lib.rs `beta`().", "beta"));
        new_artifact.communities = vec![community("community-alpha", vec![0, 1, 2])];
        let new = graph_from_artifact(new_artifact);

        let diff = assert_graph_parity(&old, &new).expect_err("membership should differ");
        let delta = diff
            .communities
            .membership_deltas
            .get("community-alpha")
            .expect("stable community id delta");
        assert_eq!(delta.added_count, 1);
        assert!(
            delta
                .added_samples
                .contains(&"symbol:pkg src/lib.rs `beta`().".to_string())
        );
    }

    #[test]
    fn graph_artifact_blob_parity_uses_compat_deserializer_path() {
        let artifact = base_artifact();
        let blob = bincode::serialize(&artifact).expect("serialize artifact");

        assert_graph_artifact_blob_parity(&blob, &blob).expect("matching blobs should match");
    }
}
