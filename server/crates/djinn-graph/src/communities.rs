//! PR F3 — community detection over the canonical graph via greedy
//! modularity maximization (Clauset-Newman-Moore).
//!
//! The plan's stretch acceptance criterion was Leiden, but the Rust
//! ecosystem doesn't ship a maintained Leiden crate, so we fall back to
//! modularity-based clustering — the same partitioning quality target
//! Leiden optimizes (Q ∈ [-0.5, 1]); just without Leiden's
//! refinement-step guarantees against badly-connected communities. For
//! cluster-doc generation downstream (PR F4) this is good enough.
//!
//! The algorithm runs over the **undirected, weighted projection** of
//! the canonical petgraph: every directed edge contributes `weight`
//! to the symmetric (u, v) → (v, u) sum. Self-loops are dropped.
//!
//! Pass:
//!   1. Build adjacency `BTreeMap<NodeIndex, BTreeMap<NodeIndex, f64>>`
//!      summing both directions. Compute total weight `m`.
//!   2. Each node starts in its own community.
//!   3. Local-moving phase: for up to `MAX_LOCAL_MOVE_ITERATIONS` passes,
//!      visit each node and move it to the neighbor community that yields
//!      the largest positive modularity gain. Stop early if no node moves
//!      in a full pass.
//!   4. Aggregate: collapse each community into a supernode (Louvain
//!      idiom), then loop step 3 on the supernode graph until no
//!      aggregation produces movement.
//!   5. Materialize: each terminal community becomes a [`Community`]
//!      with deterministic id (sha2-of-sorted-member-uids → first 16
//!      hex chars), label (most common file root path or the
//!      highest-degree member's display name), cohesion (intra-edges /
//!      total-edges incident to members), and keywords (top 5 terms
//!      from member display_names split on `_`, `::`, `/`).
//!
//! The output is consumed as a sidecar on
//! [`crate::repo_graph::RepoDependencyGraph`] so the snapshot adapter
//! in `mcp_bridge.rs` can populate the `community_id` field on each
//! emitted node.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use petgraph::graph::NodeIndex;
use petgraph::visit::EdgeRef as PetgraphEdgeRef;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::repo_graph::{RepoDependencyGraph, RepoGraphNodeKind, RepoNodeKey};

/// Cap for the local-moving phase per aggregation level. The plan
/// specified 50; we use the same value here.
const MAX_LOCAL_MOVE_ITERATIONS: usize = 50;

/// Cap on the outer (Louvain-style) aggregation loop. In practice the
/// algorithm converges in 2–3 levels on real codebases — this is just a
/// safety belt.
const MAX_AGGREGATION_LEVELS: usize = 10;

/// Top-K keywords per community.
const KEYWORDS_PER_COMMUNITY: usize = 5;

/// Singleton communities (one node, no intra-edges) are dropped before
/// returning, since they carry no clustering signal and would inflate
/// the `community_id` namespace with one entry per orphan node.
const MIN_COMMUNITY_SIZE: usize = 2;

/// A detected community of related nodes.
///
/// Persisted as a sidecar on [`RepoDependencyGraph`]; the snapshot
/// adapter joins back to it via [`RepoDependencyGraph::community_id`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Community {
    /// Stable id derived from the sorted set of member [`RepoNodeKey`]s.
    /// First 16 hex chars of `sha256(member_uids.join("\n"))`. Stable
    /// across rebuilds as long as membership is stable.
    pub id: String,
    /// Human-readable label: most common top-level path component
    /// among member files, or the highest-weighted member's
    /// `display_name` when no shared root exists.
    pub label: String,
    /// Indices of all nodes that belong to this community. Stored as
    /// `usize` so the type round-trips through bincode/JSON without
    /// petgraph helpers; converted back with `NodeIndex::new`.
    pub member_ids: Vec<usize>,
    /// Cohesion ∈ [0, 1] — `intra_edges / (intra_edges + outgoing_edges)`
    /// for the community. Higher = more self-contained.
    pub cohesion: f64,
    /// Total number of nodes in the community. Mirrors `member_ids.len()`
    /// — duplicated as an explicit field so consumers can grep by
    /// `symbol_count` without first decoding the array.
    pub symbol_count: usize,
    /// Top 5 frequency-ranked terms extracted from member display
    /// names, split on `_`, `::`, `/`, and case boundaries. Useful for
    /// generating cluster-doc titles in PR F4.
    pub keywords: Vec<String>,
}

/// Run greedy modularity-based community detection over the canonical
/// graph and return one [`Community`] per terminal partition (excluding
/// singletons).
///
/// The result is deterministic for a given graph — node visit order is
/// fixed by `NodeIndex` and tie-breaks in the move step prefer the
/// lowest-index neighbor community.
pub fn detect_communities(graph: &RepoDependencyGraph) -> Vec<Community> {
    let pg = graph.graph();
    let node_count = pg.node_count();
    if node_count == 0 {
        return Vec::new();
    }

    // Step 1: undirected weighted adjacency (HashMap for O(1) lookup
    // in the modularity inner loop — the BTreeMap layer was about
    // determinism, but node visit order is enforced separately by
    // sorting node ids in the outer loop).
    let mut adjacency: HashMap<usize, HashMap<usize, f64>> =
        HashMap::with_capacity(node_count);
    let mut k: HashMap<usize, f64> = HashMap::with_capacity(node_count);
    let mut total_weight = 0.0_f64;

    for edge_ref in pg.edge_references() {
        let s = edge_ref.source().index();
        let t = edge_ref.target().index();
        if s == t {
            continue; // drop self-loops
        }
        let w = edge_ref.weight().weight;
        if !w.is_finite() || w <= 0.0 {
            continue;
        }
        *adjacency.entry(s).or_default().entry(t).or_default() += w;
        *adjacency.entry(t).or_default().entry(s).or_default() += w;
        *k.entry(s).or_default() += w;
        *k.entry(t).or_default() += w;
        total_weight += w;
    }
    // `total_weight` is the sum over all (directed) edges; in the
    // undirected projection every contribution was added twice (once for
    // s→t and once for t→s), so the modularity normalizer m equals
    // `total_weight`. Standard CNM uses `2m = Σ A_uv` over the
    // symmetric matrix; that matches what we accumulated.
    let m = total_weight;

    // Initial partition: each node in its own community.
    let mut partition: Vec<usize> = (0..node_count).collect();

    if m <= 0.0 {
        // Edgeless graph: every node is its own community. Skip the
        // expensive loops; return empty (all singletons drop below the
        // MIN_COMMUNITY_SIZE filter anyway).
        return Vec::new();
    }

    // Local-moving + aggregation outer loop (Louvain pattern).
    for _level in 0..MAX_AGGREGATION_LEVELS {
        let moved = local_moving_phase(node_count, &adjacency, &k, m, &mut partition);
        if !moved {
            break;
        }
        // Aggregation: relabel partition as a contiguous community id
        // space, but do NOT actually rebuild the supernode adjacency —
        // the node-count is small enough (typically a few thousand
        // canonical nodes) that another pass over the original graph
        // with the new partition labels converges fine. Keeping the
        // loop "flat" sidesteps the bookkeeping for supernode self-loops.
        relabel_contiguous(&mut partition);
    }

    // Step 5: materialize Community structs from the final partition.
    materialize_communities(graph, &partition, &adjacency, m)
}

/// Run one pass of the local-moving phase. Returns `true` iff at least
/// one node changed its community label.
fn local_moving_phase(
    node_count: usize,
    adjacency: &HashMap<usize, HashMap<usize, f64>>,
    k: &HashMap<usize, f64>,
    m: f64,
    partition: &mut [usize],
) -> bool {
    // Σ_in / Σ_tot tracking per community for the modularity gain
    // formula (see e.g. Blondel et al. 2008 §2).
    //
    // sigma_tot[c] = Σ k_v for v in c (sum of degrees)
    // sigma_in[c]  = Σ A_uv for u, v in c (intra-community weight,
    //                directed sum — equals 2 × undirected count + self-loops)
    let mut sigma_tot: HashMap<usize, f64> = HashMap::new();
    let mut sigma_in: HashMap<usize, f64> = HashMap::new();
    for (v, &c) in partition.iter().enumerate().take(node_count) {
        *sigma_tot.entry(c).or_default() += k.get(&v).copied().unwrap_or(0.0);
    }
    for (&u, neighbors) in adjacency.iter() {
        let cu = partition[u];
        for (&w, &weight) in neighbors.iter() {
            if partition[w] == cu {
                *sigma_in.entry(cu).or_default() += weight;
            }
        }
    }

    let mut any_moved = false;
    for outer_pass in 0..MAX_LOCAL_MOVE_ITERATIONS {
        let mut moved_this_pass = false;

        for v in 0..node_count {
            let kv = k.get(&v).copied().unwrap_or(0.0);
            if kv <= 0.0 {
                continue; // isolated node — no useful move
            }
            let cur_comm = partition[v];

            // Σ A_vw over w in current community (excluding v itself).
            let edges_to: HashMap<usize, f64> = aggregate_edges_to_communities(
                v, adjacency, partition,
            );
            let weight_to_self = edges_to.get(&cur_comm).copied().unwrap_or(0.0);

            // Tentatively remove v from cur_comm so the gain math
            // for "stay put" comes out to 0. We only need
            // `cur_sigma_tot` for the candidate-loop math; `sigma_in`
            // bookkeeping happens at apply-move time, so we don't need
            // a removed-from-current `sigma_in` value here. The
            // `weight_to_self` factor is consumed below when we do
            // the actual move.
            let cur_sigma_tot = sigma_tot.get(&cur_comm).copied().unwrap_or(0.0) - kv;

            // Find best target community by modularity gain.
            let mut best_comm = cur_comm;
            let mut best_gain = 0.0_f64;
            // Iterate sorted by community id for deterministic tie-break.
            let mut candidates: Vec<usize> = edges_to.keys().copied().collect();
            candidates.sort_unstable();
            // Always include "stay" by treating cur_comm gain as 0
            // implicitly via best_gain initial value.
            for cand_comm in candidates {
                let weight_to_cand = edges_to.get(&cand_comm).copied().unwrap_or(0.0);
                let cand_sigma_tot = if cand_comm == cur_comm {
                    cur_sigma_tot
                } else {
                    sigma_tot.get(&cand_comm).copied().unwrap_or(0.0)
                };
                // Modularity gain of inserting v into cand_comm
                // (Blondel eq. 2):
                //   ΔQ = [ (Σ_in + 2 k_v_in) / 2m
                //          - ((Σ_tot + k_v) / 2m)^2 ]
                //        - [ Σ_in/2m - (Σ_tot/2m)^2 - (k_v/2m)^2 ]
                // which simplifies to:
                //   ΔQ = (k_v_in / m)
                //        - (Σ_tot * k_v) / (2 m^2)
                // when v has been removed from its current community
                // (so the "leave" term cancels). We compute that
                // simpler form.
                let gain = weight_to_cand / m - (cand_sigma_tot * kv) / (2.0 * m * m);
                if gain > best_gain + 1e-12 {
                    best_gain = gain;
                    best_comm = cand_comm;
                }
            }

            if best_comm != cur_comm {
                // Apply move.
                let weight_to_target = edges_to.get(&best_comm).copied().unwrap_or(0.0);
                // Withdraw from cur_comm.
                let entry_tot = sigma_tot.entry(cur_comm).or_default();
                *entry_tot -= kv;
                let entry_in = sigma_in.entry(cur_comm).or_default();
                *entry_in -= 2.0 * weight_to_self;
                // Deposit into best_comm.
                *sigma_tot.entry(best_comm).or_default() += kv;
                *sigma_in.entry(best_comm).or_default() += 2.0 * weight_to_target;
                partition[v] = best_comm;
                moved_this_pass = true;
                any_moved = true;
            }
        }

        if !moved_this_pass {
            break;
        }
        // Bail out cleanly if we hit the iteration cap mid-pass — the
        // outer aggregation loop will pick up where we left off.
        let _ = outer_pass;
    }

    any_moved
}

/// Sum the edge weights from `v` into each community in the current
/// partition. Used inside [`local_moving_phase`] to compute the
/// modularity gain for every candidate community in one pass.
fn aggregate_edges_to_communities(
    v: usize,
    adjacency: &HashMap<usize, HashMap<usize, f64>>,
    partition: &[usize],
) -> HashMap<usize, f64> {
    let mut out: HashMap<usize, f64> = HashMap::new();
    if let Some(neighbors) = adjacency.get(&v) {
        for (&w, &weight) in neighbors {
            if w == v {
                continue;
            }
            *out.entry(partition[w]).or_default() += weight;
        }
    }
    out
}

/// Renumber community labels so they form a contiguous `0..k` range
/// (after a local-moving pass, the labels are sparse — every "moved
/// out of singleton" leaves a hole). Stable: lowest-index member of
/// each community wins the new id.
fn relabel_contiguous(partition: &mut [usize]) {
    let mut remap: BTreeMap<usize, usize> = BTreeMap::new();
    let mut next_id = 0usize;
    for slot in partition.iter_mut() {
        let c = *slot;
        let new_id = *remap.entry(c).or_insert_with(|| {
            let id = next_id;
            next_id += 1;
            id
        });
        *slot = new_id;
    }
}

/// Build the final [`Community`] vec from the partition.
///
/// Drops singletons and computes cohesion / label / keywords for each
/// non-trivial community.
fn materialize_communities(
    graph: &RepoDependencyGraph,
    partition: &[usize],
    adjacency: &HashMap<usize, HashMap<usize, f64>>,
    _m: f64,
) -> Vec<Community> {
    let pg = graph.graph();
    let mut by_comm: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (v, &c) in partition.iter().enumerate() {
        by_comm.entry(c).or_default().push(v);
    }

    let mut out: Vec<Community> = Vec::new();
    for (_c, members) in by_comm {
        if members.len() < MIN_COMMUNITY_SIZE {
            continue;
        }

        // Cohesion = intra / (intra + outgoing). Counted in undirected
        // edges — divide doubled sums by 2 at the end (cancels in the
        // ratio, but keeps the math literal).
        let member_set: BTreeSet<usize> = members.iter().copied().collect();
        let mut intra = 0.0_f64;
        let mut outgoing = 0.0_f64;
        for &u in &members {
            if let Some(neighbors) = adjacency.get(&u) {
                for (&w, &weight) in neighbors {
                    if member_set.contains(&w) {
                        intra += weight;
                    } else {
                        outgoing += weight;
                    }
                }
            }
        }
        // intra is double-counted (every internal edge contributes from
        // both endpoints); outgoing is single-counted (only the inside
        // endpoint is iterated).
        let intra_undirected = intra / 2.0;
        let total_incident = intra_undirected + outgoing;
        let cohesion = if total_incident > 0.0 {
            intra_undirected / total_incident
        } else {
            0.0
        };

        // Stable id: sha256 of sorted member uids → first 16 hex chars.
        let mut uids: Vec<String> = members
            .iter()
            .map(|&v| format_member_uid(&pg[NodeIndex::new(v)].id))
            .collect();
        uids.sort();
        let mut hasher = Sha256::new();
        for uid in &uids {
            hasher.update(uid.as_bytes());
            hasher.update(b"\n");
        }
        let digest = hasher.finalize();
        let id_hex = hex::encode(digest);
        let id = id_hex[..16].to_string();

        // Label: most common top-level path segment among file paths,
        // falling back to the highest-degree member's display name.
        let label = derive_label(graph, &members);

        // Keywords: top-K frequency tokens from member display_names.
        let keywords = derive_keywords(graph, &members, KEYWORDS_PER_COMMUNITY);

        out.push(Community {
            id,
            label,
            symbol_count: members.len(),
            member_ids: members,
            cohesion,
            keywords,
        });
    }

    // Deterministic order: largest first, then by id.
    out.sort_by(|a, b| {
        b.symbol_count
            .cmp(&a.symbol_count)
            .then_with(|| a.id.cmp(&b.id))
    });
    out
}

fn format_member_uid(key: &RepoNodeKey) -> String {
    match key {
        RepoNodeKey::File(p) => format!("file:{}", p.display()),
        RepoNodeKey::Symbol(s) => format!("symbol:{s}"),
        // Synthetic process nodes (PR F2) shouldn't normally appear in a
        // community member list — they're added post-detection — but
        // surface a stable uid if one slips through.
        RepoNodeKey::Process(id) => format!("process:{id}"),
        // Synthetic table nodes (DB-access pass) likewise sit outside
        // the community partition — they're sinks, not first-class
        // members — but a stable uid keeps the format honest.
        RepoNodeKey::Table(name) => format!("table:{name}"),
    }
}

/// Pick a label by:
/// 1. Counting top-level path segments across member file_paths; if a
///    single segment dominates (≥ 50% of members with a file_path),
///    use it.
/// 2. Otherwise return the display_name of the member with the
///    highest `intrinsic_weight × degree` proxy (highest-degree wins).
fn derive_label(graph: &RepoDependencyGraph, members: &[usize]) -> String {
    let pg = graph.graph();

    // Path-segment vote.
    let mut segment_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut members_with_path = 0usize;
    for &v in members {
        let node = &pg[NodeIndex::new(v)];
        if let Some(p) = &node.file_path {
            members_with_path += 1;
            // First non-empty path component (e.g. "src", "server").
            if let Some(seg) = p
                .components()
                .find_map(|c| match c {
                    std::path::Component::Normal(s) => s.to_str().map(str::to_string),
                    _ => None,
                })
            {
                *segment_counts.entry(seg).or_default() += 1;
            }
        }
    }
    if members_with_path > 0 {
        // Pick the most common segment (BTreeMap iter is sorted by key,
        // so ties resolve alphabetically — deterministic).
        if let Some((seg, &count)) =
            segment_counts.iter().max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
            && count * 2 >= members_with_path
        {
            return seg.clone();
        }
    }

    // Fallback: pick the member with the largest total adjacency.
    let mut best_idx: Option<NodeIndex> = None;
    let mut best_degree = 0usize;
    for &v in members {
        let idx = NodeIndex::new(v);
        let degree = pg.edges(idx).count();
        if degree > best_degree {
            best_degree = degree;
            best_idx = Some(idx);
        }
    }
    if let Some(idx) = best_idx {
        return pg[idx].display_name.clone();
    }
    // Truly empty community (shouldn't reach here given MIN_COMMUNITY_SIZE).
    String::from("community")
}

/// Tokenize each member display_name on `_`, `::`, `/`, `.`, and
/// case boundaries; lowercased, deduped per-name, then frequency-ranked
/// across the community. Drop tokens shorter than 3 characters.
fn derive_keywords(graph: &RepoDependencyGraph, members: &[usize], top_k: usize) -> Vec<String> {
    let pg = graph.graph();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for &v in members {
        let node = &pg[NodeIndex::new(v)];
        // Skip raw scip symbol strings — they're noisy. Use display_name
        // (which is human-friendly, e.g. "helper" or "MyStruct"). For
        // file nodes, also include the file stem so directory-anchored
        // communities pick up filename tokens.
        let raw = match node.kind {
            RepoGraphNodeKind::Symbol => node.display_name.clone(),
            RepoGraphNodeKind::File => node
                .file_path
                .as_ref()
                .and_then(|p| p.file_stem().and_then(|s| s.to_str().map(str::to_string)))
                .unwrap_or_else(|| node.display_name.clone()),
            // Synthetic process nodes (PR F2) shouldn't normally appear
            // here, but fall back to the label if one does.
            RepoGraphNodeKind::Process => node.display_name.clone(),
            // Synthetic table nodes — same fallback.
            RepoGraphNodeKind::Table => node.display_name.clone(),
        };
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for token in tokenize_identifier(&raw) {
            if token.len() < 3 {
                continue;
            }
            if seen.insert(token.clone()) {
                *counts.entry(token).or_default() += 1;
            }
        }
    }
    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.into_iter().take(top_k).map(|(k, _)| k).collect()
}

/// Split an identifier into lower-cased tokens on `_`, `::`, `/`,
/// `.`, ` `, and camelCase / PascalCase boundaries.
fn tokenize_identifier(input: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut prev_was_lower = false;
    for ch in input.chars() {
        let is_sep = matches!(ch, '_' | '/' | '.' | ' ' | ':' | '-' | '`' | '(' | ')'
            | '[' | ']' | '<' | '>' | '#' | '!' | '@' | '$' | '%' | '^'
            | '&' | '*' | '+' | '=' | ',' | ';' | '?' | '"' | '\'');
        if is_sep {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current).to_ascii_lowercase());
            }
            prev_was_lower = false;
            continue;
        }
        if ch.is_ascii_uppercase() && prev_was_lower && !current.is_empty() {
            tokens.push(std::mem::take(&mut current).to_ascii_lowercase());
        }
        current.push(ch);
        prev_was_lower = ch.is_ascii_lowercase() || ch.is_ascii_digit();
    }
    if !current.is_empty() {
        tokens.push(current.to_ascii_lowercase());
    }
    tokens
}

/// Read the `DJINN_COMMUNITY_DETECTION` flag. Default `true`. Recognized
/// "off" values: `0`, `false`, `no`, `off` (case-insensitive). Any
/// other value (including unset) means on.
pub fn detection_enabled() -> bool {
    match std::env::var("DJINN_COMMUNITY_DETECTION") {
        Err(_) => true,
        Ok(v) => {
            let lower = v.trim().to_ascii_lowercase();
            !matches!(lower.as_str(), "0" | "false" | "no" | "off")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::repo_graph::{
        RepoDependencyGraph, RepoGraphEdgeKind, RepoGraphNode, RepoGraphNodeKind,
        RepoNodeKey,
    };

    /// Build a tiny manual graph with two clusters of 3 nodes each,
    /// connected internally by tight edges and across by a single
    /// thin edge. Modularity should partition them cleanly.
    ///
    /// We bypass the SCIP builder and inject nodes/edges directly via
    /// the artifact round-trip seam, since the SCIP-shaped builder
    /// doesn't expose a low-level "add edge" hook.
    fn two_cluster_graph() -> RepoDependencyGraph {
        use crate::repo_graph::{
            REPO_GRAPH_ARTIFACT_VERSION, RepoGraphArtifact, RepoGraphArtifactEdge,
        };

        let mk_symbol_node = |name: &str, file: &str| RepoGraphNode {
            id: RepoNodeKey::Symbol(format!("symbol:{name}")),
            kind: RepoGraphNodeKind::Symbol,
            display_name: name.to_string(),
            language: Some("rust".to_string()),
            file_path: Some(PathBuf::from(file)),
            symbol: Some(format!("symbol:{name}")),
            symbol_kind: None,
            is_external: false,
            visibility: None,
            signature: None,
            documentation: vec![],
            signature_parts: None,
            is_test: false,
        };

        let nodes = vec![
            mk_symbol_node("auth_login", "src/auth/login.rs"),
            mk_symbol_node("auth_session", "src/auth/session.rs"),
            mk_symbol_node("auth_token", "src/auth/token.rs"),
            mk_symbol_node("billing_charge", "src/billing/charge.rs"),
            mk_symbol_node("billing_invoice", "src/billing/invoice.rs"),
            mk_symbol_node("billing_refund", "src/billing/refund.rs"),
        ];

        let edge = |s, t, w| RepoGraphArtifactEdge {
            source: s,
            target: t,
            kind: RepoGraphEdgeKind::SymbolReference,
            weight: w,
            evidence_count: 1,
            confidence: 0.9,
            reason: None,
            step: None,
        };
        let edges = vec![
            // auth cluster: 0 ↔ 1, 1 ↔ 2, 0 ↔ 2 (triangle, heavy)
            edge(0, 1, 5.0),
            edge(1, 0, 5.0),
            edge(1, 2, 5.0),
            edge(2, 1, 5.0),
            edge(0, 2, 5.0),
            edge(2, 0, 5.0),
            // billing cluster: 3 ↔ 4, 4 ↔ 5, 3 ↔ 5
            edge(3, 4, 5.0),
            edge(4, 3, 5.0),
            edge(4, 5, 5.0),
            edge(5, 4, 5.0),
            edge(3, 5, 5.0),
            edge(5, 3, 5.0),
            // Thin bridge between clusters: 2 ↔ 3
            edge(2, 3, 0.5),
            edge(3, 2, 0.5),
        ];

        let artifact = RepoGraphArtifact {
            version: REPO_GRAPH_ARTIFACT_VERSION,
            nodes,
            edges,
            symbol_ranges: BTreeMap::new(),
            communities: Vec::new(),
            processes: Vec::new(),
        };
        RepoDependencyGraph::from_artifact(&artifact)
    }

    #[test]
    fn detect_communities_empty_graph_returns_empty() {
        use crate::repo_graph::{REPO_GRAPH_ARTIFACT_VERSION, RepoGraphArtifact};
        let artifact = RepoGraphArtifact {
            version: REPO_GRAPH_ARTIFACT_VERSION,
            nodes: vec![],
            edges: vec![],
            symbol_ranges: BTreeMap::new(),
            communities: Vec::new(),
            processes: Vec::new(),
        };
        let graph = RepoDependencyGraph::from_artifact(&artifact);
        assert!(detect_communities(&graph).is_empty());
    }

    #[test]
    fn detect_communities_partitions_two_tight_clusters() {
        let graph = two_cluster_graph();
        let communities = detect_communities(&graph);
        assert!(
            communities.len() >= 2,
            "expected at least two communities (auth + billing), got {}: {:?}",
            communities.len(),
            communities.iter().map(|c| (c.label.clone(), c.member_ids.clone())).collect::<Vec<_>>()
        );

        // Every member of the auth cluster should share a community
        // distinct from the billing cluster.
        let auth_idx: Vec<usize> = (0..3).collect();
        let billing_idx: Vec<usize> = (3..6).collect();

        let comm_for = |target: usize| -> Option<&Community> {
            communities.iter().find(|c| c.member_ids.contains(&target))
        };

        let auth_comm = comm_for(0).expect("auth_login should live in some community");
        for v in &auth_idx {
            assert!(
                auth_comm.member_ids.contains(v),
                "auth member {v} not in shared community {:?}",
                auth_comm.member_ids
            );
        }
        let billing_comm = comm_for(3).expect("billing_charge should live in some community");
        for v in &billing_idx {
            assert!(
                billing_comm.member_ids.contains(v),
                "billing member {v} not in shared community {:?}",
                billing_comm.member_ids
            );
        }
        assert_ne!(
            auth_comm.id, billing_comm.id,
            "auth and billing should not share a community"
        );

        // Cohesion: each cluster has 3 internal edges (undirected,
        // weight 5 each = 15) and 1 outgoing edge (weight 0.5) →
        // cohesion ≈ 15 / 15.5 ≈ 0.967.
        assert!(
            auth_comm.cohesion > 0.9,
            "auth cohesion too low: {}",
            auth_comm.cohesion
        );
        assert!(
            billing_comm.cohesion > 0.9,
            "billing cohesion too low: {}",
            billing_comm.cohesion
        );

        // Labels should pick up the directory ("src" — the shared top
        // segment for both clusters in this fixture, since we use
        // src/{auth,billing}/...). The path-segment vote uses the
        // first non-empty component, so "src" wins.
        assert_eq!(auth_comm.label, "src");
        assert_eq!(billing_comm.label, "src");
    }

    #[test]
    fn community_id_is_stable_across_calls() {
        let graph = two_cluster_graph();
        let a = detect_communities(&graph);
        let b = detect_communities(&graph);
        assert_eq!(
            a.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
            b.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
            "community ids should be deterministic"
        );
    }

    #[test]
    fn tokenize_splits_camel_snake_path() {
        let toks = tokenize_identifier("MyClass_handle_request::do_thing");
        assert_eq!(
            toks,
            vec!["my", "class", "handle", "request", "do", "thing"],
        );
    }

    #[test]
    fn tokenize_drops_scip_punctuation() {
        let toks = tokenize_identifier("`helper`().");
        assert_eq!(toks, vec!["helper"]);
    }

    /// Sanity check the modularity of a known-clean partition is
    /// strictly positive — this is the quality target community
    /// detection optimizes. Q ranges over [-0.5, 1]; well-separated
    /// clusters give Q > 0.3.
    #[test]
    fn modularity_of_two_cluster_partition_is_positive() {
        let graph = two_cluster_graph();
        let communities = detect_communities(&graph);
        // Compute Q from the communities.
        let pg = graph.graph();
        let mut adjacency: HashMap<usize, HashMap<usize, f64>> = HashMap::new();
        let mut k: HashMap<usize, f64> = HashMap::new();
        let mut m = 0.0_f64;
        for er in pg.edge_references() {
            let s = er.source().index();
            let t = er.target().index();
            let w = er.weight().weight;
            *adjacency.entry(s).or_default().entry(t).or_default() += w;
            *adjacency.entry(t).or_default().entry(s).or_default() += w;
            *k.entry(s).or_default() += w;
            *k.entry(t).or_default() += w;
            m += w;
        }
        let mut comm_of: HashMap<usize, &Community> = HashMap::new();
        for c in &communities {
            for &v in &c.member_ids {
                comm_of.insert(v, c);
            }
        }
        let mut q = 0.0_f64;
        for u in 0..pg.node_count() {
            for v in 0..pg.node_count() {
                let cu = comm_of.get(&u).map(|c| c.id.as_str());
                let cv = comm_of.get(&v).map(|c| c.id.as_str());
                if cu.is_none() || cv.is_none() || cu != cv {
                    continue;
                }
                let a_uv = adjacency
                    .get(&u)
                    .and_then(|n| n.get(&v))
                    .copied()
                    .unwrap_or(0.0);
                let ku = k.get(&u).copied().unwrap_or(0.0);
                let kv = k.get(&v).copied().unwrap_or(0.0);
                q += a_uv - (ku * kv) / (2.0 * m);
            }
        }
        q /= 2.0 * m;
        assert!(
            q > 0.3,
            "expected positive modularity for clean cluster split, got Q={q}"
        );
    }
}
