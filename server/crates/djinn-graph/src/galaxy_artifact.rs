//! Out-of-transaction producer for one full-warm galaxy snapshot artifact.
//!
//! The caller supplies a pre-reserved UUIDv7 ([`GenerationId`]). The producer
//! emits the *compatible* canonical snapshot JSON (the same wire shape the
//! unchanged old `SnapshotPayload` bridge reader consumes), the
//! `graph_content_hash` computed over the canonical hash-input bytes, an
//! ordered bounded gzip spool of chunks each at most [`CHUNK_MAX_BYTES`], the
//! ordered per-chunk SHA-256 values, the total compressed byte count, and the
//! `transport_sha256` over the concatenated gzip bytes.
//!
//! ## Hash domains (three, never conflated)
//!
//! 1. **Semantic** (`graph_content_hash`) — SHA-256 over the canonical
//!    hash-input JSON. The hash input includes `generation_id`, omits *only*
//!    `graph_content_hash`, and uses stable semantic ordering (nodes by
//!    pagerank-desc then id; edges by kind then from then to). The identity
//!    participates in the hash, so changing only the generation identity
//!    changes `graph_content_hash`.
//! 2. **Per-chunk** — SHA-256 over each individual gzip chunk's bytes.
//! 3. **Transport** (`transport_sha256`) — SHA-256 over the exact
//!    concatenation of all gzip chunk bytes.
//!
//! The transport hash **never** enters the payload JSON. The final payload
//! JSON carries `graph_content_hash` but never `transport_sha256`.
//!
//! ## Incremental gzip spool
//!
//! The final payload is serialized to bytes once, then streamed through a gzip
//! encoder into a bounded spool. As compressed bytes are emitted they are
//! accumulated into a [`CHUNK_MAX_BYTES`]-sized buffer; whenever the buffer
//! fills, a chunk is finalized (its SHA-256 computed, the transport digest
//! updated) and a fresh buffer started. No second full compressed-payload
//! allocation is made: only one chunk-sized buffer is live at a time.
//!
//! ## Compatibility
//!
//! The payload/node/edge types here are wire-compatible mirrors of
//! `djinn_control_plane::bridge::{SnapshotPayload,SnapshotNode,SnapshotEdge}`
//! (see `server/src/mcp_bridge/snapshot.rs` and
//! `server/crates/djinn-control-plane/src/bridge/graph_data.rs`). They reuse
//! the exact field names, ordering conventions, and `skip_serializing_if`
//! rules so the canonical JSON is byte-identical in shape to what the old
//! server produces. `djinn-graph` deliberately does **not** depend on
//! `djinn-control-plane`, so the types are duplicated here rather than
//! imported. The public bridge types are unchanged.

use flate2::Compression;
use petgraph::graph::NodeIndex;
use petgraph::visit::EdgeRef;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::cochange::encode_reason;
use crate::repo_graph::RepoDependencyGraph;
use crate::repo_graph::RepoGraphEdgeKind;
use crate::scip_parser::prettify_scip_descriptor;

/// Maximum byte length of a single gzip chunk in the bounded spool (256 KiB).
pub const CHUNK_MAX_BYTES: usize = 262_144;

/// Format the canonical node key string, mirroring `format_node_key` in
/// `server/src/mcp_bridge/graph_neighbors.rs` and
/// `RepoNodeKey::stable_uid`.
fn format_node_key(key: &crate::repo_graph::RepoNodeKey) -> String {
    key.stable_uid()
}

/// Wraps a caller-reserved UUIDv7 that is the identity of this galaxy
/// generation. Construction validates that the value is a real UUIDv7
/// (version nibble 7); a non-v7 UUID is rejected up front because identity
/// participates in the semantic hash and the publication contract reserves
/// the id before the producer runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GenerationId(uuid::Uuid);

impl GenerationId {
    /// Wrap a pre-reserved [`uuid::Uuid`], rejecting anything that is not a
    /// version-7 UUID.
    pub fn new(id: uuid::Uuid) -> Result<Self, InvalidGenerationId> {
        if id.get_version() == Some(uuid::Version::SortRand) {
            // uuid 1.x names the v7 variant `SortRand`.
            Ok(Self(id))
        } else {
            Err(InvalidGenerationId {
                id,
                reason: "expected a UUIDv7 (time-ordered) generation identity",
            })
        }
    }

    /// Parse a hyphenated UUIDv7 string.
    pub fn parse(s: &str) -> Result<Self, InvalidGenerationId> {
        let id = uuid::Uuid::parse_str(s).map_err(|_| InvalidGenerationId::unparseable(s))?;
        Self::new(id)
    }

    /// The underlying UUIDv7.
    pub fn as_uuid(&self) -> uuid::Uuid {
        self.0
    }

    /// Hyphenated canonical string form.
    pub fn as_str(&self) -> String {
        self.0.to_string()
    }
}

impl std::fmt::Display for GenerationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// A generation identity was not a valid UUIDv7.
#[derive(Debug, Clone, Error)]
#[error("invalid generation id {id}: {reason}")]
pub struct InvalidGenerationId {
    id: uuid::Uuid,
    reason: &'static str,
}

impl InvalidGenerationId {
    fn unparseable(_raw: &str) -> Self {
        // Keep the raw out of the error to avoid leaking an arbitrary string
        // into a typed error; the caller already has it.
        Self {
            id: uuid::Uuid::nil(),
            reason: "not a parseable UUID",
        }
    }
}

// ── Wire-compatible payload types ───────────────────────────────────────────
// These mirror djinn_control_plane::bridge::{SnapshotNode,SnapshotEdge,
// SnapshotPayload} field-for-field so the canonical JSON matches the old
// server reader byte-for-byte. The serde attributes (skip_serializing_if,
// rename, defaults) are copied from graph_data.rs.

/// Canonical galaxy snapshot node. Wire-compatible with
/// `djinn_control_plane::bridge::SnapshotNode`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GalaxySnapshotNode {
    pub id: String,
    #[serde(default)]
    pub uid: String,
    pub kind: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub internal_edge_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    pub pagerank: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub community_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cognitive: Option<u16>,
    #[serde(default)]
    pub is_test: bool,
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gx: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gy: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gz: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degree: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
}

/// Canonical galaxy snapshot edge. Wire-compatible with
/// `djinn_control_plane::bridge::SnapshotEdge`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GalaxySnapshotEdge {
    pub from: String,
    pub to: String,
    pub kind: String,
    pub confidence: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Canonical galaxy snapshot payload. Wire-compatible with
/// `djinn_control_plane::bridge::SnapshotPayload` plus the two additive
/// identity/hash fields (`generation_id`, `graph_content_hash`).
///
/// `transport_sha256` is intentionally **never** a field here — it must not
/// enter payload JSON.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GalaxySnapshotPayload {
    pub project_id: String,
    pub git_head: String,
    pub generated_at: String,
    /// Caller-reserved UUIDv7 identity of this generation. Participates in the
    /// semantic hash.
    pub generation_id: String,
    /// SHA-256 over the canonical hash-input JSON (this payload with this
    /// field omitted). Present in the final payload; absent from hash input.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_content_hash: Option<String>,
    pub truncated: bool,
    pub total_nodes: usize,
    pub total_edges: usize,
    pub node_cap: usize,
    pub nodes: Vec<GalaxySnapshotNode>,
    pub edges: Vec<GalaxySnapshotEdge>,
}

/// One bounded gzip chunk in the artifact spool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GalaxyChunk {
    /// 0-based contiguous index.
    pub index: u32,
    /// Compressed payload bytes (at most [`CHUNK_MAX_BYTES`]).
    pub bytes: Vec<u8>,
    /// SHA-256 hex over `bytes`.
    pub sha256: String,
}

/// The ordered spool of gzip chunks plus the aggregate transport digest.
#[derive(Debug, Clone)]
pub struct GalaxyArtifactSpool {
    /// Contiguous chunks, each at most [`CHUNK_MAX_BYTES`] bytes.
    pub chunks: Vec<GalaxyChunk>,
    /// Ordered per-chunk SHA-256 hex values.
    pub chunk_hashes: Vec<String>,
    /// Total compressed byte count across all chunks.
    pub total_compressed_bytes: u64,
    /// SHA-256 hex over the exact concatenation of all chunk bytes.
    pub transport_sha256: String,
}

/// The full out-of-transaction artifact for one full-warm galaxy generation.
#[derive(Debug, Clone)]
pub struct GalaxyArtifact {
    /// Caller-reserved generation identity echoed back.
    pub generation_id: GenerationId,
    /// Canonical hash-input JSON bytes (includes generation_id, omits
    /// graph_content_hash). Exposed for golden-test recomputation.
    pub hash_input_json: Vec<u8>,
    /// SHA-256 hex over `hash_input_json`.
    pub graph_content_hash: String,
    /// Final compatible payload JSON bytes (includes graph_content_hash,
    /// omits transport_sha256).
    pub payload_json: Vec<u8>,
    /// Bounded gzip spool of the final payload JSON.
    pub spool: GalaxyArtifactSpool,
}

/// A configurable upper bound on the total compressed artifact size.
#[derive(Debug, Clone, Copy)]
pub struct ArtifactSizeCap {
    max_compressed_bytes: u64,
}

impl ArtifactSizeCap {
    /// Build a cap expressed in compressed bytes.
    pub fn compressed_bytes(max_compressed_bytes: u64) -> Self {
        Self {
            max_compressed_bytes,
        }
    }

    /// Build a cap expressed in mebibytes.
    pub fn mib(mib: u64) -> Self {
        Self::compressed_bytes(mib * 1024 * 1024)
    }

    /// The configured maximum compressed byte count.
    pub fn max_compressed_bytes(&self) -> u64 {
        self.max_compressed_bytes
    }
}

impl Default for ArtifactSizeCap {
    fn default() -> Self {
        // 64 MiB default — generous for a single full-warm galaxy snapshot
        // while still bounding pathological growth before any DB work.
        Self::mib(64)
    }
}

/// Input needed to build a galaxy artifact.
#[derive(Debug, Clone)]
pub struct GalaxyArtifactInput<'a> {
    /// The full-warm graph (with warm-time layout + galaxy sidecars).
    pub graph: &'a RepoDependencyGraph,
    /// Project id (UUID).
    pub project_id: String,
    /// Git head commit sha this graph was built from.
    pub git_head: String,
    /// ISO-8601 UTC timestamp at which the artifact is assembled.
    pub generated_at: String,
    /// Caller-reserved UUIDv7 generation identity.
    pub generation_id: GenerationId,
    /// Upper bound on total compressed bytes before publication inputs are
    /// exposed.
    pub size_cap: ArtifactSizeCap,
}

/// Typed producer failure. None of the variants expose publication inputs.
#[derive(Debug, thiserror::Error)]
pub enum GalaxyArtifactError {
    /// The compressed artifact exceeded the configured size cap.
    #[error("galaxy artifact compressed size {actual} bytes exceeds cap {cap} bytes")]
    Oversize { actual: u64, cap: u64 },
    /// JSON serialization of the canonical payload failed.
    #[error("serialize canonical payload: {0}")]
    Serialize(#[from] serde_json::Error),
    /// The gzip encoder failed to flush.
    #[error("gzip spool: {0}")]
    Gzip(#[from] std::io::Error),
}

/// Build the full-warm galaxy snapshot artifact.
///
/// See the module docs for the hash-domain and spool semantics. This function
/// performs no database work and allocates at most one chunk-sized buffer at
/// a time during gzip spooling.
pub fn build_galaxy_artifact(
    input: GalaxyArtifactInput<'_>,
) -> Result<GalaxyArtifact, GalaxyArtifactError> {
    let GalaxyArtifactInput {
        graph,
        project_id,
        git_head,
        generated_at,
        generation_id,
        size_cap,
    } = input;

    // 1. Assemble the deterministic semantic model (nodes + edges) in stable
    //    order. The full warm snapshot includes every eligible node and every
    //    eligible edge — no UI node cap, since this is the canonical full
    //    galaxy artifact the galaxy renderer and every reader consume.
    let (nodes, edges, total_nodes, total_edges) = build_semantic_model(graph);

    // 2. Serialize the hash-input payload: generation_id included,
    //    graph_content_hash omitted, stable semantic ordering.
    let hash_input = GalaxySnapshotPayload {
        project_id: project_id.clone(),
        git_head: git_head.clone(),
        generated_at: generated_at.clone(),
        generation_id: generation_id.as_str(),
        graph_content_hash: None,
        truncated: false,
        total_nodes,
        total_edges,
        node_cap: total_nodes,
        nodes: nodes.clone(),
        edges: edges.clone(),
    };
    // Deterministic serialization: compact, no trailing newline, stable key
    // order from the struct field order.
    let hash_input_json = serde_json::to_vec(&hash_input)?;
    let mut hasher = Sha256::new();
    hasher.update(&hash_input_json);
    let graph_content_hash = hex::encode(hasher.finalize());

    // 3. Serialize the final compatible payload: graph_content_hash present,
    //    transport_sha256 never present.
    let final_payload = GalaxySnapshotPayload {
        project_id,
        git_head,
        generated_at,
        generation_id: generation_id.as_str(),
        graph_content_hash: Some(graph_content_hash.clone()),
        truncated: false,
        total_nodes,
        total_edges,
        node_cap: total_nodes,
        nodes,
        edges,
    };
    let payload_json = serde_json::to_vec(&final_payload)?;

    // 4. Incrementally gzip-spool the final payload into bounded chunks,
    //    computing per-chunk and transport digests without a second full
    //    compressed-payload allocation.
    let spool = spool_gzip(&payload_json, size_cap)?;

    Ok(GalaxyArtifact {
        generation_id,
        hash_input_json,
        graph_content_hash,
        payload_json,
        spool,
    })
}

/// Containment edge kinds — mirrors `is_containment_edge_kind` in
/// `server/src/mcp_bridge/snapshot.rs`.
fn is_containment_edge_kind(kind: RepoGraphEdgeKind) -> bool {
    matches!(
        kind,
        RepoGraphEdgeKind::ContainsDefinition
            | RepoGraphEdgeKind::DeclaredInFile
            | RepoGraphEdgeKind::MemberOf
    )
}

/// Build the deterministic semantic model from the full graph.
///
/// Nodes are sorted by pagerank-desc then id (matching the existing snapshot
/// convention). Edges are sorted by kind > from > to. Co-change edges are
/// appended after structural edges, mirroring the snapshot builder.
fn build_semantic_model(
    graph: &RepoDependencyGraph,
) -> (
    Vec<GalaxySnapshotNode>,
    Vec<GalaxySnapshotEdge>,
    usize,
    usize,
) {
    use std::collections::{HashMap, HashSet};

    // Compute the ranking once so node ordering + pagerank values match the
    // snapshot builder convention. The ranking excludes route/tool nodes, so
    // nodes outside the ranking get pagerank 0.0 (matching the snapshot
    // builder's fallback walk).
    let ranking = graph.rank();
    let mut pagerank_lookup: HashMap<NodeIndex, f64> = HashMap::new();
    for ranked in &ranking.nodes {
        pagerank_lookup.insert(ranked.node_index, ranked.page_rank);
    }

    // Tally eligible nodes: exclude route/tool nodes and external nodes,
    // matching the existing snapshot eligibility filter.
    let mut eligible: Vec<NodeIndex> = Vec::new();
    for idx in graph.graph().node_indices() {
        let node = graph.node(idx);
        if node.is_route_or_tool() {
            continue;
        }
        if node.is_external {
            continue;
        }
        eligible.push(idx);
    }
    // Keep the exact eligibility lookup while emitting edges. The compatibility
    // snapshot drops every edge whose endpoint did not survive node filtering;
    // otherwise consumers receive dangling endpoints absent from `nodes`.
    let eligible_set: HashSet<NodeIndex> = eligible.iter().copied().collect();
    let total_nodes = eligible.len();

    let mut snapshot_nodes: Vec<GalaxySnapshotNode> = eligible
        .into_iter()
        .map(|idx| {
            let node = graph.node(idx);
            let pagerank = pagerank_lookup.get(&idx).copied().unwrap_or(0.0);
            let label = prettify_scip_descriptor(&node.display_name);
            GalaxySnapshotNode {
                id: format_node_key(&node.id),
                uid: format_node_key(&node.id),
                kind: format!("{:?}", node.kind).to_lowercase(),
                label,
                workspace: node.workspace.clone(),
                workspace_kind: None,
                member_count: None,
                internal_edge_count: None,
                symbol_kind: node
                    .symbol_kind
                    .as_ref()
                    .map(|k| format!("{k:?}").to_lowercase()),
                file_path: node.file_path.as_ref().map(|p| p.display().to_string()),
                pagerank,
                community_id: graph.community_id(idx).map(str::to_string),
                cognitive: node.complexity.map(|c| c.cognitive),
                is_test: node.is_test,
                x: graph.layout_position(idx).map(|p| p.x).unwrap_or_default(),
                y: graph.layout_position(idx).map(|p| p.y).unwrap_or_default(),
                gx: graph.galaxy_position(idx).map(|p| p.x),
                gy: graph.galaxy_position(idx).map(|p| p.y),
                gz: graph.galaxy_position(idx).map(|p| p.z),
                degree: graph.galaxy_degree(idx),
                keywords: Vec::new(),
            }
        })
        .collect();
    // Stable ordering: pagerank desc (tie-broken by id asc) — matches the
    // snapshot builder convention.
    snapshot_nodes.sort_by(|a, b| {
        b.pagerank
            .partial_cmp(&a.pagerank)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });

    // Build edges over the eligible graph (no cap for the canonical artifact).
    let mut total_edges: usize = 0;
    let mut containment_edges: Vec<GalaxySnapshotEdge> = Vec::new();
    let mut cross_workspace_edges: Vec<GalaxySnapshotEdge> = Vec::new();
    let mut drawable_edges: Vec<GalaxySnapshotEdge> = Vec::new();
    for edge_ref in graph.graph().edge_references() {
        if !eligible_set.contains(&edge_ref.source()) || !eligible_set.contains(&edge_ref.target())
        {
            continue;
        }
        let from_node = graph.node(edge_ref.source());
        let to_node = graph.node(edge_ref.target());
        total_edges += 1;
        let weight = edge_ref.weight();
        let edge = GalaxySnapshotEdge {
            from: format_node_key(&from_node.id),
            to: format_node_key(&to_node.id),
            kind: format!("{:?}", weight.kind),
            confidence: weight.confidence,
            reason: weight.reason.clone(),
        };
        if is_containment_edge_kind(weight.kind) {
            containment_edges.push(edge);
        } else if from_node.workspace.is_some()
            && to_node.workspace.is_some()
            && from_node.workspace != to_node.workspace
        {
            cross_workspace_edges.push(edge);
        } else {
            drawable_edges.push(edge);
        }
    }

    // Co-change edges (sidecar, appended after structural edges — matches
    // the snapshot builder). Not counted in total_edges.
    let mut snapshot_edges: Vec<GalaxySnapshotEdge> = Vec::with_capacity(
        containment_edges.len() + cross_workspace_edges.len() + drawable_edges.len(),
    );
    snapshot_edges.append(&mut containment_edges);
    snapshot_edges.append(&mut cross_workspace_edges);
    snapshot_edges.append(&mut drawable_edges);
    for cc in graph.cochange_edges() {
        if !eligible_set.contains(&cc.source) || !eligible_set.contains(&cc.target) {
            continue;
        }
        let from_node = graph.node(cc.source);
        let to_node = graph.node(cc.target);
        snapshot_edges.push(GalaxySnapshotEdge {
            from: format_node_key(&from_node.id),
            to: format_node_key(&to_node.id),
            kind: format!("{:?}", RepoGraphEdgeKind::CoChangedWith),
            confidence: cc.confidence,
            reason: Some(encode_reason(cc.last_co_change)),
        });
    }

    // Deterministic edge ordering: kind > from > to — matches the snapshot
    // builder.
    snapshot_edges.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then_with(|| a.from.cmp(&b.from))
            .then_with(|| a.to.cmp(&b.to))
    });

    (snapshot_nodes, snapshot_edges, total_nodes, total_edges)
}

/// Incrementally gzip the payload bytes into a bounded chunk spool.
///
/// The payload is fed into a [`flate2::bufread::GzEncoder`] whose output is
/// drained into a single [`CHUNK_MAX_BYTES`]-sized buffer. Whenever the
/// buffer fills, the chunk is finalized (per-chunk SHA-256 computed, transport
/// digest updated) and the buffer is cleared and reused. The transport digest
/// is updated incrementally as compressed bytes are emitted, so no second full
/// compressed-payload allocation is ever live — only one chunk-sized buffer.
fn spool_gzip(
    payload: &[u8],
    size_cap: ArtifactSizeCap,
) -> Result<GalaxyArtifactSpool, GalaxyArtifactError> {
    use flate2::bufread::GzEncoder;
    use std::io::Read;

    let reader = std::io::BufReader::new(payload);
    let mut encoder = GzEncoder::new(reader, Compression::default());

    let mut chunks: Vec<GalaxyChunk> = Vec::new();
    let mut chunk_hashes: Vec<String> = Vec::new();
    let mut transport_hasher = Sha256::new();
    let mut total_compressed_bytes: u64 = 0;

    // One read buffer + one chunk accumulator, reused across chunks. Each fill
    // triggers a finalize + transport-digest update, then the accumulator is
    // cleared and reused. No second full compressed allocation is ever live.
    let mut read_buf = vec![0u8; CHUNK_MAX_BYTES];
    let mut current: Vec<u8> = Vec::with_capacity(CHUNK_MAX_BYTES);
    let mut index: u32 = 0;

    loop {
        let read = encoder.read(&mut read_buf)?;
        if read == 0 {
            break;
        }
        current.extend_from_slice(&read_buf[..read]);
        // While the accumulated buffer holds at least one full chunk, emit it.
        while current.len() >= CHUNK_MAX_BYTES {
            let chunk_bytes: Vec<u8> = current.drain(..CHUNK_MAX_BYTES).collect();
            let mut chunk_hasher = Sha256::new();
            chunk_hasher.update(&chunk_bytes);
            let sha256 = hex::encode(chunk_hasher.finalize());

            transport_hasher.update(&chunk_bytes);
            total_compressed_bytes += chunk_bytes.len() as u64;

            // Enforce the cap as we emit — fail before publication inputs are
            // exposed.
            if total_compressed_bytes > size_cap.max_compressed_bytes() {
                return Err(GalaxyArtifactError::Oversize {
                    actual: total_compressed_bytes,
                    cap: size_cap.max_compressed_bytes(),
                });
            }

            chunks.push(GalaxyChunk {
                index,
                bytes: chunk_bytes,
                sha256: sha256.clone(),
            });
            chunk_hashes.push(sha256);
            index += 1;
        }
    }

    // Emit the trailing partial chunk (if any). An exactly-aligned artifact
    // leaves `current` empty and emits no extra chunk.
    if !current.is_empty() {
        let mut chunk_hasher = Sha256::new();
        chunk_hasher.update(&current);
        let sha256 = hex::encode(chunk_hasher.finalize());

        transport_hasher.update(&current);
        total_compressed_bytes += current.len() as u64;

        if total_compressed_bytes > size_cap.max_compressed_bytes() {
            return Err(GalaxyArtifactError::Oversize {
                actual: total_compressed_bytes,
                cap: size_cap.max_compressed_bytes(),
            });
        }

        chunks.push(GalaxyChunk {
            index,
            bytes: current,
            sha256: sha256.clone(),
        });
        chunk_hashes.push(sha256);
    }

    let transport_sha256 = hex::encode(transport_hasher.finalize());

    Ok(GalaxyArtifactSpool {
        chunks,
        chunk_hashes,
        total_compressed_bytes,
        transport_sha256,
    })
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod publication_integration_tests;
