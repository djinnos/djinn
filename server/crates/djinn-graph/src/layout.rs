//! Deterministic warm-time graph layout coordinates.
//!
//! This module intentionally avoids randomness and petgraph `NodeIndex`-only
//! placement. Coordinates are derived from stable node UIDs and the persisted
//! community sidecar so the same logical graph gets the same layout across
//! warm runs and cache reloads.

use std::collections::{BTreeMap, BTreeSet};

use petgraph::graph::NodeIndex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::repo_graph::RepoDependencyGraph;

const TAU: f64 = std::f64::consts::PI * 2.0;
const COMMUNITY_RING_RADIUS: f64 = 900.0;
const MEMBER_BASE_RADIUS: f64 = 120.0;
const MEMBER_RADIUS_STEP: f64 = 14.0;
const FALLBACK_RING_RADIUS: f64 = 1_800.0;

/// Stable layout coordinate for a graph node.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GraphLayoutPosition {
    pub x: f64,
    pub y: f64,
}

/// Compute deterministic node coordinates keyed by [`RepoNodeKey::stable_uid`].
///
/// Communities are placed first on a stable ring sorted by community id. Members
/// are then placed around their community center, sorted by stable UID. Nodes not
/// present in any community use a deterministic fallback ring outside the
/// community ring. Hash-derived angle offsets prevent all equal-sized groups from
/// sharing identical radial spokes while remaining fully reproducible.
pub fn derive_layout_positions(
    graph: &RepoDependencyGraph,
) -> BTreeMap<String, GraphLayoutPosition> {
    let mut positions = BTreeMap::new();
    if graph.node_count() == 0 {
        return positions;
    }

    let mut assigned = BTreeSet::new();
    let mut communities = graph.communities().to_vec();
    communities.sort_by(|left, right| left.id.cmp(&right.id));

    let community_count = communities.len().max(1);
    for (community_ord, community) in communities.iter().enumerate() {
        let center_angle = angle_for_ordinal(community_ord, community_count, &community.id);
        let center = GraphLayoutPosition {
            x: COMMUNITY_RING_RADIUS * center_angle.cos(),
            y: COMMUNITY_RING_RADIUS * center_angle.sin(),
        };

        let mut members: Vec<(String, NodeIndex)> = community
            .member_ids
            .iter()
            .filter_map(|&member_pos| {
                let idx = NodeIndex::new(member_pos);
                graph
                    .graph()
                    .node_weight(idx)
                    .map(|node| (node.stable_uid(), idx))
            })
            .collect();
        members.sort_by(|left, right| left.0.cmp(&right.0));

        let member_count = members.len().max(1);
        let member_radius = MEMBER_BASE_RADIUS + MEMBER_RADIUS_STEP * (member_count as f64).sqrt();
        for (member_ord, (uid, _idx)) in members.into_iter().enumerate() {
            let angle = angle_for_ordinal(member_ord, member_count, &uid);
            positions.insert(
                uid.clone(),
                GraphLayoutPosition {
                    x: center.x + member_radius * angle.cos(),
                    y: center.y + member_radius * angle.sin(),
                },
            );
            assigned.insert(uid);
        }
    }

    let mut fallback_nodes: Vec<String> = graph
        .graph()
        .node_weights()
        .map(|node| node.stable_uid())
        .filter(|uid| !assigned.contains(uid))
        .collect();
    fallback_nodes.sort();

    let fallback_count = fallback_nodes.len().max(1);
    let fallback_radius = FALLBACK_RING_RADIUS + 12.0 * (fallback_count as f64).sqrt();
    for (ord, uid) in fallback_nodes.into_iter().enumerate() {
        let angle = angle_for_ordinal(ord, fallback_count, &uid);
        let radial_jitter = stable_unit(&uid, b"fallback-radius") * 180.0;
        let radius = fallback_radius + radial_jitter;
        positions.insert(
            uid,
            GraphLayoutPosition {
                x: radius * angle.cos(),
                y: radius * angle.sin(),
            },
        );
    }

    positions
}

fn angle_for_ordinal(ord: usize, total: usize, stable_key: &str) -> f64 {
    let base = TAU * (ord as f64) / (total.max(1) as f64);
    let offset = (stable_unit(stable_key, b"angle") - 0.5) * (TAU / (total.max(1) as f64)) * 0.35;
    base + offset
}

fn stable_unit(value: &str, salt: &[u8]) -> f64 {
    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update(b"\0");
    hasher.update(value.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    let raw = u64::from_be_bytes(bytes);
    (raw as f64) / (u64::MAX as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo_graph::{
        REPO_GRAPH_ARTIFACT_VERSION, RepoDependencyGraph, RepoGraphArtifact, RepoGraphArtifactEdge,
        RepoGraphEdgeKind, RepoGraphNode, RepoGraphNodeKind, RepoNodeKey,
    };

    fn node(uid: &str) -> RepoGraphNode {
        RepoGraphNode {
            id: RepoNodeKey::Symbol(uid.to_string()),
            kind: RepoGraphNodeKind::Symbol,
            display_name: uid.to_string(),
            language: Some("rust".to_string()),
            file_path: None,
            symbol: Some(uid.to_string()),
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

    fn edge(source: usize, target: usize) -> RepoGraphArtifactEdge {
        RepoGraphArtifactEdge {
            source,
            target,
            kind: RepoGraphEdgeKind::SymbolReference,
            weight: 1.0,
            evidence_count: 1,
            confidence: 1.0,
            reason: None,
            step: None,
        }
    }

    fn graph_with_communities() -> RepoDependencyGraph {
        let artifact = RepoGraphArtifact {
            version: REPO_GRAPH_ARTIFACT_VERSION,
            nodes: vec![node("a"), node("b"), node("c"), node("orphan")],
            edges: vec![edge(0, 1), edge(1, 0), edge(1, 2)],
            symbol_ranges: BTreeMap::new(),
            communities: vec![crate::communities::Community {
                id: "community-alpha".to_string(),
                label: "alpha".to_string(),
                member_ids: vec![0, 1, 2],
                cohesion: 1.0,
                symbol_count: 3,
                keywords: Vec::new(),
            }],
            processes: Vec::new(),
            route_exclusion_config: Default::default(),
            layout_positions: BTreeMap::new(),
            galaxy_positions: BTreeMap::new(),
            galaxy_degrees: BTreeMap::new(),
        };
        RepoDependencyGraph::from_artifact(&artifact)
    }

    #[test]
    fn layout_derivation_is_deterministic_and_finite() {
        let graph = graph_with_communities();
        let first = derive_layout_positions(&graph);
        let second = derive_layout_positions(&graph);
        assert_eq!(first, second);
        assert_eq!(first.len(), graph.node_count());
        for position in first.values() {
            assert!(position.x.is_finite());
            assert!(position.y.is_finite());
        }
        let unique: BTreeSet<(u64, u64)> = first
            .values()
            .map(|position| (position.x.to_bits(), position.y.to_bits()))
            .collect();
        assert!(
            unique.len() > 1,
            "layout should not collapse every node to one point"
        );
    }

    #[test]
    fn community_members_are_near_their_center_and_fallbacks_are_outside() {
        let graph = graph_with_communities();
        let positions = derive_layout_positions(&graph);
        let community = &graph.communities()[0];
        let center_angle = angle_for_ordinal(0, 1, &community.id);
        let center = GraphLayoutPosition {
            x: COMMUNITY_RING_RADIUS * center_angle.cos(),
            y: COMMUNITY_RING_RADIUS * center_angle.sin(),
        };

        for member_pos in &community.member_ids {
            let uid = graph.node(NodeIndex::new(*member_pos)).stable_uid();
            let position = positions.get(&uid).expect("member position");
            let distance =
                ((position.x - center.x).powi(2) + (position.y - center.y).powi(2)).sqrt();
            assert!(
                distance < 250.0,
                "member {uid} should be placed around its community center"
            );
        }

        let orphan = positions.get("symbol:orphan").expect("fallback position");
        let orphan_distance = (orphan.x.powi(2) + orphan.y.powi(2)).sqrt();
        assert!(
            orphan_distance > 1_700.0,
            "fallback nodes should sit outside community clusters"
        );
    }
}
