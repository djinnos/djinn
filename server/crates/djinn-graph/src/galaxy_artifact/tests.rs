//! Tests for the galaxy artifact producer.
//!
//! Golden fixtures independently decompress the gzip chunks, recompute the
//! semantic, per-chunk, and transport hash domains, and validate the
//! decompressed compatible JSON — they do not merely compare producer fields.

use super::*;

use sha2::{Digest, Sha256};

fn hex_sha256(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

fn test_graph() -> RepoDependencyGraph {
    crate::test_helpers::td55_equivalence_fixture_graph()
}

fn generation_id() -> GenerationId {
    // A fixed, well-formed UUIDv7 (version nibble 7, variant 10xx).
    // 019f741c-0000-7000-8000-000000000000 — version nibble (char index 14)
    // is '7', variant bits are '8' (1000).
    GenerationId::parse("019f741c-0000-7000-8000-000000000000").expect("valid uuidv7")
}

fn build_basic_artifact() -> GalaxyArtifact {
    let graph = test_graph();
    build_galaxy_artifact(GalaxyArtifactInput {
        graph: &graph,
        project_id: "test-project".to_string(),
        git_head: "abc123".to_string(),
        generated_at: "2026-07-18T00:00:00Z".to_string(),
        generation_id: generation_id(),
        size_cap: ArtifactSizeCap::default(),
    })
    .expect("build artifact")
}

#[test]
fn rejects_non_uuidv7_identity() {
    // A v4 random UUID must be rejected.
    let v4 = uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
    assert!(GenerationId::new(v4).is_err());
    assert!(GenerationId::parse("550e8400-e29b-41d4-a716-446655440000").is_err());
    // A garbage string must be rejected.
    assert!(GenerationId::parse("not-a-uuid").is_err());
    // A valid v7 must be accepted.
    assert!(generation_id().as_str().starts_with("019f741c"));
}

#[test]
fn payload_json_contains_generation_id_and_graph_hash_but_not_transport() {
    let artifact = build_basic_artifact();
    let payload_str = String::from_utf8(artifact.payload_json.clone()).unwrap();

    assert!(
        payload_str.contains("\"generation_id\":\"019f741c-0000-7000-8000-000000000000\""),
        "payload must contain generation_id"
    );
    assert!(
        payload_str.contains(&format!(
            "\"graph_content_hash\":\"{}\"",
            artifact.graph_content_hash
        )),
        "payload must contain graph_content_hash"
    );
    // transport_sha256 must NEVER appear in payload JSON.
    assert!(
        !payload_str.contains("transport_sha256"),
        "transport_sha256 must never enter payload JSON"
    );
}

#[test]
fn hash_input_includes_generation_id_and_omits_graph_content_hash() {
    let artifact = build_basic_artifact();
    let hash_input_str = String::from_utf8(artifact.hash_input_json.clone()).unwrap();

    assert!(
        hash_input_str.contains("\"generation_id\":\"019f741c-0000-7000-8000-000000000000\""),
        "hash input must include generation_id"
    );
    // graph_content_hash must be absent from hash input.
    assert!(
        !hash_input_str.contains("graph_content_hash"),
        "hash input must omit graph_content_hash"
    );
    // transport_sha256 must also be absent.
    assert!(
        !hash_input_str.contains("transport_sha256"),
        "hash input must omit transport_sha256"
    );
}

#[test]
fn graph_content_hash_is_sha256_of_hash_input_bytes() {
    let artifact = build_basic_artifact();
    // Independently recompute the semantic hash from the hash-input bytes.
    let recomputed = hex_sha256(&artifact.hash_input_json);
    assert_eq!(
        recomputed, artifact.graph_content_hash,
        "graph_content_hash must be SHA-256 of hash-input bytes"
    );
}

#[test]
fn changing_generation_identity_changes_graph_content_hash() {
    let graph = test_graph();
    let id_a = GenerationId::parse("019f741c-0000-7000-8000-000000000000").unwrap();
    let id_b = GenerationId::parse("019f741d-0000-7000-8000-000000000000").unwrap();

    let artifact_a = build_galaxy_artifact(GalaxyArtifactInput {
        graph: &graph,
        project_id: "p".to_string(),
        git_head: "c".to_string(),
        generated_at: "t".to_string(),
        generation_id: id_a,
        size_cap: ArtifactSizeCap::default(),
    })
    .unwrap();

    let artifact_b = build_galaxy_artifact(GalaxyArtifactInput {
        graph: &graph,
        project_id: "p".to_string(),
        git_head: "c".to_string(),
        generated_at: "t".to_string(),
        generation_id: id_b,
        size_cap: ArtifactSizeCap::default(),
    })
    .unwrap();

    assert_ne!(
        artifact_a.graph_content_hash, artifact_b.graph_content_hash,
        "changing only the generation identity must change graph_content_hash"
    );
    // The non-identity fields of the payload should be otherwise identical.
    assert_eq!(artifact_a.payload_json.len(), artifact_b.payload_json.len());
}

#[test]
fn same_inputs_produce_identical_artifact() {
    let graph = test_graph();
    let id = generation_id();
    let make = || {
        build_galaxy_artifact(GalaxyArtifactInput {
            graph: &graph,
            project_id: "p".to_string(),
            git_head: "c".to_string(),
            generated_at: "t".to_string(),
            generation_id: id,
            size_cap: ArtifactSizeCap::default(),
        })
        .unwrap()
    };
    let a = make();
    let b = make();
    assert_eq!(a.graph_content_hash, b.graph_content_hash);
    assert_eq!(a.payload_json, b.payload_json);
    assert_eq!(a.spool.transport_sha256, b.spool.transport_sha256);
    assert_eq!(a.spool.chunk_hashes, b.spool.chunk_hashes);
}

#[test]
fn chunks_are_contiguous_and_bounded() {
    let artifact = build_basic_artifact();
    let spool = &artifact.spool;

    assert!(!spool.chunks.is_empty(), "must emit at least one chunk");
    for (i, chunk) in spool.chunks.iter().enumerate() {
        assert_eq!(
            chunk.index, i as u32,
            "chunk indexes must be contiguous starting at 0"
        );
        assert!(
            chunk.bytes.len() <= CHUNK_MAX_BYTES,
            "chunk {} is {} bytes, exceeds {} cap",
            i,
            chunk.bytes.len(),
            CHUNK_MAX_BYTES
        );
    }
}

#[test]
fn total_compressed_bytes_equals_sum_of_chunk_bytes() {
    let artifact = build_basic_artifact();
    let sum: usize = artifact.spool.chunks.iter().map(|c| c.bytes.len()).sum();
    assert_eq!(
        sum as u64, artifact.spool.total_compressed_bytes,
        "total_compressed_bytes must equal the sum of chunk byte lengths"
    );
}

#[test]
fn per_chunk_hashes_recompute_independently() {
    let artifact = build_basic_artifact();
    // Independently recompute each chunk hash from its bytes.
    for (i, chunk) in artifact.spool.chunks.iter().enumerate() {
        let recomputed = hex_sha256(&chunk.bytes);
        assert_eq!(
            recomputed, chunk.sha256,
            "chunk {i} hash must recompute from its bytes"
        );
        assert_eq!(
            recomputed, artifact.spool.chunk_hashes[i],
            "chunk_hashes[{i}] must match the chunk's sha256 field"
        );
    }
}

#[test]
fn transport_hash_recomputes_from_concatenated_chunks() {
    let artifact = build_basic_artifact();
    // Independently recompute the transport hash from the concatenation of
    // all chunk bytes — not from any producer-provided aggregate.
    let mut concat: Vec<u8> = Vec::new();
    for chunk in &artifact.spool.chunks {
        concat.extend_from_slice(&chunk.bytes);
    }
    let recomputed = hex_sha256(&concat);
    assert_eq!(
        recomputed, artifact.spool.transport_sha256,
        "transport_sha256 must be SHA-256 of concatenated chunk bytes"
    );
    assert_ne!(
        artifact.spool.transport_sha256, artifact.graph_content_hash,
        "transport and semantic hash domains must never collide"
    );
}

#[test]
fn golden_decompress_chunks_and_recompute_all_domains() {
    let artifact = build_basic_artifact();
    let hash_input = include_bytes!("fixtures/hash_input.json");
    let payload = include_bytes!("fixtures/payload.json");
    let compressed = include_bytes!("fixtures/payload.json.gz");
    let manifest: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/manifest.json")).expect("parse manifest");
    let graph_content_hash = manifest["graph_content_hash"]
        .as_str()
        .expect("fixture graph hash");
    let chunk_hashes = manifest["chunk_hashes"]
        .as_array()
        .expect("fixture chunk hashes");
    let transport_sha256 = manifest["transport_sha256"]
        .as_str()
        .expect("fixture transport hash");
    let total_compressed_bytes = manifest["total_compressed_bytes"]
        .as_u64()
        .expect("fixture byte total");

    // The producer must retain these pinned canonical bytes, not merely agree
    // with values derived from its own current serialization.
    assert_eq!(artifact.hash_input_json, hash_input);
    assert_eq!(artifact.payload_json, payload);
    assert_eq!(artifact.graph_content_hash, graph_content_hash);
    assert_eq!(
        artifact.spool.total_compressed_bytes,
        total_compressed_bytes
    );
    assert_eq!(artifact.spool.transport_sha256, transport_sha256);
    assert_eq!(artifact.spool.chunks.len(), chunk_hashes.len());
    assert_eq!(
        artifact
            .spool
            .chunks
            .iter()
            .flat_map(|chunk| chunk.bytes.iter().copied())
            .collect::<Vec<_>>(),
        compressed
    );

    // Independently decompress the checked-in gzip bytes.
    let mut decoder = flate2::read::GzDecoder::new(compressed.as_slice());
    let mut decompressed = Vec::new();
    use std::io::Read;
    decoder.read_to_end(&mut decompressed).expect("decompress");
    assert_eq!(decompressed, payload);

    // Validate compatible JSON without round-tripping the producer mirror.
    let wire: serde_json::Value = serde_json::from_slice(&decompressed).expect("parse wire JSON");
    let object = wire.as_object().expect("payload object");
    assert_eq!(object["generation_id"], generation_id().as_str());
    assert_eq!(object["graph_content_hash"], graph_content_hash);
    assert!(!object.contains_key("transport_sha256"));
    let node_ids: std::collections::HashSet<&str> = object["nodes"]
        .as_array()
        .expect("nodes array")
        .iter()
        .map(|node| node["id"].as_str().expect("node id"))
        .collect();
    for edge in object["edges"].as_array().expect("edges array") {
        assert!(node_ids.contains(edge["from"].as_str().expect("edge from")));
        assert!(node_ids.contains(edge["to"].as_str().expect("edge to")));
    }

    // Independently recompute all hash domains from the checked-in bytes.
    assert_eq!(hex_sha256(hash_input), graph_content_hash);
    for (index, chunk) in compressed.chunks(CHUNK_MAX_BYTES).enumerate() {
        assert_eq!(
            hex_sha256(chunk),
            chunk_hashes[index].as_str().expect("string chunk hash")
        );
    }
    assert_eq!(hex_sha256(compressed), transport_sha256);
}

#[test]
fn payload_json_is_valid_compatible_snapshot_shape() {
    let artifact = build_basic_artifact();
    // The payload must deserialize back into the canonical type.
    let parsed: GalaxySnapshotPayload =
        serde_json::from_slice(&artifact.payload_json).expect("parse payload");
    assert_eq!(parsed.project_id, "test-project");
    assert_eq!(parsed.git_head, "abc123");
    assert!(!parsed.nodes.is_empty(), "full graph must have nodes");
    // Nodes must be sorted by pagerank desc then id (stable ordering).
    let mut sorted = parsed.nodes.clone();
    sorted.sort_by(|a, b| {
        b.pagerank
            .partial_cmp(&a.pagerank)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    assert_eq!(
        parsed.nodes, sorted,
        "nodes must be in stable pagerank-desc/id-asc order"
    );
}

#[test]
fn size_cap_failure_returns_typed_oversize_error() {
    let graph = test_graph();
    // A 1-byte cap — any real graph will exceed it.
    let result = build_galaxy_artifact(GalaxyArtifactInput {
        graph: &graph,
        project_id: "p".to_string(),
        git_head: "c".to_string(),
        generated_at: "t".to_string(),
        generation_id: generation_id(),
        size_cap: ArtifactSizeCap::compressed_bytes(1),
    });
    match result {
        Err(GalaxyArtifactError::Oversize { actual, cap }) => {
            assert_eq!(cap, 1);
            assert!(actual > 1, "actual compressed size must exceed cap");
        }
        other => panic!("expected Oversize error, got {other:?}"),
    }
}

#[test]
fn size_cap_allows_when_artifact_fits() {
    let graph = test_graph();
    // Build once to learn the compressed size, then set a cap just above it.
    let probe = build_galaxy_artifact(GalaxyArtifactInput {
        graph: &graph,
        project_id: "p".to_string(),
        git_head: "c".to_string(),
        generated_at: "t".to_string(),
        generation_id: generation_id(),
        size_cap: ArtifactSizeCap::compressed_bytes(u64::MAX),
    })
    .expect("probe build");

    let just_above = probe.spool.total_compressed_bytes;
    let result = build_galaxy_artifact(GalaxyArtifactInput {
        graph: &graph,
        project_id: "p".to_string(),
        git_head: "c".to_string(),
        generated_at: "t".to_string(),
        generation_id: generation_id(),
        size_cap: ArtifactSizeCap::compressed_bytes(just_above),
    });
    assert!(result.is_ok(), "cap equal to actual size must pass");
}

#[test]
fn oversize_error_does_not_expose_publication_inputs() {
    let graph = test_graph();
    let result = build_galaxy_artifact(GalaxyArtifactInput {
        graph: &graph,
        project_id: "p".to_string(),
        git_head: "c".to_string(),
        generated_at: "t".to_string(),
        generation_id: generation_id(),
        size_cap: ArtifactSizeCap::compressed_bytes(1),
    });
    // The Oversize error carries only byte counts, never the artifact/spool.
    assert!(matches!(result, Err(GalaxyArtifactError::Oversize { .. })));
}

#[test]
fn single_chunk_artifact_when_compressed_under_cap() {
    let artifact = build_basic_artifact();
    // The td55 fixture is small enough to compress well under one chunk.
    if artifact.spool.total_compressed_bytes <= CHUNK_MAX_BYTES as u64 {
        assert_eq!(
            artifact.spool.chunks.len(),
            1,
            "artifact under one chunk must produce exactly one chunk"
        );
        assert_eq!(artifact.spool.chunks[0].index, 0);
    }
}

#[test]
fn multi_chunk_artifact_partitions_correctly() {
    // Build a large-enough payload to force multiple chunks. We can't change
    // CHUNK_MAX_BYTES, so synthesize a high-entropy payload directly through
    // spool_gzip to verify the partition logic holds when compressed output
    // exceeds the boundary. High-entropy (SHA-256-derived) data is used so
    // gzip cannot compress it below the chunk boundary.
    let mut payload: Vec<u8> = Vec::with_capacity(CHUNK_MAX_BYTES * 3);
    let mut counter: u64 = 0;
    while payload.len() < CHUNK_MAX_BYTES * 3 {
        let mut hasher = Sha256::new();
        hasher.update(counter.to_le_bytes());
        payload.extend_from_slice(&hasher.finalize());
        counter += 1;
    }
    let spool = spool_gzip(&payload, ArtifactSizeCap::compressed_bytes(u64::MAX)).unwrap();

    assert!(
        spool.chunks.len() >= 2,
        "large payload must produce multiple chunks, got {}",
        spool.chunks.len()
    );
    for chunk in &spool.chunks {
        assert!(chunk.bytes.len() <= CHUNK_MAX_BYTES);
    }
    // Contiguous indexes.
    for (i, chunk) in spool.chunks.iter().enumerate() {
        assert_eq!(chunk.index, i as u32);
    }
    // Decompress and verify round-trip.
    let mut compressed: Vec<u8> = Vec::new();
    for chunk in &spool.chunks {
        compressed.extend_from_slice(&chunk.bytes);
    }
    let mut decoder = flate2::read::GzDecoder::new(compressed.as_slice());
    let mut decompressed = Vec::new();
    use std::io::Read;
    decoder.read_to_end(&mut decompressed).unwrap();
    assert_eq!(decompressed, payload, "multi-chunk round-trip must match");
}
