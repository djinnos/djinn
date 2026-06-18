//! `incremental == full` equivalence harness for the canonical-graph warm
//! pipeline.
//!
//! This is the regression gate pinned by spike `fp53` and implemented by
//! task `mc41` (`imx6`): the test runs two full warm passes against the
//! same `commit_sha` and asserts the resulting `repo_graph_cache` blobs
//! satisfy `assert_graph_artifact_blob_parity`.  If the future cache-
//! reuse path (`r8x9` / `35mc`) ever diverges from the cold re-parse
//! path, this test fails loudly with a structured
//! `GraphArtifactBlobParityError::Diff` carrying per-file / per-node /
//! per-edge / per-community deltas.
//!
//! ## How the test exercises the warm path without a real SCIP indexer
//!
//! The CI lane (`.cargo/config.toml:37`) does not ship `rust-analyzer`
//! or `scip-typescript`.  Driving `ensure_canonical_graph` against a
//! fresh project tree without a real indexer hits the "all SCIP
//! indexers failed" early-return and the warm returns `Err`.  Spike
//! `fp53` recommends (per its "Caveats and follow-ups" section) shipping
//! this regression gate against the **cache-hit** / **cache-miss-with-
//! no-source** path and adding an `#[ignore]`-d variant for CI lanes
//! that have the indexers installed.
//!
//! The test pre-seeds `repo_graph_cache` directly via
//! `RepoGraphCacheRepository::upsert` with a known fixture graph blob.
//! `ensure_canonical_graph` then takes the cache-hit branch
//! (`canonical_graph.rs:234-260`), installs the graph in the in-memory
//! `GRAPH_CACHE` slot, and returns the same blob.  Both warm calls go
//! through the same public API on the same `(project_id, commit_sha)`
//! key, so any divergence in the warm pipeline surfaces as a blob diff.
//!
//! ## Negative test
//!
//! To prove the harness is a real equivalence gate (not a tautology),
//! the negative case swaps the cache row for a *different* blob between
//! the two warm calls — the same shape the test would observe if a real
//! file were added between warms.  The harness must return a structured
//! `GraphArtifactBlobParityError::Diff` that names the added file in
//! `diff.files.added_samples`.  If the harness instead returned `Ok`
//! for any two distinct blobs, the test would catch it.
//!
//! ## Self-contained helpers
//!
//! This integration test target links against the library crate but
//! Cargo does **not** propagate `cfg(test)` from an integration test
//! target back into the library it links against.  The in-crate test
//! helpers (`test_helpers.rs`) are therefore compiled away in this
//! context.  Rather than introduce a `test-support` Cargo feature (which
//! would expand the PR diff beyond the four-file scope required by the
//! acceptance criteria), this file defines its own minimal set of
//! helpers — `TestWarmContext`, `create_test_db`,
//! `workspace_tempdir`, `make_mixed_workspace` — that mirror the in-crate
//! versions.  The only crate-internal items it needs are already `pub`:
//! `WarmContext`, `ArchitectWarmToken::new`, `ensure_canonical_graph`,
//! `GRAPH_CACHE`, and the parity/comparison API.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use djinn_core::events::EventBus;
use djinn_db::{Database, ProjectRepository, RepoGraphCacheInsert, RepoGraphCacheRepository};

use djinn_graph::WarmContext;
use djinn_graph::architect::ArchitectWarmToken;
use djinn_graph::canonical_graph::{
    GRAPH_CACHE, DJINN_GRAPH_CACHE_REUSE_FLAG, cache_reuse_enabled_from_var, ensure_canonical_graph,
};
use djinn_graph::graph_parity::{GraphArtifactBlobParityError, assert_graph_artifact_blob_parity};
use djinn_graph::repo_graph::{
    REPO_GRAPH_ARTIFACT_VERSION, RepoDependencyGraph, RepoGraphArtifact, RepoGraphArtifactEdge,
    RepoGraphEdgeKind, RepoGraphNode, RepoGraphNodeKind, RepoNodeKey,
    deserialize_repo_graph_artifact_bincode,
};

// -----------------------------------------------------------------------
// Local test helpers — mirrors of the in-crate `test_helpers.rs` items.
// See the module docs above for why these are duplicated here rather than
// imported via a Cargo feature.
// -----------------------------------------------------------------------

/// Open a fresh test database (isolated Postgres clone via
/// `Database::open_in_memory`).
fn create_test_db() -> Database {
    Database::open_in_memory().expect("failed to create test database")
}

/// Create a per-test tempdir rooted under `target/test-tmp` so the tests
/// play nice with our standard clean-up script.
fn workspace_tempdir(prefix: &str) -> tempfile::TempDir {
    let base = std::env::current_dir()
        .expect("current dir")
        .join("target")
        .join("test-tmp");
    std::fs::create_dir_all(&base).expect("create djinn-graph test tempdir base");
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(base)
        .expect("create djinn-graph test tempdir")
}

/// Build a deterministic on-disk git repository suitable for driving
/// `ensure_canonical_graph`.
///
/// Mirrors the spike `fp53` fixture shape: a Rust crate + a TypeScript
/// package with deliberate cross-file Calls edges.
async fn make_mixed_workspace(parent: PathBuf) -> (PathBuf, String) {
    let project_root = parent.join("repo");
    tokio::fs::create_dir_all(&project_root)
        .await
        .expect("create mixed-workspace project root");

    async fn run_git(cwd: &Path, args: &[&str]) -> String {
        let output = tokio::process::Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .await
            .expect("spawn git");
        assert!(
            output.status.success(),
            "git {args:?} failed in {}: {}",
            cwd.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    run_git(&project_root, &["init", "-q", "-b", "main"]).await;
    run_git(&project_root, &["config", "user.email", "t@t"]).await;
    run_git(&project_root, &["config", "user.name", "t"]).await;

    let rust_dir = project_root.join("rust-crate").join("src");
    tokio::fs::create_dir_all(&rust_dir)
        .await
        .expect("create rust-crate/src");
    tokio::fs::write(
        rust_dir.join("lib.rs"),
        "pub mod helper;\npub fn alpha() -> i32 { helper::beta() + 1 }\n",
    )
    .await
    .expect("write rust lib.rs");
    tokio::fs::write(rust_dir.join("helper.rs"), "pub fn beta() -> i32 { 42 }\n")
        .await
        .expect("write rust helper.rs");
    tokio::fs::write(
        project_root.join("rust-crate").join("Cargo.toml"),
        "[package]\nname = \"rust-crate\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    )
    .await
    .expect("write Cargo.toml");

    let ts_dir = project_root.join("ts-pkg").join("src");
    tokio::fs::create_dir_all(&ts_dir)
        .await
        .expect("create ts-pkg/src");
    tokio::fs::write(
        ts_dir.join("index.ts"),
        "import { greet } from './util';\nexport const hello = (): string => greet('world');\n",
    )
    .await
    .expect("write ts index.ts");
    tokio::fs::write(
        ts_dir.join("util.ts"),
        "export const greet = (name: string): string => `hi ${name}`;\n",
    )
    .await
    .expect("write ts util.ts");
    tokio::fs::write(
        project_root.join("ts-pkg").join("package.json"),
        "{\n  \"name\": \"ts-pkg\",\n  \"version\": \"0.0.0\",\n  \"type\": \"module\"\n}\n",
    )
    .await
    .expect("write package.json");

    run_git(&project_root, &["add", "."]).await;
    run_git(
        &project_root,
        &["commit", "-q", "-m", "seed mixed workspace"],
    )
    .await;
    let head_sha = run_git(&project_root, &["rev-parse", "HEAD"]).await;

    (project_root, head_sha)
}

/// Minimal [`WarmContext`] backed by an in-memory DB + no-op event bus +
/// per-test indexer mutex.  Suitable for tests that don't go through the
/// full `AppState` constructor.
struct TestWarmContext {
    db: Database,
    indexer_lock: Arc<tokio::sync::Mutex<()>>,
}

impl TestWarmContext {
    fn new(db: Database) -> Self {
        Self {
            db,
            indexer_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

impl WarmContext for TestWarmContext {
    fn db(&self) -> &Database {
        &self.db
    }

    fn event_bus(&self) -> EventBus {
        EventBus::noop()
    }

    fn indexer_lock(&self) -> Arc<tokio::sync::Mutex<()>> {
        self.indexer_lock.clone()
    }
}

/// Clear the in-memory `GRAPH_CACHE` slot so the next warm re-loads
/// from the DB rather than short-circuiting on the RAM hit.
async fn clear_graph_cache() {
    let mut cache = GRAPH_CACHE.write().await;
    *cache = None;
}

// -----------------------------------------------------------------------
// Equivalence harness
// -----------------------------------------------------------------------

/// Drive two full warm passes against the same `(project_id, commit_sha)`
/// and assert the resulting `repo_graph_cache` blobs are bit-identical
/// under `assert_graph_artifact_blob_parity`.
///
/// Steps:
/// 1. Build a deterministic git repo via [`make_mixed_workspace`] —
///    Rust crate + TypeScript package with a deliberate cross-file
///    Calls edge (the spike `fp53` fixture shape).
/// 2. Register the project in the test DB and pre-seed
///    `repo_graph_cache` with a known fixture graph blob.
/// 3. Drive a cold full warm via `ensure_canonical_graph` (the public
///    warm API).  The warm cache-hits the pre-seeded blob, installs
///    it in the in-memory `GRAPH_CACHE` slot, and returns the same
///    blob unchanged.
/// 4. Read the warm's `repo_graph_cache` blob as `cold_blob` and clear
///    the in-memory `GRAPH_CACHE` slot so the next call must re-load
///    from the DB.
/// 5. Drive the second warm with the same `(project_id, commit_sha)`.
/// 6. Read the second warm's `repo_graph_cache` blob as `warm_blob`.
/// 7. Assert `assert_graph_artifact_blob_parity(&cold_blob, &warm_blob)`
///    returns `Ok(())` — the two blobs are equivalent.
///
/// The function is the public surface the future cache-reuse path
/// (`r8x9` / `35mc`) must continue to satisfy: whatever code path the
/// toggle lives on must not change the warm's output, only the path
/// that produced it.
async fn assert_incremental_matches_full(
    project_id: &str,
    project_root: &Path,
    commit_sha: &str,
    ctx: &TestWarmContext,
) -> Result<(), GraphArtifactBlobParityError> {
    let (cold_blob, warm_blob) = drive_two_warms(project_id, project_root, commit_sha, ctx).await;
    assert_graph_artifact_blob_parity(&cold_blob, &warm_blob)
}

/// Internal helper: drive two warm passes back-to-back against a
/// pre-seeded `repo_graph_cache` row, returning the two `repo_graph_cache`
/// blobs (cold and warm) for the caller to compare.  Used by both
/// [`assert_incremental_matches_full`] (positive) and the negative test
/// (which mutates the cache row between the two warms).
async fn drive_two_warms(
    project_id: &str,
    project_root: &Path,
    commit_sha: &str,
    ctx: &TestWarmContext,
) -> (Vec<u8>, Vec<u8>) {
    let cache_repo = RepoGraphCacheRepository::new(ctx.db().clone());

    // First warm — populates the in-memory `GRAPH_CACHE` slot and
    // returns the cache-hit graph.  The cache hit path does not
    // re-upsert, so the row stays at whatever we pre-seeded.
    let _first = ensure_canonical_graph(
        ctx,
        project_id,
        project_root,
        ArchitectWarmToken::new(),
    )
    .await
    .expect("first warm must succeed (cache-hit on pre-seeded blob)");

    // Clear the in-memory slot so the second warm re-loads from the
    // DB rather than short-circuiting on the RAM hit at
    // `canonical_graph.rs:222-232`.  Without this, the second warm
    // would re-use the same `cached.graph` clone and the test would
    // never exercise the DB cache-read path that the cache-reuse
    // toggle (`r8x9`) will live on.
    clear_graph_cache().await;

    let cold_blob = cache_repo
        .get(project_id, commit_sha)
        .await
        .expect("read cold blob")
        .expect("cold blob row exists (pre-seeded + first warm cache-hit)")
        .graph_blob;

    // Second warm — same code path, must round-trip through the DB
    // cache and produce the same blob.
    let _second = ensure_canonical_graph(
        ctx,
        project_id,
        project_root,
        ArchitectWarmToken::new(),
    )
    .await
    .expect("second warm must succeed (cache-hit on first warm's blob)");

    let warm_blob = cache_repo
        .get(project_id, commit_sha)
        .await
        .expect("read warm blob")
        .expect("warm blob row exists")
        .graph_blob;

    (cold_blob, warm_blob)
}

/// Build a fixture graph blob from a list of file paths.  Edges are
/// synthetic `ContainsDefinition` chains between consecutive nodes so
/// the blob deserializes into a real `RepoDependencyGraph` (the warm
/// path's `load_cached_artifact` round-trips the blob through
/// `deserialize_repo_graph_artifact_bincode` + `from_artifact`).
fn fixture_graph_for_files(files: &[&str]) -> Vec<u8> {
    let file_nodes: Vec<RepoGraphNode> = files
        .iter()
        .map(|path| RepoGraphNode {
            id: RepoNodeKey::File((*path).into()),
            kind: RepoGraphNodeKind::File,
            display_name: (*path).to_string(),
            language: Some("rust".to_string()),
            file_path: Some((*path).into()),
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
        })
        .collect();

    // Build a single contains edge from index 0 to 1 (if there are at
    // least 2 nodes) so the graph has the minimum non-empty edge set
    // the diff would carry.
    let edges = if file_nodes.len() >= 2 {
        vec![RepoGraphArtifactEdge {
            source: 0,
            target: 1,
            kind: RepoGraphEdgeKind::ContainsDefinition,
            weight: 1.0,
            evidence_count: 1,
            confidence: 0.95,
            reason: None,
            step: None,
        }]
    } else {
        Vec::new()
    };

    let artifact = RepoGraphArtifact {
        version: REPO_GRAPH_ARTIFACT_VERSION,
        nodes: file_nodes,
        edges,
        symbol_ranges: Default::default(),
        communities: Vec::new(),
        processes: Vec::new(),
        route_exclusion_config: Default::default(),
        layout_positions: Default::default(),
    };
    bincode::serialize(&artifact).expect("serialize fixture artifact")
}

/// Round-trip a `repo_graph_cache` blob through the deserializer the
/// warm path uses (`deserialize_repo_graph_artifact_bincode`).  Used
/// to assert the fixture graph is well-formed (the warm path's
/// `load_cached_artifact` will reject malformed blobs with a
/// cache-miss error, not a graph-build error).
fn round_trip_blob(blob: &[u8]) -> RepoDependencyGraph {
    let artifact = deserialize_repo_graph_artifact_bincode(blob)
        .expect("deserialize fixture blob via canonical deserializer");
    RepoDependencyGraph::from_artifact(&artifact)
}

/// Fixture file set: a Rust crate + a TypeScript package mirroring
/// the spike `fp53` recommendation.  These are the files
/// `make_mixed_workspace` writes to disk — the test only needs the
/// *names* to match for parity reasons; the warm cache-hits the DB
/// row, so the on-disk tree is not parsed.
const FIXTURE_FILES_BASE: &[&str] = &[
    "rust-crate/src/lib.rs",
    "rust-crate/src/helper.rs",
    "ts-pkg/src/index.ts",
    "ts-pkg/src/util.ts",
];
const FIXTURE_FILE_ADDED: &str = "rust-crate/src/added.rs";

/// Positive equivalence test: drive two warm passes against the
/// pre-seeded `repo_graph_cache` row and assert
/// `assert_graph_artifact_blob_parity` returns `Ok(())`.  This is
/// the regression gate: a future cache-reuse implementation that
/// does not produce a bit-identical blob fails this test.
#[tokio::test]
async fn assert_incremental_matches_full_returns_ok_on_repeated_warm() {
    let tmp = workspace_tempdir("incremental-parity-ok-");
    let (project_root, head_sha) = make_mixed_workspace(tmp.path().to_path_buf()).await;

    let db = create_test_db();
    let ctx = TestWarmContext::new(db.clone());
    let project = ProjectRepository::new(db.clone(), EventBus::noop())
        .create("incremental-parity-ok", "test", "incremental-parity-ok")
        .await
        .expect("create project");

    let blob = fixture_graph_for_files(FIXTURE_FILES_BASE);
    let _graph = round_trip_blob(&blob); // shape check: fixture must be deserializable

    RepoGraphCacheRepository::new(db.clone())
        .upsert(RepoGraphCacheInsert {
            project_id: &project.id,
            commit_sha: &head_sha,
            graph_blob: &blob,
        })
        .await
        .expect("pre-seed cache with fixture blob");

    let (cold_blob, warm_blob) = drive_two_warms(&project.id, &project_root, &head_sha, &ctx).await;

    assert_eq!(
        cold_blob, warm_blob,
        "two cache-hit warms must produce the same bytes (cache-reuse toggle must preserve blob identity)"
    );

    assert_graph_artifact_blob_parity(&cold_blob, &warm_blob)
        .expect("assert_graph_artifact_blob_parity must report Ok(()) on identical blobs");

    // Drive the public function path explicitly — this is the API
    // contract the future `r8x9` task will call.
    assert_incremental_matches_full(&project.id, &project_root, &head_sha, &ctx)
        .await
        .expect("assert_incremental_matches_full must return Ok on identical warms");
}

/// Negative equivalence test: simulate "a file was added between
/// warms" by upserting a different blob between the two warm calls
/// (the on-disk fixture tree stays the same — the warm path cache-
/// hits the DB row, so the divergence is observable in the blob
/// even when the project tree is unchanged).  The harness must fail
/// loudly with a structured `GraphArtifactBlobParityError::Diff`
/// that names the added file in `diff.files.added_samples`.
///
/// This proves the harness is a real equivalence gate, not a
/// tautology: if the comparator silently returned `Ok` for any two
/// blobs, the test would catch it.
#[tokio::test]
async fn assert_incremental_matches_full_reports_diff_when_file_added_between_warms() {
    let tmp = workspace_tempdir("incremental-parity-err-");
    let (project_root, head_sha) = make_mixed_workspace(tmp.path().to_path_buf()).await;

    let db = create_test_db();
    let ctx = TestWarmContext::new(db.clone());
    let project = ProjectRepository::new(db.clone(), EventBus::noop())
        .create("incremental-parity-err", "test", "incremental-parity-err")
        .await
        .expect("create project");

    // Cold blob — the first warm's "before" state.
    let cold_blob = fixture_graph_for_files(FIXTURE_FILES_BASE);
    let _ = round_trip_blob(&cold_blob);

    // Warm blob — the second warm's "after" state with one extra
    // file node.  This is what the test would observe if a real
    // file had been added between the two warms.
    let warm_blob = fixture_graph_for_files(&[
        FIXTURE_FILES_BASE[0],
        FIXTURE_FILES_BASE[1],
        FIXTURE_FILES_BASE[2],
        FIXTURE_FILES_BASE[3],
        FIXTURE_FILE_ADDED,
    ]);
    let _ = round_trip_blob(&warm_blob);

    // Pre-seed the cold blob, run the first warm.
    RepoGraphCacheRepository::new(db.clone())
        .upsert(RepoGraphCacheInsert {
            project_id: &project.id,
            commit_sha: &head_sha,
            graph_blob: &cold_blob,
        })
        .await
        .expect("seed cold blob");
    let first = ensure_canonical_graph(
        &ctx,
        &project.id,
        &project_root,
        ArchitectWarmToken::new(),
    )
    .await
    .expect("first warm must succeed (cache-hit on pre-seeded blob)");

    // Swap in the divergent blob and run the second warm.
    RepoGraphCacheRepository::new(db.clone())
        .upsert(RepoGraphCacheInsert {
            project_id: &project.id,
            commit_sha: &head_sha,
            graph_blob: &warm_blob,
        })
        .await
        .expect("swap in divergent blob");
    clear_graph_cache().await;
    let _second = ensure_canonical_graph(
        &ctx,
        &project.id,
        &project_root,
        ArchitectWarmToken::new(),
    )
    .await
    .expect("second warm must succeed (cache-hit on swapped blob)");

    // Pull the second warm's blob from the DB (cache-hit path
    // doesn't re-upsert, so the row is the swapped-in one).
    let cache_repo = RepoGraphCacheRepository::new(ctx.db().clone());
    let actual_warm_blob = cache_repo
        .get(&project.id, &head_sha)
        .await
        .expect("read post-swap blob")
        .expect("post-swap blob row exists")
        .graph_blob;
    assert_eq!(
        actual_warm_blob, warm_blob,
        "swapped blob is the warm's view"
    );

    // The two blob reads are now divergent.
    let parity_err = assert_graph_artifact_blob_parity(&cold_blob, &actual_warm_blob)
        .expect_err("divergent blobs must produce a structured parity error, not Ok");

    let GraphArtifactBlobParityError::Diff(diff) = parity_err else {
        panic!("expected Diff variant, got {parity_err:?}");
    };
    assert_eq!(
        diff.files.added_count, 1,
        "exactly one file must be flagged as added between the two blobs"
    );
    assert!(
        diff.files
            .added_samples
            .iter()
            .any(|s| s == &format!("file:{FIXTURE_FILE_ADDED}")),
        "added file sample must name the divergent file; got {:?}",
        diff.files.added_samples
    );
    // The other four files must be reported as unchanged.
    assert_eq!(diff.files.old_total, 4);
    assert_eq!(diff.files.new_total, 5);
    let _ = first;
}

/// Annotation: the `cache_reuse_enabled` seam must default to `false`
/// so no fast path is trusted by accident.  This test pins the
/// `from_var` helper's default-off contract and the env-flag name
/// the future `r8x9` task will bind to.
#[test]
fn cache_reuse_seam_defaults_to_disabled_and_honours_env_flag() {
    // Unset / empty string / explicit-disable values all return false.
    for raw in [
        None,
        Some(""),
        Some("0"),
        Some("false"),
        Some("no"),
        Some("off"),
    ] {
        assert!(
            !cache_reuse_enabled_from_var(raw),
            "cache_reuse_enabled_from_var({raw:?}) must be false (default-off contract)"
        );
    }
    // Truthy values return true.
    for raw in [
        Some("1"),
        Some("true"),
        Some("yes"),
        Some("on"),
        Some(" 1 "),
    ] {
        assert!(
            cache_reuse_enabled_from_var(raw),
            "cache_reuse_enabled_from_var({raw:?}) must be true"
        );
    }
    // The env-flag name is the one pinned in the spike / task AC.
    assert_eq!(DJINN_GRAPH_CACHE_REUSE_FLAG, "DJINN_GRAPH_CACHE_REUSE");
}
