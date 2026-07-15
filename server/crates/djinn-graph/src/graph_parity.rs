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
            galaxy_positions: BTreeMap::new(),
            galaxy_degrees: BTreeMap::new(),
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

    #[test]
    fn parity_accepts_repeated_warm() {
        let artifact = base_artifact();
        let blob = bincode::serialize(&artifact).expect("serialize artifact");

        assert_graph_artifact_blob_parity(&blob, &blob)
            .expect("repeated warm blob should match itself");
    }

    #[test]
    fn parity_reports_diff_when_file_added_between_warms() {
        let old_blob = bincode::serialize(&base_artifact()).expect("serialize old artifact");

        let mut new_artifact = base_artifact();
        new_artifact.nodes.push(file_node("src/added.rs"));
        let new_blob = bincode::serialize(&new_artifact).expect("serialize new artifact");

        let err = assert_graph_artifact_blob_parity(&old_blob, &new_blob)
            .expect_err("added file should produce a parity diff");
        match err {
            GraphArtifactBlobParityError::Diff(diff) => {
                assert!(
                    diff.files.added_count >= 1,
                    "expected at least one added file, got {}",
                    diff.files.added_count
                );
            }
            other => panic!("expected Diff variant, got {other:?}"),
        }
    }

    #[test]
    fn td55_equivalence_fixture_builds_matching_artifact_blobs() {
        let old_blob = crate::test_helpers::td55_equivalence_fixture_artifact_blob();
        let new_blob = crate::test_helpers::td55_equivalence_fixture_artifact_blob();
        let artifact = deserialize_repo_graph_artifact_bincode(&old_blob)
            .expect("fixture blob should deserialize through artifact compat seam");

        assert!(
            artifact.edges.iter().any(|edge| {
                edge.kind == RepoGraphEdgeKind::SymbolReference
                    && artifact.nodes[edge.source].file_path
                        != artifact.nodes[edge.target].file_path
            }),
            "fixture should include a cross-file symbol reference edge"
        );

        assert_graph_artifact_blob_parity(&old_blob, &new_blob)
            .expect("identical td55 fixture builds should match");
    }

    /// td55 primary gate: a cold/full parse+build of the fixture artifact set
    /// must produce the same serialized graph artifact bytes as a cache-reuse
    /// parse+build of the **same complete** artifact set.
    ///
    /// This is the explicit `incremental == full` equivalence check for the
    /// unchanged/cache-warm path. The cache-reuse side still parses every
    /// artifact (no changed-file-only subset); the difference is only whether
    /// the parse cache is populated/reused or bypassed.
    #[test]
    fn td55_cold_vs_cache_reuse_graph_artifact_parity() {
        // Isolated temp cache root for the cache-reuse phase — no env var
        // mutation, no shared/global cache, deterministic.
        let cache_root = crate::test_helpers::workspace_tempdir("td55-cache-reuse-root-");
        let cache_store = crate::scip_indexer::cache::ScipCacheStore::new(cache_root.path());

        // --- Phase 1: cold parse (cache reuse disabled) ---
        let cold_fixture = crate::test_helpers::td55_cache_reuse_scip_fixture();
        let cold_parsed = crate::scip_parser::parse_scip_artifacts_with_cache_store(
            &cold_fixture.artifacts,
            None,
        )
        .expect("cold parse should succeed");
        let mut cold_graph =
            crate::repo_graph::RepoDependencyGraph::try_build_with_source(&cold_parsed, None)
                .expect("cold graph build should succeed");
        cold_graph.set_layout_positions(crate::layout::derive_layout_positions(&cold_graph));
        let cold_blob = bincode::serialize(&cold_graph.to_artifact())
            .expect("serialize cold graph artifact blob");

        // --- Phase 2: cache-reuse parse of the same complete artifact set ---
        // A fresh fixture is built so temp file paths differ, but the SCIP
        // content is byte-identical — the cache key is content-derived so the
        // cache store recognizes/uses/repopulates identically.
        let reuse_fixture = crate::test_helpers::td55_cache_reuse_scip_fixture();
        // First call with cache: populates the store (cold misses → stores).
        let _ = crate::scip_parser::parse_scip_artifacts_with_cache_store(
            &reuse_fixture.artifacts,
            Some(&cache_store),
        )
        .expect("cache-reuse parse (population) should succeed");
        // Second call with cache: exercises the hit path (reuses stored bytes).
        let reuse_parsed = crate::scip_parser::parse_scip_artifacts_with_cache_store(
            &reuse_fixture.artifacts,
            Some(&cache_store),
        )
        .expect("cache-reuse parse (hit) should succeed");
        let mut reuse_graph =
            crate::repo_graph::RepoDependencyGraph::try_build_with_source(&reuse_parsed, None)
                .expect("cache-reuse graph build should succeed");
        reuse_graph.set_layout_positions(crate::layout::derive_layout_positions(&reuse_graph));
        let reuse_blob = bincode::serialize(&reuse_graph.to_artifact())
            .expect("serialize cache-reuse graph artifact blob");

        // Sanity: both graphs have at least one cross-file edge so a
        // changed-file-only path that drops cross-file edges would be caught.
        // The cross-file signal includes `FileReference` (file→file/symbol)
        // and `SymbolReference`/`Reads`/`Writes` (symbol→file) edges that
        // span partitions.
        for (label, blob) in [("cold", &cold_blob), ("cache-reuse", &reuse_blob)] {
            let artifact = deserialize_repo_graph_artifact_bincode(blob)
                .unwrap_or_else(|err| panic!("{label} blob should deserialize: {err}"));
            let is_cross_file_ref = |edge: &RepoGraphArtifactEdge| {
                matches!(
                    edge.kind,
                    RepoGraphEdgeKind::SymbolReference
                        | RepoGraphEdgeKind::Reads
                        | RepoGraphEdgeKind::Writes
                        | RepoGraphEdgeKind::FileReference
                ) && artifact.nodes[edge.source].file_path != artifact.nodes[edge.target].file_path
            };
            assert!(
                artifact.edges.iter().any(is_cross_file_ref),
                "{label} fixture graph should include a cross-file reference edge"
            );
        }

        // The essential gate: artifact-level parity through the shared helper.
        assert_graph_artifact_blob_parity(&cold_blob, &reuse_blob)
            .expect("cold and cache-reuse graph artifact bytes should be equivalent");
    }

    /// td55 one-partition-changed incremental-shaped gate: after a cache-
    /// populating full run, change one fixture partition, replay unchanged
    /// partitions from cache, rebuild the full current graph, and prove
    /// parity against a full current rebuild.
    ///
    /// This guards against the unsafe changed-file-only graph shape that
    /// would drop unchanged partitions from the final artifact set.
    #[test]
    fn td55_one_partition_changed_incremental_shaped_graph_artifact_parity() {
        let cache_root = crate::test_helpers::workspace_tempdir("td55-incremental-root-");
        let cache_store = crate::scip_indexer::cache::ScipCacheStore::new(cache_root.path());

        // --- Phase 1: cold parse of the original fixture populates cache ---
        let cold_fixture = crate::test_helpers::td55_cache_reuse_scip_fixture();
        let cold_parsed = crate::scip_parser::parse_scip_artifacts_with_cache_store(
            &cold_fixture.artifacts,
            Some(&cache_store),
        )
        .expect("cold parse should succeed");
        let mut cold_graph =
            crate::repo_graph::RepoDependencyGraph::try_build_with_source(&cold_parsed, None)
                .expect("cold graph build should succeed");
        cold_graph.set_layout_positions(crate::layout::derive_layout_positions(&cold_graph));
        let _cold_blob = bincode::serialize(&cold_graph.to_artifact())
            .expect("serialize cold graph artifact blob");

        // --- Phase 2: incremental-shaped parse with one changed partition ---
        // The changed fixture has a different app partition (extra symbol).
        // The domain partition is byte-identical to the cold fixture, so it
        // should hit the parse cache. The app partition is changed, so it
        // should miss cache and be parsed fresh.
        let changed_fixture = crate::test_helpers::td55_incremental_scip_fixture("app");
        let incremental_parsed = crate::scip_parser::parse_scip_artifacts_with_cache_store(
            &changed_fixture.artifacts,
            Some(&cache_store),
        )
        .expect("incremental parse should succeed");
        let mut incremental_graph = crate::repo_graph::RepoDependencyGraph::try_build_with_source(
            &incremental_parsed,
            None,
        )
        .expect("incremental graph build should succeed");
        incremental_graph
            .set_layout_positions(crate::layout::derive_layout_positions(&incremental_graph));
        let incremental_blob = bincode::serialize(&incremental_graph.to_artifact())
            .expect("serialize incremental graph artifact blob");

        // --- Phase 3: full current rebuild (cache disabled) of changed fixture ---
        let full_fixture = crate::test_helpers::td55_incremental_scip_fixture("app");
        let full_parsed = crate::scip_parser::parse_scip_artifacts_with_cache_store(
            &full_fixture.artifacts,
            None,
        )
        .expect("full parse should succeed");
        let mut full_graph =
            crate::repo_graph::RepoDependencyGraph::try_build_with_source(&full_parsed, None)
                .expect("full graph build should succeed");
        full_graph.set_layout_positions(crate::layout::derive_layout_positions(&full_graph));
        let full_blob = bincode::serialize(&full_graph.to_artifact())
            .expect("serialize full graph artifact blob");

        // --- Sanity: both graphs have at least one cross-file edge ---
        for (label, blob) in [("incremental", &incremental_blob), ("full", &full_blob)] {
            let artifact = deserialize_repo_graph_artifact_bincode(blob)
                .unwrap_or_else(|err| panic!("{label} blob should deserialize: {err}"));
            let is_cross_file_ref = |edge: &RepoGraphArtifactEdge| {
                matches!(
                    edge.kind,
                    RepoGraphEdgeKind::SymbolReference
                        | RepoGraphEdgeKind::Reads
                        | RepoGraphEdgeKind::Writes
                        | RepoGraphEdgeKind::FileReference
                ) && artifact.nodes[edge.source].file_path != artifact.nodes[edge.target].file_path
            };
            assert!(
                artifact.edges.iter().any(is_cross_file_ref),
                "{label} fixture graph should include a cross-file reference edge"
            );
        }

        // --- Explicit guard: the incremental-shaped build consumed ALL
        // current partitions/files, not only the changed one. ---
        let incremental_artifact = deserialize_repo_graph_artifact_bincode(&incremental_blob)
            .expect("incremental blob should deserialize");
        let workspaces_in_graph: std::collections::BTreeSet<String> = incremental_artifact
            .nodes
            .iter()
            .filter_map(|n| n.workspace.clone())
            .collect();
        assert!(
            workspaces_in_graph.contains("app"),
            "incremental graph must contain the changed app partition"
        );
        assert!(
            workspaces_in_graph.contains("domain"),
            "incremental graph must contain the unchanged domain partition; \
             a changed-file-only path would drop it"
        );

        // --- The essential gate: artifact-level parity ---
        assert_graph_artifact_blob_parity(&full_blob, &incremental_blob).expect(
            "one-partition-changed incremental-shaped build must match full current rebuild",
        );
    }

    /// td55 negative gate: prove the changed-file-only graph shape would fail
    /// parity by deliberately dropping an unchanged partition from the artifact
    /// set and asserting the parity check catches the difference.
    #[test]
    fn td55_changed_file_only_shape_fails_parity() {
        let cache_root = crate::test_helpers::workspace_tempdir("td55-changed-only-root-");
        let cache_store = crate::scip_indexer::cache::ScipCacheStore::new(cache_root.path());

        // Populate cache with original fixture
        let original_fixture = crate::test_helpers::td55_cache_reuse_scip_fixture();
        let _ = crate::scip_parser::parse_scip_artifacts_with_cache_store(
            &original_fixture.artifacts,
            Some(&cache_store),
        )
        .expect("populate cache");

        // Build full graph from changed fixture (both partitions)
        let full_fixture = crate::test_helpers::td55_incremental_scip_fixture("app");
        let full_parsed = crate::scip_parser::parse_scip_artifacts_with_cache_store(
            &full_fixture.artifacts,
            None,
        )
        .expect("full parse");
        let mut full_graph =
            crate::repo_graph::RepoDependencyGraph::try_build_with_source(&full_parsed, None)
                .expect("full graph build");
        full_graph.set_layout_positions(crate::layout::derive_layout_positions(&full_graph));
        let full_blob = bincode::serialize(&full_graph.to_artifact()).expect("serialize full");

        // Build changed-file-only graph (only the changed app partition)
        let changed_only_artifacts: Vec<_> = full_fixture
            .artifacts
            .iter()
            .filter(|a| a.workspace_slug == "app")
            .cloned()
            .collect();
        let changed_only_parsed = crate::scip_parser::parse_scip_artifacts_with_cache_store(
            &changed_only_artifacts,
            None,
        )
        .expect("changed-only parse");
        let mut changed_only_graph = crate::repo_graph::RepoDependencyGraph::try_build_with_source(
            &changed_only_parsed,
            None,
        )
        .expect("changed-only graph build");
        changed_only_graph
            .set_layout_positions(crate::layout::derive_layout_positions(&changed_only_graph));
        let changed_only_blob =
            bincode::serialize(&changed_only_graph.to_artifact()).expect("serialize changed-only");

        // Parity must fail — proves the gate catches the unsafe shape.
        let err = assert_graph_artifact_blob_parity(&full_blob, &changed_only_blob)
            .expect_err("changed-file-only graph must fail parity against full graph");
        assert!(
            matches!(err, GraphArtifactBlobParityError::Diff(_)),
            "expected Diff variant, got {err:?}"
        );
    }

    /// td55 canonical warm cache-reuse toggle regression.
    ///
    /// This is a focused structure guard proving the production canonical
    /// warm cache-reuse env toggle (`DJINN_GRAPH_CACHE_REUSE_ENABLED` /
    /// `DJINN_CACHE_REUSE_ENABLED` / `CACHE_REUSE_ENABLED`) routes through
    /// [`crate::canonical_graph::resolve_canonical_warm_cache_reuse`] into
    /// [`crate::scip_parser::parse_scip_artifacts_with_cache_reuse`] — the
    /// SAME whole-artifact parse/build seam exercised by the td55 parity
    /// tests above. The goal is to prevent future code from bypassing the
    /// equivalence gate with a separate partial/incremental graph assembly
    /// path.
    ///
    /// Cache reuse is allowed ONLY as an input/artifact reuse optimization
    /// before the whole-graph build, NOT as changed-file-only graph
    /// resolution. This test verifies that the cache-reuse-enabled path:
    /// 1. parses the COMPLETE artifact set (no changed-file-only subset);
    /// 2. produces a graph artifact that passes
    ///    [`assert_graph_artifact_blob_parity`] against a cold/full build
    ///    of the same complete set;
    /// 3. uses an isolated temp cache root (no DB, no K8s, no Docker, no
    ///    real SCIP binaries, no network, no credentials).
    #[test]
    fn canonical_warm_cache_reuse_toggle_reaches_parity_seam() {
        // Serialize against the canonical-graph cache-reuse warm test, which
        // mutates the same `*CACHE_REUSE_ENABLED` toggles and
        // `DJINN_SCIP_CACHE_DIR` on Cargo's shared test threads.
        let _env_lock = crate::test_helpers::lock_pipeline_env();
        // SAFETY: this test manipulates env vars for the duration of the
        // test. `std::env::set_var` / `remove_var` are marked unsafe in
        // Rust 2024 because they race with concurrent getenv readers. This
        // test does not spawn threads and the env mutation is local to this
        // single-threaded unit test body, so the access is serialized.
        // Save and restore the env state to avoid leaking into other tests.
        let saved = [
            "DJINN_GRAPH_CACHE_REUSE_ENABLED",
            "DJINN_CACHE_REUSE_ENABLED",
            "CACHE_REUSE_ENABLED",
        ]
        .map(|name| (name, std::env::var(name).ok()));
        for &name in [
            "DJINN_GRAPH_CACHE_REUSE_ENABLED",
            "DJINN_CACHE_REUSE_ENABLED",
            "CACHE_REUSE_ENABLED",
        ]
        .iter()
        {
            unsafe {
                std::env::remove_var(name);
            }
        }
        let restore = || {
            for (name, value) in &saved {
                unsafe {
                    if let Some(v) = value {
                        std::env::set_var(name, v);
                    } else {
                        std::env::remove_var(name);
                    }
                }
            }
        };

        // --- Structure guard 1: toggle resolves to the same seam ---
        //
        // With the env toggle enabled, the canonical warm resolution must
        // report cache reuse as active (unless a stale sentinel forces a
        // full rebuild). This proves the toggle is wired to
        // `resolve_canonical_warm_cache_reuse`.
        unsafe {
            std::env::set_var("DJINN_GRAPH_CACHE_REUSE_ENABLED", "1");
        }
        assert!(
            crate::canonical_graph::resolve_canonical_warm_cache_reuse(false),
            "cache-reuse env toggle must resolve to enabled when force_full_rebuild=false"
        );
        // Force-full-rebuild (stale sentinel) must disable cache reuse even
        // when the toggle is on — this is the safety rail, not a changed-
        // file-only bypass.
        assert!(
            !crate::canonical_graph::resolve_canonical_warm_cache_reuse(true),
            "stale-sentinel forced full rebuild must disable cache reuse"
        );

        // With the toggle off, cache reuse must be disabled.
        unsafe {
            std::env::set_var("DJINN_GRAPH_CACHE_REUSE_ENABLED", "0");
        }
        assert!(
            !crate::canonical_graph::resolve_canonical_warm_cache_reuse(false),
            "cache-reuse disabled env value must resolve to disabled"
        );

        // Also verify the alias env names work.
        unsafe {
            std::env::remove_var("DJINN_GRAPH_CACHE_REUSE_ENABLED");
            std::env::set_var("DJINN_CACHE_REUSE_ENABLED", "on");
        }
        assert!(
            crate::canonical_graph::resolve_canonical_warm_cache_reuse(false),
            "DJINN_CACHE_REUSE_ENABLED alias must be honored"
        );

        unsafe {
            std::env::remove_var("DJINN_CACHE_REUSE_ENABLED");
            std::env::set_var("CACHE_REUSE_ENABLED", "true");
        }
        assert!(
            crate::canonical_graph::resolve_canonical_warm_cache_reuse(false),
            "CACHE_REUSE_ENABLED alias must be honored"
        );

        // Restore env for the parity-build portion: enable the toggle so the
        // path under test matches production cache-reuse-on behavior.
        unsafe {
            std::env::set_var("DJINN_GRAPH_CACHE_REUSE_ENABLED", "1");
        }

        // --- Structure guard 2: the enabled toggle routes through the same
        // whole-artifact parse/build seam as the td55 parity tests ---
        //
        // `resolve_canonical_warm_cache_reuse(true_value)` returns the same
        // boolean that `ensure_canonical_graph` passes to
        // `parse_scip_artifacts_with_cache_reuse`. When true, that function
        // constructs a `ScipCacheStore::from_environment()`. We isolate the
        // cache root via `DJINN_SCIP_CACHE_DIR` so the test is deterministic
        // and does not touch the real cache.
        //
        // SAFETY: isolated single-threaded env mutation — no threads spawned.
        let cache_root = crate::test_helpers::workspace_tempdir("td55-toggle-seam-cache-");
        let saved_cache_dir = std::env::var("DJINN_SCIP_CACHE_DIR").ok();
        unsafe {
            std::env::set_var("DJINN_SCIP_CACHE_DIR", cache_root.path());
        }

        // Cold build of the COMPLETE fixture set (no cache).
        let cold_fixture = crate::test_helpers::td55_cache_reuse_scip_fixture();
        let cold_parsed = crate::scip_parser::parse_scip_artifacts_with_cache_store(
            &cold_fixture.artifacts,
            None,
        )
        .expect("cold parse should succeed");
        let mut cold_graph =
            crate::repo_graph::RepoDependencyGraph::try_build_with_source(&cold_parsed, None)
                .expect("cold graph build should succeed");
        cold_graph.set_layout_positions(crate::layout::derive_layout_positions(&cold_graph));
        let cold_blob = bincode::serialize(&cold_graph.to_artifact())
            .expect("serialize cold graph artifact blob");

        // Cache-reuse-enabled build: use the SAME seam as
        // `ensure_canonical_graph` (resolve_canonical_warm_cache_reuse →
        // parse_scip_artifacts_with_cache_reuse) with the COMPLETE artifact
        // set. A fresh fixture ensures temp file paths differ but SCIP
        // content is byte-identical, so the content-addressed cache key
        // matches after the first population call.
        let reuse_fixture = crate::test_helpers::td55_cache_reuse_scip_fixture();
        // Population pass — fills the cache store.
        let _ = crate::scip_parser::parse_scip_artifacts_with_cache_reuse(
            &reuse_fixture.artifacts,
            crate::canonical_graph::resolve_canonical_warm_cache_reuse(false),
        )
        .expect("cache-reuse parse (population) should succeed");
        // Hit pass — exercises the reuse path through the same seam.
        let reuse_parsed = crate::scip_parser::parse_scip_artifacts_with_cache_reuse(
            &reuse_fixture.artifacts,
            crate::canonical_graph::resolve_canonical_warm_cache_reuse(false),
        )
        .expect("cache-reuse parse (hit) should succeed");
        let mut reuse_graph =
            crate::repo_graph::RepoDependencyGraph::try_build_with_source(&reuse_parsed, None)
                .expect("cache-reuse graph build should succeed");
        reuse_graph.set_layout_positions(crate::layout::derive_layout_positions(&reuse_graph));
        let reuse_blob = bincode::serialize(&reuse_graph.to_artifact())
            .expect("serialize cache-reuse graph artifact blob");

        // Both graphs must include a cross-file edge — a changed-file-only
        // path would drop it, and the parity check would catch that.
        for (label, blob) in [("cold", &cold_blob), ("cache-reuse", &reuse_blob)] {
            let artifact = deserialize_repo_graph_artifact_bincode(blob)
                .unwrap_or_else(|err| panic!("{label} blob should deserialize: {err}"));
            let is_cross_file_ref = |edge: &RepoGraphArtifactEdge| {
                matches!(
                    edge.kind,
                    RepoGraphEdgeKind::SymbolReference
                        | RepoGraphEdgeKind::Reads
                        | RepoGraphEdgeKind::Writes
                        | RepoGraphEdgeKind::FileReference
                ) && artifact.nodes[edge.source].file_path != artifact.nodes[edge.target].file_path
            };
            assert!(
                artifact.edges.iter().any(is_cross_file_ref),
                "{label} fixture graph should include a cross-file reference edge"
            );
        }

        // The essential gate: the toggle-enabled path produces a graph
        // artifact that passes artifact parity against the cold/full build.
        // This proves the toggle routes through the same whole-artifact
        // build seam — a separate partial/incremental assembly path would
        // fail here.
        assert_graph_artifact_blob_parity(&cold_blob, &reuse_blob).expect(
            "canonical warm cache-reuse toggle must produce a graph equivalent to the cold/full build",
        );

        // Restore all env state.
        unsafe {
            if let Some(v) = saved_cache_dir {
                std::env::set_var("DJINN_SCIP_CACHE_DIR", v);
            } else {
                std::env::remove_var("DJINN_SCIP_CACHE_DIR");
            }
        }
        restore();
    }
}
