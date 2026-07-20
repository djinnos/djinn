//! On-demand producer for the operational memory-sustainability fixtures.
//! It imports the landed artifact types and bincode compatibility reader instead
//! of implementing a second repository or serializer.
use djinn_graph::{
    galaxy_artifact::{GalaxySnapshotNode, GalaxySnapshotPayload},
    repo_graph::{
        REPO_GRAPH_ARTIFACT_VERSION, RepoGraphArtifact, RepoGraphArtifactEdge, RepoGraphEdgeKind,
        RepoGraphNode, RepoGraphNodeKind, RepoNodeKey, RouteExclusionConfig,
        deserialize_repo_graph_artifact_bincode,
    },
};
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

const GENERATION_ID: &str = "018f7e8a-0000-7000-8000-000000000001";
fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn node(index: usize, documentation: Vec<String>) -> RepoGraphNode {
    RepoGraphNode {
        id: RepoNodeKey::Symbol(format!("memory-sustainability::node::{index}")),
        kind: RepoGraphNodeKind::Symbol,
        display_name: format!("memory_sustainability_node_{index}"),
        language: Some("rust".into()),
        file_path: Some(PathBuf::from(format!("src/fixture/{index}.rs"))),
        symbol: Some(format!("fixture::{index}")),
        symbol_kind: None,
        is_external: false,
        visibility: None,
        signature: None,
        documentation,
        signature_parts: None,
        is_test: false,
        complexity: None,
        workspace: Some("memory-sustainability".into()),
        route_framework: None,
        route_handler_symbol: None,
    }
}
fn graph(nodes: usize, edges: usize, final_doc: Vec<String>) -> RepoGraphArtifact {
    let graph_nodes = (0..nodes)
        .map(|i| {
            node(
                i,
                if i + 1 == nodes {
                    final_doc.clone()
                } else {
                    Vec::new()
                },
            )
        })
        .collect();
    let graph_edges = (0..edges)
        .map(|i| RepoGraphArtifactEdge {
            source: i % nodes,
            target: (i.wrapping_mul(17).wrapping_add(1)) % nodes,
            kind: RepoGraphEdgeKind::SymbolReference,
            weight: 1.0,
            evidence_count: 1,
            confidence: 0.9,
            reason: Some("memory-sustainability-fixture".into()),
            step: None,
        })
        .collect();
    RepoGraphArtifact {
        version: REPO_GRAPH_ARTIFACT_VERSION,
        nodes: graph_nodes,
        edges: graph_edges,
        symbol_ranges: BTreeMap::new(),
        communities: Vec::new(),
        processes: Vec::new(),
        route_exclusion_config: RouteExclusionConfig::default(),
        layout_positions: BTreeMap::new(),
        galaxy_positions: BTreeMap::new(),
        galaxy_degrees: BTreeMap::new(),
    }
}
fn graph_bytes(nodes: usize, edges: usize, required: usize) -> Vec<u8> {
    let base = bincode::serialize(&graph(nodes, edges, Vec::new())).expect("serialize graph base");
    assert!(
        base.len() <= required,
        "graph schema base exceeds requested size"
    );
    // A one-element Vec<String> adds its bincode string-length prefix beyond
    // the empty Vec already present in `base`.
    let string_length_prefix = std::mem::size_of::<u64>();
    assert!(
        base.len() + string_length_prefix <= required,
        "graph target too small"
    );
    let result = bincode::serialize(&graph(
        nodes,
        edges,
        vec!["x".repeat(required - base.len() - string_length_prefix)],
    ))
    .expect("serialize graph");
    assert_eq!(result.len(), required, "bincode graph padding drifted");
    result
}
fn payload(padding: usize) -> (Vec<u8>, String) {
    let node = GalaxySnapshotNode {
        id: "symbol:memory-sustainability::artifact-padding".into(),
        uid: "symbol:memory-sustainability::artifact-padding".into(),
        kind: "Symbol".into(),
        label: "x".repeat(padding),
        workspace: Some("memory-sustainability".into()),
        workspace_kind: None,
        member_count: None,
        internal_edge_count: None,
        symbol_kind: None,
        file_path: None,
        pagerank: 1.0,
        community_id: None,
        cognitive: None,
        is_test: false,
        x: 0.0,
        y: 0.0,
        gx: None,
        gy: None,
        gz: None,
        degree: None,
        keywords: Vec::new(),
    };
    let input = GalaxySnapshotPayload {
        project_id: "00000000-0000-7000-8000-000000000001".into(),
        git_head: "memory-sustainability-fixture".into(),
        generated_at: "2026-01-01T00:00:00Z".into(),
        generation_id: GENERATION_ID.into(),
        graph_content_hash: None,
        truncated: false,
        total_nodes: 1,
        total_edges: 0,
        node_cap: 1,
        nodes: vec![node],
        edges: Vec::new(),
    };
    let content_hash = hash(&serde_json::to_vec(&input).expect("serialize hash input"));
    (
        serde_json::to_vec(&GalaxySnapshotPayload {
            graph_content_hash: Some(content_hash.clone()),
            ..input
        })
        .expect("serialize payload"),
        content_hash,
    )
}
fn gzip_target(total: usize) -> (Vec<u8>, String) {
    // Stored DEFLATE blocks add a small framing cost. Measure the landed flate2
    // transport and correct JSON padding to make the compressed size exact.
    let (empty, _) = payload(0);
    let mut padding = total
        .checked_sub(18 + empty.len())
        .expect("artifact target too small");
    for _ in 0..3 {
        let (payload, graph_content_hash) = payload(padding);
        let mut encoder = GzEncoder::new(Vec::new(), Compression::none());
        encoder.write_all(&payload).expect("gzip payload");
        let compressed = encoder.finish().expect("finish gzip");
        let drift = compressed.len() as isize - total as isize;
        if drift == 0 {
            return (compressed, graph_content_hash);
        }
        padding = padding
            .checked_add_signed(-drift)
            .expect("artifact padding correction overflow");
    }
    panic!("gzip stored-block size drifted");
}
fn generate(
    out: &Path,
    nodes: usize,
    edges: usize,
    graph_size: usize,
    chunks: usize,
    total: usize,
) {
    fs::create_dir_all(out.join("galaxy-artifact")).expect("create output");
    fs::write(
        out.join("canonical-graph.blob"),
        graph_bytes(nodes, edges, graph_size),
    )
    .expect("write graph");
    let (transport, graph_content_hash) = gzip_target(total);
    assert_eq!(transport.len() % chunks, 0);
    let chunk_bytes = transport.len() / chunks;
    let hashes: Vec<String> = transport.chunks(chunk_bytes).map(hash).collect();
    for (i, chunk) in transport.chunks(chunk_bytes).enumerate() {
        fs::write(
            out.join("galaxy-artifact")
                .join(format!("chunk-{i:05}.bin")),
            chunk,
        )
        .expect("write chunk");
    }
    let manifest = json!({"schema":"galaxy-artifact-spool-fixture/v2","artifact_version":1,"encoding":"gzip","generation_id":GENERATION_ID,"artifact_id":GENERATION_ID,"graph_content_hash":graph_content_hash,"chunk_count":chunks,"byte_count":total,"chunk_hashes":hashes,"transport_sha256":hash(&transport)});
    fs::write(
        out.join("galaxy-artifact/manifest.json"),
        serde_json::to_vec_pretty(&manifest).expect("manifest"),
    )
    .expect("write manifest");
}
fn validate(out: &Path, nodes: usize, edges: usize) {
    let bytes = fs::read(out.join("canonical-graph.blob")).expect("read graph");
    let graph = deserialize_repo_graph_artifact_bincode(&bytes)
        .expect("deserialize landed RepoGraphArtifact");
    assert_eq!(graph.nodes.len(), nodes, "graph node count drift");
    assert_eq!(graph.edges.len(), edges, "graph edge count drift");
    let mut transport = Vec::new();
    for i in 0.. {
        let path = out
            .join("galaxy-artifact")
            .join(format!("chunk-{i:05}.bin"));
        if !path.exists() {
            break;
        }
        transport.extend(fs::read(path).expect("read chunk"));
    }
    let mut decoded = Vec::new();
    std::io::Read::read_to_end(&mut GzDecoder::new(transport.as_slice()), &mut decoded)
        .expect("decode gzip transport");
    let payload: GalaxySnapshotPayload =
        serde_json::from_slice(&decoded).expect("decode landed galaxy payload");
    let input = GalaxySnapshotPayload {
        graph_content_hash: None,
        ..payload.clone()
    };
    let expected_content_hash = hash(&serde_json::to_vec(&input).expect("hash input"));
    assert_eq!(
        payload.graph_content_hash.as_deref(),
        Some(expected_content_hash.as_str()),
        "graph content hash drift"
    );
}
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 8 {
        panic!("usage: fixture <generate|validate> OUT NODES EDGES GRAPH_BYTES CHUNKS TOTAL_BYTES");
    }
    let n = |i: usize| args[i].parse::<usize>().expect("numeric argument");
    match args[1].as_str() {
        "generate" => generate(Path::new(&args[2]), n(3), n(4), n(5), n(6), n(7)),
        "validate" => validate(Path::new(&args[2]), n(3), n(4)),
        _ => panic!("unknown fixture action"),
    }
}
