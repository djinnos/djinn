//! Live-Postgres full-warm publication regression coverage.
//!
//! This deliberately rebuilds all validation inputs from rows read back from
//! Postgres. It never uses producer-calculated hashes as the assertion oracle.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use djinn_db::repositories::repo_graph_generation::ReservedPublicationFailureStage;
use djinn_db::{
    CurrentGalaxyArtifact, Database, RepoGraphCacheRepository, RepoGraphGenerationRepository,
    ReservedGalaxyArtifactChunk, ReservedGalaxyArtifactManifest, ReservedGraphPublication,
};
use protobuf::{EnumOrUnknown, Message};
use scip::types::{Document, Index, Occurrence, SymbolInformation, symbol_information};
use sha2::{Digest, Sha256};

use super::{
    ArtifactSizeCap, GalaxyArtifact, GalaxyArtifactError, GalaxyArtifactInput, GenerationId,
    build_galaxy_artifact,
};

const PROJECT: &str = "full-warm-publication-regression";

fn database_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn sha256(bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(bytes);
    format!("{:x}", digest.finalize())
}

async fn fresh() -> Option<(Database, RepoGraphGenerationRepository)> {
    std::env::var("DJINN_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .or_else(|_| std::env::var("TEST_POSTGRES_URL"))
        .ok()?;
    let db = Database::open_in_memory().expect("open isolated Postgres test database");
    db.ensure_initialized()
        .await
        .expect("initialize isolated Postgres test database");
    let repo = RepoGraphGenerationRepository::new(db.clone());
    repo.prepare_publication_test_project(PROJECT)
        .await
        .expect("seed project");
    Some((db, repo))
}

struct EnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let previous = std::env::var_os(key);
        // `run_real_full_warm` holds the shared pipeline lock for this whole
        // process-environment mutation and its spawned indexer subprocess.
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        unsafe {
            match self.previous.take() {
                Some(previous) => std::env::set_var(self.key, previous),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

fn write_fake_rust_analyzer(tmp: &Path) -> (PathBuf, PathBuf) {
    let fixture_path = tmp.join("fixture.scip");
    let mut document = Document::new();
    document.relative_path = "src/lib.rs".to_string();
    document.language = "rust".to_string();
    document.occurrences = vec![Occurrence {
        range: vec![0, 7, 13],
        symbol: "scip-rust full-warm src/lib.rs `answer`().".to_string(),
        symbol_roles: scip::types::SymbolRole::Definition as i32,
        ..Occurrence::new()
    }];
    document.symbols = vec![SymbolInformation {
        symbol: "scip-rust full-warm src/lib.rs `answer`().".to_string(),
        display_name: "answer".to_string(),
        kind: EnumOrUnknown::new(symbol_information::Kind::Function),
        ..SymbolInformation::new()
    }];
    let mut index = Index::new();
    index.documents = vec![document];
    std::fs::write(
        &fixture_path,
        index.write_to_bytes().expect("encode SCIP fixture"),
    )
    .expect("write SCIP fixture");

    let fake_bin = tmp.join("fake-bin");
    std::fs::create_dir_all(&fake_bin).expect("create fake indexer bin dir");
    let script_path = fake_bin.join("rust-analyzer");
    std::fs::write(
        &script_path,
        r#"#!/bin/sh
set -eu
out=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output" ]; then shift; out="$1"; fi
  shift || true
done
mkdir -p "$(dirname "$out")"
cp "$DJINN_TEST_SCIP_FIXTURE" "$out"
"#,
    )
    .expect("write fake rust-analyzer");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&script_path)
            .expect("fake rust-analyzer metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script_path, permissions).expect("chmod fake rust-analyzer");
    }
    (fake_bin, fixture_path)
}

/// Exercise the source-bearing warm path end-to-end. This includes the real
/// reservation, producer inputs, graph-blob selection, manifest conversion,
/// and warmer publication call; assertions below only inspect stored rows.
///
/// Returns the commit SHA and the `RepoDependencyGraph` returned by
/// `ensure_canonical_graph`. Callers serialize the graph independently — they
/// never read `graph_blob` back from the compatibility cache to build the
/// assertion oracle, so a production code path that stores an unrelated blob
/// is caught.
#[allow(clippy::await_holding_lock)]
async fn run_real_full_warm(
    db: &Database,
) -> (String, Arc<crate::repo_graph::RepoDependencyGraph>) {
    let _env_lock = crate::test_helpers::lock_pipeline_env();
    crate::canonical_graph::clear_test_caches().await;
    let temp = crate::test_helpers::workspace_tempdir("full-warm-publication-");
    let project_root = temp.path().join("repo");
    std::fs::create_dir_all(project_root.join("src")).expect("create source fixture");
    std::fs::write(
        project_root.join("Cargo.toml"),
        "[package]\nname = \"full_warm_fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[workspace]\n",
    )
    .expect("write fixture manifest");
    std::fs::write(
        project_root.join("src/lib.rs"),
        "pub fn answer() -> u32 { 42 }\n",
    )
    .expect("write fixture source");
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec!["config", "user.email", "full-warm@test"],
        vec!["config", "user.name", "full warm"],
        vec!["add", "Cargo.toml", "src/lib.rs"],
        vec!["commit", "-q", "-m", "full warm fixture"],
    ] {
        let output = djinn_git::run_git_command_in(
            &project_root,
            args.into_iter().map(str::to_owned).collect(),
        )
        .await
        .expect("run fixture git command");
        assert_eq!(output.code, 0, "fixture git command failed: {output:?}");
    }
    let (fake_bin, fixture_path) = write_fake_rust_analyzer(temp.path());
    let path = std::env::var_os("PATH").unwrap_or_default();
    let joined_path =
        std::env::join_paths(std::iter::once(fake_bin).chain(std::env::split_paths(&path)))
            .expect("join PATH with fake rust-analyzer");
    let _path = EnvVarGuard::set("PATH", joined_path);
    let _fixture = EnvVarGuard::set("DJINN_TEST_SCIP_FIXTURE", fixture_path);

    let context = crate::test_helpers::TestWarmContext::new(db.clone());
    let result = crate::canonical_graph::ensure_canonical_graph(
        &context,
        PROJECT,
        &project_root,
        crate::architect::ArchitectWarmToken::for_tests(),
    )
    .await;
    assert!(result.is_ok(), "real full warm failed: {result:?}");
    let (_handle, graph) = result.expect("checked ok");
    let commit_sha = djinn_git::head_commit_sha(&project_root)
        .await
        .expect("resolve full-warm fixture HEAD");
    crate::canonical_graph::clear_test_caches().await;
    (commit_sha, graph)
}

// Failure-stage injection is a repository seam by design; the successful
// publication assertions exercise the real warmer through `run_real_full_warm`.
fn failure_publication(artifact: &GalaxyArtifact) -> ReservedGraphPublication {
    let generation_id = artifact.generation_id.as_str();
    ReservedGraphPublication {
        project_id: PROJECT.to_owned(),
        commit_sha: "injected-failure".to_owned(),
        generation_id: generation_id.clone(),
        graph_blob: crate::test_helpers::td55_equivalence_fixture_artifact_blob(),
        artifact: ReservedGalaxyArtifactManifest {
            artifact_id: generation_id.clone(),
            generation_id: generation_id.clone(),
            graph_content_hash: artifact.graph_content_hash.clone(),
            transport_sha256: artifact.spool.transport_sha256.clone(),
            chunk_count: i32::try_from(artifact.spool.chunks.len()).expect("chunk count"),
            byte_count: i64::try_from(artifact.spool.total_compressed_bytes).expect("byte count"),
            chunk_hashes: artifact.spool.chunk_hashes.clone(),
        },
        chunks: artifact
            .spool
            .chunks
            .iter()
            .map(|chunk| ReservedGalaxyArtifactChunk {
                generation_id: generation_id.clone(),
                artifact_id: generation_id.clone(),
                chunk_index: i32::try_from(chunk.index).expect("chunk index"),
                sha256: chunk.sha256.clone(),
                bytes: chunk.bytes.clone(),
            })
            .collect(),
    }
}

fn build_failure_artifact() -> GalaxyArtifact {
    let graph = crate::test_helpers::td55_equivalence_fixture_graph();
    build_galaxy_artifact(GalaxyArtifactInput {
        graph: &graph,
        project_id: PROJECT.to_owned(),
        git_head: "injected-failure".to_owned(),
        generated_at: "2026-07-18T00:00:00Z".to_owned(),
        generation_id: GenerationId::new(uuid::Uuid::now_v7()).expect("UUIDv7 generation"),
        size_cap: ArtifactSizeCap::default(),
    })
    .expect("failure-stage artifact")
}

#[tokio::test]
async fn full_warm_publishes_one_identity_and_stored_artifact_recomputes_independently() {
    let _serial = database_lock().lock().await;
    let Some((db, repo)) = fresh().await else {
        return;
    };
    let (commit_sha, _) = run_real_full_warm(&db).await;

    let expected_id = repo
        .compatibility_generation_id(PROJECT, &commit_sha)
        .await
        .unwrap();
    let generation = repo
        .generation_by_id(&expected_id)
        .await
        .unwrap()
        .expect("immutable generation");
    let current = repo
        .current_generation_for_project(PROJECT)
        .await
        .unwrap()
        .expect("current generation");
    let metadata = match repo
        .current_galaxy_artifact_for_project(PROJECT)
        .await
        .unwrap()
    {
        CurrentGalaxyArtifact::Available { artifact, .. } => artifact,
        other => panic!("expected stored artifact, got {other:?}"),
    };
    let chunks = repo.galaxy_chunks_for_test(&expected_id).await.unwrap();

    assert_eq!(generation.generation_id, expected_id);
    assert!(generation.artifact_required);
    assert_eq!(current.generation_id, expected_id);
    assert_eq!(metadata.artifact_id, expected_id);
    assert_eq!(metadata.generation_id, expected_id);
    assert_eq!(chunks.len(), metadata.chunk_count as usize);

    let mut compressed = Vec::new();
    let mut manifest_hashes = Vec::new();
    for (index, chunk) in chunks.iter().enumerate() {
        assert_eq!(chunk.generation_id, expected_id);
        assert_eq!(chunk.artifact_id, expected_id);
        assert_eq!(chunk.chunk_index, index as i32);
        let chunk_hash = sha256(&chunk.bytes);
        assert_eq!(chunk_hash, chunk.sha256);
        compressed.extend_from_slice(&chunk.bytes);
        manifest_hashes.push(chunk_hash);
    }
    assert_eq!(
        serde_json::from_str::<Vec<String>>(&metadata.chunk_hashes).unwrap(),
        manifest_hashes
    );
    assert_eq!(sha256(&compressed), metadata.transport_sha256);
    assert_eq!(compressed.len() as i64, metadata.byte_count);

    let mut payload = Vec::new();
    flate2::read::GzDecoder::new(compressed.as_slice())
        .read_to_end(&mut payload)
        .unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    let object = value.as_object_mut().unwrap();
    assert_eq!(object["generation_id"], expected_id);
    assert_eq!(object["graph_content_hash"], metadata.graph_content_hash);
    assert!(!object.contains_key("transport_sha256"));
    // Rebuild canonical semantic JSON from the decompressed stored payload,
    // never from producer-retained hash-input bytes. The canonical wire order
    // is part of the hash domain, so remove exactly the one hash member from
    // validated stored JSON rather than reordering its objects via a map.
    let stored_hash = metadata.graph_content_hash.clone();
    let hash_member = format!("\"graph_content_hash\":\"{stored_hash}\",");
    let canonical_semantic = std::str::from_utf8(&payload)
        .expect("stored payload UTF-8")
        .replacen(&hash_member, "", 1);
    assert_ne!(canonical_semantic, std::str::from_utf8(&payload).unwrap());
    assert_eq!(sha256(canonical_semantic.as_bytes()), stored_hash);
}

#[tokio::test]
async fn full_warm_failures_preserve_previous_pointer_and_every_table() {
    let _serial = database_lock().lock().await;
    let Some((_, repo)) = fresh().await else {
        return;
    };
    repo.legacy_upsert_for_publication_test(PROJECT, "old", b"old graph")
        .await
        .unwrap();
    let before = repo.publication_snapshot_for_test(PROJECT).await.unwrap();
    let graph = crate::test_helpers::td55_equivalence_fixture_graph();
    let cap = build_galaxy_artifact(GalaxyArtifactInput {
        graph: &graph,
        project_id: PROJECT.to_owned(),
        git_head: "too-big".to_owned(),
        generated_at: "2026-07-18T00:00:00Z".to_owned(),
        generation_id: GenerationId::new(uuid::Uuid::now_v7()).unwrap(),
        size_cap: ArtifactSizeCap::compressed_bytes(1),
    });
    assert!(matches!(cap, Err(GalaxyArtifactError::Oversize { .. })));
    assert_eq!(
        repo.publication_snapshot_for_test(PROJECT).await.unwrap(),
        before,
        "size cap must not open publication"
    );
    for stage in [
        ReservedPublicationFailureStage::CompatibilityUpsert,
        ReservedPublicationFailureStage::ArtifactInsert,
        ReservedPublicationFailureStage::FirstChunkInsert,
        ReservedPublicationFailureStage::Commit,
    ] {
        let artifact = build_failure_artifact();
        assert!(
            repo.publish_reserved_generation_with_failure(failure_publication(&artifact), stage)
                .await
                .is_err()
        );
        assert_eq!(
            repo.publication_snapshot_for_test(PROJECT).await.unwrap(),
            before,
            "{stage:?} leaked cache/generation/metadata/chunks/current/clock"
        );
    }
}

#[tokio::test]
async fn unchanged_old_reader_sees_new_warm_and_unmarked_legacy_advances_artifactless_current() {
    let _serial = database_lock().lock().await;
    let Some((db, repo)) = fresh().await else {
        return;
    };
    let (commit_sha, graph) = run_real_full_warm(&db).await;
    let serialized_blob = bincode::serialize(&graph.to_artifact()).expect("serialize graph");

    // These helpers execute the unchanged old SELECT and select the immutable
    // generation through the current pointer, respectively.
    let old = repo
        .legacy_latest_for_publication_test(PROJECT)
        .await
        .unwrap();
    let current_generation = repo
        .current_generation_for_project(PROJECT)
        .await
        .unwrap()
        .expect("current generation");

    assert_eq!(old.project_id, current_generation.project_id);
    assert_eq!(old.commit_sha, current_generation.commit_sha);
    assert_eq!(old.generation_id, current_generation.generation_id);
    assert_eq!(old.graph_blob, current_generation.graph_blob);
    assert_eq!(old.project_id, PROJECT);
    assert_eq!(old.commit_sha, commit_sha);
    assert_eq!(old.graph_blob, serialized_blob);
    assert_eq!(current_generation.graph_blob, serialized_blob);

    repo.legacy_upsert_for_publication_test(PROJECT, "legacy-after-warm", b"legacy graph")
        .await
        .unwrap();
    match repo
        .current_galaxy_artifact_for_project(PROJECT)
        .await
        .unwrap()
    {
        CurrentGalaxyArtifact::ArtifactUnavailable { generation } => {
            assert_eq!(generation.commit_sha, "legacy-after-warm")
        }
        other => panic!(
            "legacy upsert must advance to an artifactless current generation, got {other:?}"
        ),
    }
    let cache = RepoGraphCacheRepository::new(db);
    assert_eq!(
        cache
            .latest_for_project(PROJECT)
            .await
            .unwrap()
            .unwrap()
            .commit_sha,
        "legacy-after-warm"
    );
}
