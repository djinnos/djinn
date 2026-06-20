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

/// Controls the granularity of community detection.  Higher resolution
/// produces more, smaller communities; lower resolution produces fewer,
/// larger communities.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Resolution {
    /// More communities, smaller minimum size (min_community_size = 1).
    Fine,
    /// Default balanced resolution (min_community_size = 2).
    #[default]
    Medium,
    /// Fewer communities, larger minimum size (min_community_size = 4).
    Coarse,
}

/// Map a [`Resolution`] to the minimum community member count.
fn min_community_size_for(resolution: Resolution) -> usize {
    match resolution {
        Resolution::Fine => 1,
        Resolution::Medium => 2,
        Resolution::Coarse => 4,
    }
}

/// Cap for the local-moving phase per aggregation level. The plan
/// specified 50; we use the same value here.
const MAX_LOCAL_MOVE_ITERATIONS: usize = 50;

/// Cap on the outer (Louvain-style) aggregation loop. In practice the
/// algorithm converges in 2–3 levels on real codebases — this is just a
/// safety belt.
const MAX_AGGREGATION_LEVELS: usize = 10;

/// Top-K keywords per community.
const KEYWORDS_PER_COMMUNITY: usize = 5;

/// Minimum community size is now controlled by [`Resolution`] via
/// [`min_community_size_for`].  The default ([`Resolution::Medium`])
/// uses a minimum of 2 members — singletons are dropped since they
/// carry no clustering signal.
///
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

/// Optional configuration for community detection: granularity plus
/// optional crate-aware pre-seeding of the initial partition.
///
/// When `seed_by_crate` is `Some`, the initial partition groups all
/// nodes whose `file_path` belongs to the same crate (longest-prefix
/// match against the map) into a single starting community before the
/// Louvain-style local-moving phase runs. This biases the final
/// communities toward crate boundaries — modularity optimization can
/// still split or merge crates where cross-crate edges are dense, but
/// members of one crate predominantly end up together. Default
/// (`None`) leaves the legacy singletons-as-start behaviour untouched.
#[derive(Debug, Clone, Default)]
pub struct CommunityDetectionOptions {
    /// Community granularity. See [`Resolution`].
    pub resolution: Resolution,
    /// Optional crate map (`crate-root-prefix → crate-name`) used to
    /// pre-seed the initial partition by crate membership. `None`
    /// (the default) disables seeding for backward compatibility.
    pub seed_by_crate: Option<std::collections::BTreeMap<std::path::PathBuf, String>>,
}

/// Run greedy modularity-based community detection over the canonical
/// graph and return one [`Community`] per terminal partition (excluding
/// singletons).
///
/// The result is deterministic for a given graph — node visit order is
/// fixed by `NodeIndex` and tie-breaks in the move step prefer the
/// lowest-index neighbor community.
///
/// Uses [`Resolution::default`] for community granularity.  Call
/// [`detect_communities_with_resolution`] to override.
pub fn detect_communities(graph: &RepoDependencyGraph) -> Vec<Community> {
    detect_communities_with_options(graph, CommunityDetectionOptions::default())
}

/// Like [`detect_communities`] but accepts an explicit [`Resolution`]
/// to control community granularity.
pub fn detect_communities_with_resolution(
    graph: &RepoDependencyGraph,
    resolution: Resolution,
) -> Vec<Community> {
    detect_communities_with_options(
        graph,
        CommunityDetectionOptions {
            resolution,
            seed_by_crate: None,
        },
    )
}

/// Full-feature entry point: greedy modularity-based community detection
/// over the canonical graph with optional crate-aware pre-seeding.
///
/// When `options.seed_by_crate` is `Some`, the initial partition groups
/// nodes by crate membership (longest-prefix match) before the
/// Louvain-style local-moving phase runs, biasing the result toward
/// crate boundaries. When `None`, behaviour is identical to the legacy
/// singletons-as-start path.
///
/// The result is deterministic for a given graph and set of options —
/// node visit order is fixed by `NodeIndex` and tie-breaks in the move
/// step prefer the lowest-index neighbor community.
pub fn detect_communities_with_options(
    graph: &RepoDependencyGraph,
    options: CommunityDetectionOptions,
) -> Vec<Community> {
    let min_size = min_community_size_for(options.resolution);
    let pg = graph.graph();
    let node_count = pg.node_count();
    if node_count == 0 {
        return Vec::new();
    }

    // Step 1: undirected weighted adjacency (HashMap for O(1) lookup
    // in the modularity inner loop — the BTreeMap layer was about
    // determinism, but node visit order is enforced separately by
    // sorting node ids in the outer loop).
    let mut adjacency: HashMap<usize, HashMap<usize, f64>> = HashMap::with_capacity(node_count);
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

    // Initial partition. With crate-aware seeding enabled, nodes that
    // share a crate start in the same community (contiguous id per
    // crate); otherwise each node starts as a singleton.
    let mut partition: Vec<usize> = match options.seed_by_crate.as_ref() {
        Some(crate_map) => seed_partition_by_crate(graph, node_count, crate_map),
        None => (0..node_count).collect(),
    };

    if m <= 0.0 {
        // Edgeless graph: every node is its own community. Skip the
        // expensive loops; return empty (all singletons drop below the
        // min_community_size filter anyway).
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
    materialize_communities(graph, &partition, &adjacency, m, min_size)
}

/// Resolve a node's file path to a crate name via the longest matching
/// prefix in `crate_map`, mirroring `crate_aggregation::resolve_crate`.
/// Returns `None` when the node has no `file_path` or the path matches
/// no crate (i.e. it lives outside any known crate boundary); such nodes
/// are left as singletons by [`seed_partition_by_crate`].
fn resolve_crate_for_node<'a>(
    file_path: Option<&std::path::Path>,
    crate_map: &'a BTreeMap<std::path::PathBuf, String>,
) -> Option<&'a str> {
    let path = file_path?;
    let mut best: Option<(&'a str, usize)> = None;
    for (prefix, name) in crate_map {
        if path.starts_with(prefix) {
            let len = prefix.as_os_str().len();
            let take = match best {
                Some((_, prev_len)) => len > prev_len,
                None => true,
            };
            if take {
                best = Some((name.as_str(), len));
            }
        }
    }
    best.map(|(name, _)| name)
}

/// Build the initial community partition by grouping nodes that share a
/// crate (longest-prefix match against `crate_map`) into a single
/// starting community. Nodes with no `file_path` or whose path matches
/// no crate each start as singletons so the local-moving phase can
/// place them freely. Community ids are assigned contiguously: distinct
/// crates receive ascending ids first, then each unmatched node gets its
/// own id.
fn seed_partition_by_crate(
    graph: &RepoDependencyGraph,
    node_count: usize,
    crate_map: &BTreeMap<std::path::PathBuf, String>,
) -> Vec<usize> {
    let pg = graph.graph();
    let mut crate_to_comm: BTreeMap<String, usize> = BTreeMap::new();
    let mut next_comm = 0usize;
    let mut partition = vec![0usize; node_count];
    for v in 0..node_count {
        let node = &pg[NodeIndex::new(v)];
        match resolve_crate_for_node(node.file_path.as_deref(), crate_map) {
            Some(crate_name) => {
                let comm = *crate_to_comm
                    .entry(crate_name.to_string())
                    .or_insert_with(|| {
                        let id = next_comm;
                        next_comm += 1;
                        id
                    });
                partition[v] = comm;
            }
            None => {
                // No crate matches this node: start it as a singleton so
                // the local-moving phase can relocate it freely.
                let comm = next_comm;
                next_comm += 1;
                partition[v] = comm;
            }
        }
    }
    partition
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
            let edges_to: HashMap<usize, f64> =
                aggregate_edges_to_communities(v, adjacency, partition);
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
/// Drops communities below `min_community_size` and computes cohesion /
/// label / keywords for each surviving community.
fn materialize_communities(
    graph: &RepoDependencyGraph,
    partition: &[usize],
    adjacency: &HashMap<usize, HashMap<usize, f64>>,
    _m: f64,
    min_community_size: usize,
) -> Vec<Community> {
    let pg = graph.graph();
    let mut by_comm: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (v, &c) in partition.iter().enumerate() {
        by_comm.entry(c).or_default().push(v);
    }

    let mut out: Vec<Community> = Vec::new();
    for (_c, members) in by_comm {
        if members.len() < min_community_size {
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
        // PR s6ch / cs4v: route / tool nodes are likewise synthetic
        // side-channel metadata outside the community partition.
        // Mirror the `process:` / `table:` prefixing so downstream
        // uids stay parseable.
        RepoNodeKey::Route(id) => format!("route:{id}"),
        RepoNodeKey::Tool(id) => format!("tool:{id}"),
    }
}

/// Pick a label by:
/// 1. Collecting path segments for each member, skipping the
///    workspace-root segment (the first component that matches the
///    node's `workspace` field).
/// 2. Finding the first path-component index where members differ
///    ("distinguishing index") and voting on that component.
///    If a single segment dominates (≥ 50% of members with a
///    file_path), use it.
/// 3. If no path segment dominates (all members share the same
///    prefix, or no segment has ≥ 50%), fall back to the top keyword
///    from [`derive_keywords`].
/// 4. Last resort: return the display_name of the member with the
///    highest degree.
fn derive_label(graph: &RepoDependencyGraph, members: &[usize]) -> String {
    let pg = graph.graph();

    // Collect path segments after skipping workspace-root component.
    let mut member_segments: Vec<Vec<String>> = Vec::new();
    for &v in members {
        let node = &pg[NodeIndex::new(v)];
        if let Some(p) = &node.file_path {
            let segments: Vec<String> = p
                .components()
                .filter_map(|c| match c {
                    std::path::Component::Normal(s) => s.to_str().map(str::to_string),
                    _ => None,
                })
                .collect();

            // Skip the first segment when it matches the workspace slug.
            let skip = if let Some(ws) = &node.workspace {
                if segments.first().map(|s| s.as_str()) == Some(ws.as_str()) {
                    1
                } else {
                    0
                }
            } else {
                0
            };

            let remaining: Vec<String> = segments.into_iter().skip(skip).collect();
            if !remaining.is_empty() {
                member_segments.push(remaining);
            }
        }
    }

    if !member_segments.is_empty() {
        // Find the first component index where not all members agree.
        // Length differences count as distinguishing: if one path ends
        // while another has another segment, the extra segment can be
        // useful. If every path has exactly the same remaining segments,
        // there is no distinguishing path component and we should use
        // keyword-derived tokens instead of returning a shared prefix like
        // "crates" or "src".
        let max_len = member_segments.iter().map(|s| s.len()).max().unwrap_or(0);
        let mut distinguishing_idx: Option<usize> = None;
        for i in 0..max_len {
            let first = member_segments[0].get(i).map(String::as_str);
            if member_segments
                .iter()
                .any(|s| s.get(i).map(String::as_str) != first)
            {
                distinguishing_idx = Some(i);
                break;
            }
        }

        if let Some(vote_idx) = distinguishing_idx {
            let mut segment_counts: BTreeMap<String, usize> = BTreeMap::new();
            let members_with_path = member_segments.len();
            for segs in &member_segments {
                if segs.len() > vote_idx {
                    *segment_counts.entry(segs[vote_idx].clone()).or_default() += 1;
                }
            }

            if let Some((seg, &count)) = segment_counts
                .iter()
                .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
                && count * 2 >= members_with_path
            {
                return seg.clone();
            }
        }

        // No segment distinguishes this community (or no distinguishing
        // segment dominates) — fall back to top keyword token.
        let keywords = derive_keywords(graph, members, KEYWORDS_PER_COMMUNITY);
        if let Some(top) = keywords.into_iter().next() {
            return top;
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
    // Truly empty community (shouldn't reach here given min_community_size).
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
            // PR s6ch / cs4v: synthetic route / tool nodes — same
            // fallback. The community partition excludes them in
            // practice, but tokenizing the display_name keeps the
            // match exhaustive and a stray member from producing a
            // non-deterministic label.
            RepoGraphNodeKind::Route => node.display_name.clone(),
            RepoGraphNodeKind::Tool => node.display_name.clone(),
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
        let is_sep = matches!(
            ch,
            '_' | '/'
                | '.'
                | ' '
                | ':'
                | '-'
                | '`'
                | '('
                | ')'
                | '['
                | ']'
                | '<'
                | '>'
                | '#'
                | '!'
                | '@'
                | '$'
                | '%'
                | '^'
                | '&'
                | '*'
                | '+'
                | '='
                | ','
                | ';'
                | '?'
                | '"'
                | '\''
        );
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

/// Read the `DJINN_COMMUNITY_SEED_BY_CRATE` flag. Default `false` so
/// production community detection remains unseeded unless explicitly opted in.
/// Recognized "on" values: `1`, `true`, `yes`, `on` (case-insensitive).
pub fn crate_seeding_enabled() -> bool {
    match std::env::var("DJINN_COMMUNITY_SEED_BY_CRATE") {
        Err(_) => false,
        Ok(v) => {
            let lower = v.trim().to_ascii_lowercase();
            matches!(lower.as_str(), "1" | "true" | "yes" | "on")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::canonical_graph::CrateMap;
    use crate::repo_graph::{
        RepoDependencyGraph, RepoGraphEdgeKind, RepoGraphNode, RepoGraphNodeKind, RepoNodeKey,
    };

    /// Build a multi-crate test fixture with 3+ crates (alpha, beta, gamma),
    /// each containing 3+ nodes, internal heavy edges, and thin cross-crate
    /// bridges. Returns the graph plus a `CrateMap` that maps each crate's
    /// root directory to its crate name.
    fn multi_crate_fixture() -> (RepoDependencyGraph, CrateMap) {
        use crate::repo_graph::{
            REPO_GRAPH_ARTIFACT_VERSION, RepoGraphArtifact, RepoGraphArtifactEdge,
        };

        let mk_node = |name: &str, file: &str| RepoGraphNode {
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
            complexity: None,
            workspace: None,
            route_framework: None,
            route_handler_symbol: None,
        };

        let nodes = vec![
            // alpha crate (nodes 0..3)
            mk_node("alpha_one", "crates/alpha/src/one.rs"),
            mk_node("alpha_two", "crates/alpha/src/two.rs"),
            mk_node("alpha_three", "crates/alpha/src/three.rs"),
            // beta crate (nodes 3..6)
            mk_node("beta_one", "crates/beta/src/one.rs"),
            mk_node("beta_two", "crates/beta/src/two.rs"),
            mk_node("beta_three", "crates/beta/src/three.rs"),
            // gamma crate (nodes 6..9)
            mk_node("gamma_one", "crates/gamma/src/one.rs"),
            mk_node("gamma_two", "crates/gamma/src/two.rs"),
            mk_node("gamma_three", "crates/gamma/src/three.rs"),
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
            // alpha internal heavy edges (triangle + extra)
            edge(0, 1, 5.0),
            edge(1, 0, 5.0),
            edge(1, 2, 5.0),
            edge(2, 1, 5.0),
            edge(0, 2, 5.0),
            edge(2, 0, 5.0),
            // beta internal heavy edges
            edge(3, 4, 5.0),
            edge(4, 3, 5.0),
            edge(4, 5, 5.0),
            edge(5, 4, 5.0),
            edge(3, 5, 5.0),
            edge(5, 3, 5.0),
            // gamma internal heavy edges
            edge(6, 7, 5.0),
            edge(7, 6, 5.0),
            edge(7, 8, 5.0),
            edge(8, 7, 5.0),
            edge(6, 8, 5.0),
            edge(8, 6, 5.0),
            // thin cross-crate bridges
            edge(2, 3, 0.5),
            edge(3, 2, 0.5),
            edge(5, 6, 0.5),
            edge(6, 5, 0.5),
        ];

        let artifact = RepoGraphArtifact {
            version: REPO_GRAPH_ARTIFACT_VERSION,
            nodes,
            edges,
            symbol_ranges: BTreeMap::new(),
            communities: Vec::new(),
            processes: Vec::new(),
            route_exclusion_config: Default::default(),
            layout_positions: BTreeMap::new(),
        };
        let graph = RepoDependencyGraph::from_artifact(&artifact);

        let mut crate_map = CrateMap::new();
        crate_map.insert(PathBuf::from("crates/alpha"), "alpha".to_string());
        crate_map.insert(PathBuf::from("crates/beta"), "beta".to_string());
        crate_map.insert(PathBuf::from("crates/gamma"), "gamma".to_string());

        (graph, crate_map)
    }

    /// Compute the dominant-crate fraction for a community.
    /// For each member, resolve its crate via `resolve_crate_for_node`.
    /// Returns (max_crate_count / total_members) as a fraction in [0.0, 1.0].
    fn crate_purity(
        community: &Community,
        graph: &RepoDependencyGraph,
        crate_map: &CrateMap,
    ) -> f64 {
        let pg = graph.graph();
        let mut counts: HashMap<String, usize> = HashMap::new();
        for &v in &community.member_ids {
            let node = &pg[NodeIndex::new(v)];
            if let Some(crate_name) = resolve_crate_for_node(node.file_path.as_deref(), crate_map) {
                *counts.entry(crate_name.to_string()).or_default() += 1;
            }
        }
        let total = community.member_ids.len();
        if total == 0 {
            return 0.0;
        }
        let max_count = counts.values().copied().max().unwrap_or(0);
        max_count as f64 / total as f64
    }

    /// Return the purity of the *best* community for a given crate — i.e. the
    /// highest fraction of that crate's nodes that landed in any single
    /// community.  This is the per-crate metric used by the acceptance tests.
    fn best_crate_purity(
        crate_name: &str,
        graph: &RepoDependencyGraph,
        communities: &[Community],
        crate_map: &CrateMap,
    ) -> f64 {
        let pg = graph.graph();
        let mut crate_nodes: Vec<usize> = Vec::new();
        for v in 0..pg.node_count() {
            let node = &pg[NodeIndex::new(v)];
            if resolve_crate_for_node(node.file_path.as_deref(), crate_map) == Some(crate_name) {
                crate_nodes.push(v);
            }
        }
        if crate_nodes.is_empty() {
            return 0.0;
        }
        let mut best = 0.0_f64;
        for comm in communities {
            let in_comm = crate_nodes
                .iter()
                .filter(|&&v| comm.member_ids.contains(&v))
                .count();
            let frac = in_comm as f64 / crate_nodes.len() as f64;
            if frac > best {
                best = frac;
            }
        }
        best
    }

    #[test]
    fn seeded_communities_respect_crate_boundaries() {
        let (graph, crate_map) = multi_crate_fixture();
        let communities = detect_communities_with_options(
            &graph,
            CommunityDetectionOptions {
                resolution: Resolution::Medium,
                seed_by_crate: Some(crate_map.clone()),
            },
        );

        // Assert per-crate: ≥80% of each crate's nodes land in one community.
        for crate_name in ["alpha", "beta", "gamma"] {
            let purity = best_crate_purity(crate_name, &graph, &communities, &crate_map);
            assert!(
                purity >= 0.80,
                "crate '{}' should have ≥80% of its nodes in one community with seeding, got {:.2}",
                crate_name,
                purity
            );
        }

        // Assert per-community: every community with ≥2 members has ≥80%
        // purity (i.e. the dominant crate accounts for ≥80% of members).
        for comm in &communities {
            if comm.symbol_count >= 2 {
                let purity = crate_purity(comm, &graph, &crate_map);
                assert!(
                    purity >= 0.80,
                    "community '{}' ({} members) should have ≥80% purity, got {:.2}",
                    comm.label,
                    comm.symbol_count,
                    purity
                );
            }
        }
    }

    #[test]
    fn seeded_outperforms_unseeded() {
        let (graph, crate_map) = multi_crate_fixture();

        let seeded = detect_communities_with_options(
            &graph,
            CommunityDetectionOptions {
                resolution: Resolution::Medium,
                seed_by_crate: Some(crate_map.clone()),
            },
        );
        let unseeded = detect_communities_with_options(
            &graph,
            CommunityDetectionOptions {
                resolution: Resolution::Medium,
                seed_by_crate: None,
            },
        );

        let seeded_avg = ["alpha", "beta", "gamma"]
            .iter()
            .map(|c| best_crate_purity(c, &graph, &seeded, &crate_map))
            .sum::<f64>()
            / 3.0;
        let unseeded_avg = ["alpha", "beta", "gamma"]
            .iter()
            .map(|c| best_crate_purity(c, &graph, &unseeded, &crate_map))
            .sum::<f64>()
            / 3.0;

        // The fixture is designed so that unseeded detection may also find
        // the optimal partition (tight clusters + thin bridges), so we
        // assert seeded is *at least as good* rather than strictly greater.
        assert!(
            seeded_avg >= unseeded_avg,
            "seeded detection should have >= average crate purity than unseeded: seeded={:.3}, unseeded={:.3}",
            seeded_avg,
            unseeded_avg
        );
    }

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
            complexity: None,
            workspace: None,
            // PR s6ch / cs4v: route metadata is not applicable to
            // these placeholder symbol nodes — defaults to `None`.
            route_framework: None,
            route_handler_symbol: None,
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
            route_exclusion_config: Default::default(),
            layout_positions: BTreeMap::new(),
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
            route_exclusion_config: Default::default(),
            layout_positions: BTreeMap::new(),
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
            communities
                .iter()
                .map(|c| (c.label.clone(), c.member_ids.clone()))
                .collect::<Vec<_>>()
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

        // Labels should pick up the *distinguishing* path component.
        // Both clusters share "src" as the first component, so the
        // distinguishing index is 1: "auth" vs "billing".
        assert_eq!(auth_comm.label, "auth");
        assert_eq!(billing_comm.label, "billing");
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

    /// Build a monorepo-style fixture with workspace="server" paths.
    /// Three clusters: djinn-graph (3 nodes), djinn-auth (3 nodes),
    /// djinn-billing (3 nodes). Paths all start with
    /// `server/crates/<crate>/src/...` and workspace is "server".
    fn monorepo_cluster_graph() -> RepoDependencyGraph {
        use crate::repo_graph::{
            REPO_GRAPH_ARTIFACT_VERSION, RepoGraphArtifact, RepoGraphArtifactEdge,
        };

        let mk_node = |name: &str, file: &str, ws: Option<&str>| RepoGraphNode {
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
            complexity: None,
            workspace: ws.map(str::to_string),
            route_framework: None,
            route_handler_symbol: None,
        };

        let nodes = vec![
            // djinn-graph cluster
            mk_node(
                "detect_communities",
                "server/crates/djinn-graph/src/communities.rs",
                Some("server"),
            ),
            mk_node(
                "derive_label",
                "server/crates/djinn-graph/src/communities.rs",
                Some("server"),
            ),
            mk_node(
                "tokenize_identifier",
                "server/crates/djinn-graph/src/communities.rs",
                Some("server"),
            ),
            // djinn-auth cluster
            mk_node(
                "auth_login",
                "server/crates/djinn-auth/src/login.rs",
                Some("server"),
            ),
            mk_node(
                "auth_session",
                "server/crates/djinn-auth/src/session.rs",
                Some("server"),
            ),
            mk_node(
                "auth_token",
                "server/crates/djinn-auth/src/token.rs",
                Some("server"),
            ),
            // djinn-billing cluster
            mk_node(
                "billing_charge",
                "server/crates/djinn-billing/src/charge.rs",
                Some("server"),
            ),
            mk_node(
                "billing_invoice",
                "server/crates/djinn-billing/src/invoice.rs",
                Some("server"),
            ),
            mk_node(
                "billing_refund",
                "server/crates/djinn-billing/src/refund.rs",
                Some("server"),
            ),
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
            // graph cluster: triangle
            edge(0, 1, 5.0),
            edge(1, 0, 5.0),
            edge(1, 2, 5.0),
            edge(2, 1, 5.0),
            edge(0, 2, 5.0),
            edge(2, 0, 5.0),
            // auth cluster: triangle
            edge(3, 4, 5.0),
            edge(4, 3, 5.0),
            edge(4, 5, 5.0),
            edge(5, 4, 5.0),
            edge(3, 5, 5.0),
            edge(5, 3, 5.0),
            // billing cluster: triangle
            edge(6, 7, 5.0),
            edge(7, 6, 5.0),
            edge(7, 8, 5.0),
            edge(8, 7, 5.0),
            edge(6, 8, 5.0),
            edge(8, 6, 5.0),
            // Thin bridges
            edge(2, 3, 0.5),
            edge(3, 2, 0.5),
            edge(5, 6, 0.5),
            edge(6, 5, 0.5),
        ];

        let artifact = RepoGraphArtifact {
            version: REPO_GRAPH_ARTIFACT_VERSION,
            nodes,
            edges,
            symbol_ranges: BTreeMap::new(),
            communities: Vec::new(),
            processes: Vec::new(),
            route_exclusion_config: Default::default(),
            layout_positions: BTreeMap::new(),
        };
        RepoDependencyGraph::from_artifact(&artifact)
    }

    #[test]
    fn monorepo_labels_are_not_all_server() {
        let graph = monorepo_cluster_graph();
        let communities = detect_communities(&graph);
        assert!(
            communities.len() >= 2,
            "expected at least two communities, got {}",
            communities.len()
        );
        // No community should be labeled "server" — the workspace root
        // segment should be skipped.
        for c in &communities {
            assert_ne!(
                c.label, "server",
                "community should not be labeled 'server' (workspace root): {:?}",
                c.label
            );
        }
    }

    #[test]
    fn monorepo_labels_are_pairwise_distinct() {
        let graph = monorepo_cluster_graph();
        let communities = detect_communities(&graph);
        let labels: Vec<&str> = communities.iter().map(|c| c.label.as_str()).collect();
        let mut unique: Vec<&str> = labels.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            labels.len(),
            unique.len(),
            "community labels should be pairwise distinct, got: {:?}",
            labels
        );
    }

    #[test]
    fn label_falls_back_to_keywords_when_paths_are_not_distinguishing() {
        use crate::repo_graph::{
            REPO_GRAPH_ARTIFACT_VERSION, RepoGraphArtifact, RepoGraphArtifactEdge,
        };

        let mk_node = |name: &str| RepoGraphNode {
            id: RepoNodeKey::Symbol(format!("symbol:{name}")),
            kind: RepoGraphNodeKind::Symbol,
            display_name: name.to_string(),
            language: Some("rust".to_string()),
            file_path: Some(PathBuf::from("server/crates/djinn-payments/src/lib.rs")),
            symbol: Some(format!("symbol:{name}")),
            symbol_kind: None,
            is_external: false,
            visibility: None,
            signature: None,
            documentation: vec![],
            signature_parts: None,
            is_test: false,
            complexity: None,
            workspace: Some("server".to_string()),
            route_framework: None,
            route_handler_symbol: None,
        };

        let edge = |s, t| RepoGraphArtifactEdge {
            source: s,
            target: t,
            kind: RepoGraphEdgeKind::SymbolReference,
            weight: 5.0,
            evidence_count: 1,
            confidence: 0.9,
            reason: None,
            step: None,
        };

        let artifact = RepoGraphArtifact {
            version: REPO_GRAPH_ARTIFACT_VERSION,
            nodes: vec![
                mk_node("payments_processor"),
                mk_node("payments_gateway"),
                mk_node("payments_refund"),
            ],
            edges: vec![edge(0, 1), edge(1, 0), edge(1, 2), edge(2, 1)],
            symbol_ranges: BTreeMap::new(),
            communities: Vec::new(),
            processes: Vec::new(),
            route_exclusion_config: Default::default(),
            layout_positions: BTreeMap::new(),
        };
        let graph = RepoDependencyGraph::from_artifact(&artifact);

        assert_eq!(derive_label(&graph, &[0, 1, 2]), "payments");
    }

    #[test]
    fn community_count_not_collapsed() {
        let graph = monorepo_cluster_graph();
        let communities = detect_communities(&graph);
        // With 9 nodes in 3 tight clusters, we should get at least 3
        // communities (not collapsed into one or two giant communities).
        assert!(
            communities.len() >= 3,
            "community count should not collapse for a 3-crate fixture, got {}",
            communities.len()
        );
    }

    #[test]
    fn resolution_fine_produces_more_communities() {
        let graph = two_cluster_graph();
        let coarse = detect_communities_with_resolution(&graph, Resolution::Coarse);
        let fine = detect_communities_with_resolution(&graph, Resolution::Fine);
        // Fine should produce at least as many communities as coarse
        // (min size 1 vs 4).
        assert!(
            fine.len() >= coarse.len(),
            "fine resolution should produce >= coarse communities: fine={}, coarse={}",
            fine.len(),
            coarse.len()
        );
    }

    #[test]
    fn community_detection_options_default_is_unseeded_medium() {
        let opts = CommunityDetectionOptions::default();
        assert_eq!(opts.resolution, Resolution::Medium);
        assert!(opts.seed_by_crate.is_none());
    }

    #[test]
    fn detect_communities_with_options_unseeded_matches_default() {
        let graph = two_cluster_graph();
        let legacy = detect_communities(&graph);
        let via_options =
            detect_communities_with_options(&graph, CommunityDetectionOptions::default());
        assert_eq!(
            legacy, via_options,
            "unseeded options path must be identical to the legacy entry point"
        );
    }

    #[test]
    fn detect_communities_with_options_resolution_matches_legacy() {
        let graph = two_cluster_graph();
        for resolution in [Resolution::Fine, Resolution::Medium, Resolution::Coarse] {
            let legacy = detect_communities_with_resolution(&graph, resolution);
            let via_options = detect_communities_with_options(
                &graph,
                CommunityDetectionOptions {
                    resolution,
                    seed_by_crate: None,
                },
            );
            assert_eq!(
                legacy, via_options,
                "resolution {:?}: options path must match legacy wrapper",
                resolution
            );
        }
    }

    #[test]
    fn resolve_crate_for_node_longest_prefix_wins() {
        let mut crate_map = BTreeMap::new();
        crate_map.insert(PathBuf::from("src"), "root".to_string());
        crate_map.insert(PathBuf::from("src/auth"), "auth".to_string());

        // Longer prefix wins over the shorter parent.
        let resolved =
            resolve_crate_for_node(Some(std::path::Path::new("src/auth/login.rs")), &crate_map);
        assert_eq!(resolved, Some("auth"));

        // No file_path → None (no resolution possible).
        assert_eq!(resolve_crate_for_node(None, &crate_map), None);

        // Path outside any known crate prefix → None.
        assert_eq!(
            resolve_crate_for_node(Some(std::path::Path::new("other/x.rs")), &crate_map),
            None,
        );
    }

    #[test]
    fn seed_partition_by_crate_groups_crate_mates() {
        let graph = two_cluster_graph();
        let mut crate_map = BTreeMap::new();
        crate_map.insert(PathBuf::from("src/auth"), "auth".to_string());
        crate_map.insert(PathBuf::from("src/billing"), "billing".to_string());

        let partition = seed_partition_by_crate(&graph, 6, &crate_map);
        assert_eq!(partition.len(), 6);

        // auth cluster (nodes 0,1,2) share one community ...
        assert_eq!(partition[0], partition[1]);
        assert_eq!(partition[1], partition[2]);
        // billing cluster (nodes 3,4,5) share another ...
        assert_eq!(partition[3], partition[4]);
        assert_eq!(partition[4], partition[5]);
        // ... and the two crates are distinct.
        assert_ne!(partition[0], partition[3]);
    }

    #[test]
    fn seed_partition_by_crate_merges_paths_sharing_a_crate_name() {
        let graph = two_cluster_graph();
        // Two different prefixes map to the SAME crate name → one seed
        // community, exercising the name-based grouping.
        let mut crate_map = BTreeMap::new();
        crate_map.insert(PathBuf::from("src/auth"), "monolib".to_string());
        crate_map.insert(PathBuf::from("src/billing"), "monolib".to_string());

        let partition = seed_partition_by_crate(&graph, 6, &crate_map);
        let first = partition[0];
        for v in 0..6 {
            assert_eq!(
                partition[v], first,
                "node {v} should share the single monolib seed community"
            );
        }
    }

    #[test]
    fn seed_partition_by_crate_empty_map_leaves_singletons() {
        let graph = two_cluster_graph();
        // Empty crate_map → nothing matches → every node is its own
        // singleton community (degrades to the unseeded initial state).
        let crate_map = BTreeMap::new();
        let partition = seed_partition_by_crate(&graph, 6, &crate_map);

        let mut ids = partition.clone();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 6, "each node should be a singleton community");
    }

    #[test]
    fn seed_partition_by_crate_nodes_outside_crate_are_singletons() {
        let graph = two_cluster_graph();
        // Map only auth; billing nodes (3,4,5) match no crate prefix.
        let mut crate_map = BTreeMap::new();
        crate_map.insert(PathBuf::from("src/auth"), "auth".to_string());

        let partition = seed_partition_by_crate(&graph, 6, &crate_map);

        // auth nodes grouped together.
        assert_eq!(partition[0], partition[1]);
        assert_eq!(partition[1], partition[2]);

        // Each billing node gets a distinct singleton id, all different
        // from the auth community and from each other.
        assert_ne!(partition[3], partition[0]);
        assert_ne!(partition[4], partition[0]);
        assert_ne!(partition[5], partition[0]);
        assert_ne!(partition[3], partition[4]);
        assert_ne!(partition[4], partition[5]);
        assert_ne!(partition[3], partition[5]);
    }

    #[test]
    fn detect_communities_with_options_seeded_keeps_crate_purity() {
        let graph = two_cluster_graph();
        let mut crate_map = BTreeMap::new();
        crate_map.insert(PathBuf::from("src/auth"), "auth".to_string());
        crate_map.insert(PathBuf::from("src/billing"), "billing".to_string());

        let communities = detect_communities_with_options(
            &graph,
            CommunityDetectionOptions {
                resolution: Resolution::Medium,
                seed_by_crate: Some(crate_map),
            },
        );

        // Seeding should keep the auth cluster predominantly together.
        let auth_comm = communities
            .iter()
            .find(|c| c.member_ids.contains(&0))
            .expect("auth_login should belong to some community");
        let auth_in_same = (0..3).filter(|v| auth_comm.member_ids.contains(v)).count();
        assert!(
            auth_in_same >= 2,
            "seeding should keep ≥2/3 auth nodes together, got {auth_in_same}: {:?}",
            auth_comm.member_ids
        );
    }
}
