//! Deterministic regression harness proving the versioned SCIP cache and
//! partition plumbing make unchanged and one-partition-changed warm paths
//! substantially cheaper than cold while preserving whole-graph semantics.
//!
//! # Approach
//!
//! Models N fake partitions (simulating Go packages), each with deterministic
//! source content that feeds the versioned content-addressed SCIP cache. The
//! harness runs three scenarios against the real [`ScipCacheStore`] and
//! [`collect_scip_artifacts`] pipeline:
//!
//! | Scenario             | Cache state | Invocations | Artifacts |
//! |---------------------|-------------|-------------|-----------|
//! | Cold                | Empty       | N           | N         |
//! | Warm unchanged      | Populated   | 0           | N         |
//! | Warm one-changed    | Populated   | 1           | N         |
//!
//! The measurable cost signal is **invocation count**: warm-unchanged must be
//! 0, one-changed must be 1, and cold must be N. The final artifact set must
//! always contain ALL N partitions (whole-graph semantics — no
//! changed-file-only resolution path).
//!
//! No real SCIP binaries, Docker, Kubernetes, network access, or privileged
//! external infrastructure is required.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use super::cache::{self, CacheKeyIngredients, CacheLookup, ScipCacheKey, ScipCacheStore};
use super::indexing::collect_scip_artifacts;
use super::{ExecutedIndexerCommand, PlannedIndexerCommand, SupportedIndexer};

/// Number of fake partitions in the harness. Enough to demonstrate the
/// cost-reduction pattern without being slow.
const PARTITION_COUNT: usize = 4;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A fake partition that simulates a below-workspace indexing unit
/// (e.g. a Go package or Clang translation unit).
struct FakePartition {
    name: String,
    source_content: Vec<u8>,
}

fn fake_artifact_payload(partition: &FakePartition) -> String {
    format!(
        "fake-scip partition={} source_hash={}",
        partition.name,
        cache::content_hash(&partition.source_content)
    )
}

fn expected_payloads(partitions: &[FakePartition]) -> BTreeMap<String, String> {
    partitions
        .iter()
        .map(|partition| (partition.name.clone(), fake_artifact_payload(partition)))
        .collect()
}

/// Fake final graph-builder sink. Production parses SCIP and builds the
/// canonical graph after collection; this deterministic sink records the full
/// current artifact set it would consume without requiring valid protobuf SCIP.
fn fake_graph_build_consumes_artifacts(
    artifacts: &[super::ScipArtifact],
) -> BTreeMap<String, String> {
    artifacts
        .iter()
        .map(|artifact| {
            let payload = fs::read_to_string(&artifact.path).expect("read collected artifact");
            (artifact.workspace_slug.clone(), payload)
        })
        .collect()
}

/// Measurable cost outcome from a single simulated run.
#[derive(Debug)]
struct RunCost {
    /// Partitions that required a fake indexer invocation (cache miss).
    invocations: usize,
    /// Partitions served from cache (no invocation needed).
    cache_hits: usize,
    /// Final artifact count collected from disk.
    artifact_count: usize,
    /// Partition names whose fake indexer was invoked (cache misses). This is
    /// the deterministic stand-in for elapsed warm cost.
    invoked_partitions: Vec<String>,
    /// Partition names that the final fake graph-build stage consumed. This
    /// guards whole-graph semantics: even if only one partition changed, graph
    /// assembly must still see every current artifact.
    graph_build_partitions: BTreeSet<String>,
    /// The exact artifact payload consumed by the fake graph build for each
    /// partition. Payloads include source content hashes, so this proves the
    /// changed partition contributes its current artifact while unchanged
    /// partitions are replayed from cache.
    graph_build_payloads: BTreeMap<String, String>,
}

/// Build deterministic fake partitions.
fn make_partitions() -> Vec<FakePartition> {
    (0..PARTITION_COUNT)
        .map(|i| FakePartition {
            name: format!("pkg-{i}"),
            source_content: format!("package pkg{i}\nfunc main() {{}}\n").into_bytes(),
        })
        .collect()
}

/// Build a [`PlannedIndexerCommand`] for a fake partition.
fn plan_for_partition(name: &str, output_root: &Path) -> PlannedIndexerCommand {
    let output_path = output_root.join(format!("fake-go-{name}.scip"));
    PlannedIndexerCommand {
        indexer: SupportedIndexer::Go,
        binary_path: "/fake/bin/scip-go".into(),
        args: vec![
            "index".into(),
            "-o".into(),
            output_path.to_string_lossy().into_owned(),
        ],
        working_directory: format!("/fake/work/{name}").into(),
        workspace_root: format!("/fake/work/{name}").into(),
        workspace_rel_root: name.into(),
        workspace_slug: name.to_string(),
        output_path,
    }
}

/// Compute a [`ScipCacheKey`] for a partition with the given source content.
fn key_for_partition(plan: &PlannedIndexerCommand, source: &[u8]) -> ScipCacheKey {
    let mut source_hashes = BTreeMap::new();
    source_hashes.insert("main.go".to_string(), cache::content_hash(source));
    CacheKeyIngredients::from_plan(
        plan,
        "fake-scip-go v1.0.0",
        source_hashes,
        BTreeMap::new(),
        BTreeMap::new(),
        BTreeMap::new(),
    )
    .cache_key()
    .expect("cache key")
}

// ---------------------------------------------------------------------------
// Simulation engine
// ---------------------------------------------------------------------------

/// Simulate a warm/cold run: for each partition, check the SCIP cache.
///
/// On a **miss** (cold or changed-input), a fake `.scip` artifact is written
/// to disk and stored in the cache — simulating a real indexer invocation.
/// On a **hit** (warm-unchanged), the cache populates the output path
/// directly — simulating a zero-cost warm path.
///
/// Returns [`RunCost`] metrics and the executed commands needed for artifact
/// collection.
fn simulate_run(
    store: &ScipCacheStore,
    output_root: &Path,
    partitions: &[FakePartition],
) -> RunCost {
    fs::create_dir_all(output_root).expect("create output dir");

    let plans: Vec<PlannedIndexerCommand> = partitions
        .iter()
        .map(|p| plan_for_partition(&p.name, output_root))
        .collect();

    let mut invocations = 0usize;
    let mut cache_hits = 0usize;
    let mut invoked_partitions = Vec::new();
    let mut commands = Vec::new();

    for (plan, partition) in plans.iter().zip(partitions.iter()) {
        let key = key_for_partition(plan, &partition.source_content);

        match store.lookup(&key, &plan.output_path) {
            CacheLookup::Hit => {
                cache_hits += 1;
            }
            CacheLookup::Miss => {
                invocations += 1;
                invoked_partitions.push(partition.name.clone());
                // Simulate indexer invocation: write a fake SCIP artifact
                // whose content is deterministic per partition and source
                // content hash. A changed partition therefore produces a new
                // current artifact while unchanged partitions can be replayed
                // from the versioned cache.
                let content = fake_artifact_payload(partition);
                fs::write(&plan.output_path, &content).expect("write fake artifact");
                // Store in cache so subsequent warm runs can reuse it.
                store
                    .store_artifact(&key, &plan.output_path)
                    .expect("cache artifact");
            }
        }

        // Every partition produces an executed command entry, mirroring the
        // production flow where both CachedHit and Ran outcomes are recorded.
        commands.push(ExecutedIndexerCommand {
            plan: plan.clone(),
            exit_code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
        });
    }

    // Final graph assembly: collect all artifacts from disk. This is the
    // same step the production `run_indexers` pipeline performs.
    let artifacts = collect_scip_artifacts(output_root, &commands).expect("collect artifacts");
    let graph_build_payloads = fake_graph_build_consumes_artifacts(&artifacts);
    let graph_build_partitions = graph_build_payloads.keys().cloned().collect();

    RunCost {
        invocations,
        cache_hits,
        artifact_count: artifacts.len(),
        invoked_partitions,
        graph_build_partitions,
        graph_build_payloads,
    }
}

// ---------------------------------------------------------------------------
// Acceptance criteria tests
// ---------------------------------------------------------------------------

/// AC1: Cold run invokes all fake partitions, produces full artifact set.
#[test]
fn cold_run_invokes_all_partitions_and_produces_full_artifact_set() {
    let tmp = tempfile::Builder::new()
        .prefix("djinn-warm-cold-")
        .tempdir_in(".")
        .expect("tempdir");
    let cache_root = tmp.path().join("cache");
    let output_root = tmp.path().join("output");

    let store = ScipCacheStore::new(&cache_root);
    let partitions = make_partitions();

    let cost = simulate_run(&store, &output_root, &partitions);

    assert_eq!(
        cost.invocations, PARTITION_COUNT,
        "cold run must invoke every partition"
    );
    assert_eq!(cost.cache_hits, 0, "cold run must have zero cache hits");
    assert_eq!(
        cost.artifact_count, PARTITION_COUNT,
        "cold run must produce one artifact per partition"
    );
    assert_eq!(
        cost.invoked_partitions,
        partitions
            .iter()
            .map(|partition| partition.name.clone())
            .collect::<Vec<_>>(),
        "cold run must invoke every partition in deterministic order"
    );
    assert_eq!(
        cost.graph_build_payloads,
        expected_payloads(&partitions),
        "cold graph build must consume the complete current artifact payload set"
    );
}

/// AC1 + AC3: Warm-unchanged is cache-hit dominated with zero invocations,
/// demonstrating a measurable cost reduction vs cold.
#[test]
fn warm_unchanged_is_cache_hit_dominated_with_zero_invocations() {
    let tmp = tempfile::Builder::new()
        .prefix("djinn-warm-unchanged-")
        .tempdir_in(".")
        .expect("tempdir");
    let cache_root = tmp.path().join("cache");
    let output_cold = tmp.path().join("output-cold");
    let output_warm = tmp.path().join("output-warm");

    let store = ScipCacheStore::new(&cache_root);
    let partitions = make_partitions();

    // Phase 1: cold run populates the cache
    let cold_cost = simulate_run(&store, &output_cold, &partitions);
    assert_eq!(cold_cost.invocations, PARTITION_COUNT);

    // Phase 2: warm run with identical inputs — all cache hits
    let warm_cost = simulate_run(&store, &output_warm, &partitions);

    assert_eq!(
        warm_cost.invocations, 0,
        "warm-unchanged must invoke zero partitions"
    );
    assert_eq!(
        warm_cost.cache_hits, PARTITION_COUNT,
        "warm-unchanged must hit cache for all partitions"
    );
    assert_eq!(
        warm_cost.artifact_count, PARTITION_COUNT,
        "warm-unchanged must still produce full artifact set"
    );
    assert!(
        warm_cost.invoked_partitions.is_empty(),
        "warm-unchanged graph warm must not invoke any partition: {:?}",
        warm_cost.invoked_partitions
    );
    assert_eq!(
        warm_cost.graph_build_payloads,
        expected_payloads(&partitions),
        "warm-unchanged graph build must consume the same full current artifact set from cache"
    );

    // Measurable warm-cost reduction signal
    let reduction = cold_cost.invocations - warm_cost.invocations;
    eprintln!(
        "warm-unchanged cost: cold={cold} invocations, warm={warm} invocations, \
         reduction={reduction}/{total} (100%)",
        cold = cold_cost.invocations,
        warm = warm_cost.invocations,
        total = PARTITION_COUNT,
    );
    assert_eq!(
        reduction, PARTITION_COUNT,
        "warm-unchanged cost reduction must equal partition count (100% reduction)"
    );
}

/// AC1 + AC3: One-partition-changed warm invokes only that partition while
/// the remaining N-1 are served from cache.
#[test]
fn warm_one_partition_changed_invokes_only_that_partition() {
    let tmp = tempfile::Builder::new()
        .prefix("djinn-warm-one-changed-")
        .tempdir_in(".")
        .expect("tempdir");
    let cache_root = tmp.path().join("cache");
    let output_cold = tmp.path().join("output-cold");
    let output_warm = tmp.path().join("output-warm");

    let store = ScipCacheStore::new(&cache_root);
    let mut partitions = make_partitions();

    // Phase 1: cold run populates cache
    let cold_cost = simulate_run(&store, &output_cold, &partitions);
    assert_eq!(cold_cost.invocations, PARTITION_COUNT);

    // Phase 2: modify one partition's source content (simulating a file edit)
    let changed_index = 2;
    partitions[changed_index].source_content = b"package pkg2\nfunc updated() {}\n".to_vec();

    let warm_cost = simulate_run(&store, &output_warm, &partitions);

    assert_eq!(
        warm_cost.invocations, 1,
        "warm-one-changed must invoke exactly 1 partition (the changed one)"
    );
    assert_eq!(
        warm_cost.cache_hits,
        PARTITION_COUNT - 1,
        "warm-one-changed must hit cache for unchanged partitions"
    );
    assert_eq!(
        warm_cost.artifact_count, PARTITION_COUNT,
        "warm-one-changed must still produce full artifact set (all partitions)"
    );
    assert_eq!(
        warm_cost.invoked_partitions,
        vec![partitions[changed_index].name.clone()],
        "warm-one-changed must invoke exactly the changed partition"
    );
    assert_eq!(
        warm_cost.graph_build_payloads,
        expected_payloads(&partitions),
        "warm-one-changed graph build must consume the full current artifact set, not a changed-partition-only subset"
    );

    // Measurable warm-cost reduction signal
    let reduction = cold_cost.invocations - warm_cost.invocations;
    eprintln!(
        "warm-one-changed cost: cold={cold} invocations, warm={warm} invocation, \
         reduction={reduction}/{total} ({pct}%)",
        cold = cold_cost.invocations,
        warm = warm_cost.invocations,
        total = PARTITION_COUNT,
        pct = reduction * 100 / PARTITION_COUNT,
    );
    assert_eq!(
        reduction,
        PARTITION_COUNT - 1,
        "warm-one-changed cost reduction must be (N-1)"
    );
}

/// AC2: The final graph assembly must ALWAYS consume the full current artifact
/// set. Even when only one partition is re-indexed, the artifact set must
/// contain ALL partitions (from cache + fresh index). This guards against a
/// regression where a "changed-file-only" optimization might skip unchanged
/// partitions from the final artifact set.
#[test]
fn whole_graph_semantics_never_produce_changed_file_only_artifact_set() {
    let tmp = tempfile::Builder::new()
        .prefix("djinn-whole-graph-")
        .tempdir_in(".")
        .expect("tempdir");
    let cache_root = tmp.path().join("cache");
    let output_cold = tmp.path().join("output-cold");
    let output_warm = tmp.path().join("output-warm");

    let store = ScipCacheStore::new(&cache_root);
    let mut partitions = make_partitions();

    // Cold run: populate cache
    simulate_run(&store, &output_cold, &partitions);

    // Change one partition
    partitions[1].source_content = b"package pkg1\nfunc changed() {}\n".to_vec();

    // Warm run
    let warm_cost = simulate_run(&store, &output_warm, &partitions);

    // Even though only 1 partition was re-indexed, the final artifact set
    // must contain ALL partitions (from cache + fresh index).
    assert_eq!(
        warm_cost.artifact_count,
        PARTITION_COUNT,
        "final artifact set must contain ALL partitions regardless of which changed; \
         got {got} artifacts, expected {expected}",
        got = warm_cost.artifact_count,
        expected = PARTITION_COUNT,
    );
    assert_eq!(
        warm_cost.graph_build_partitions,
        partitions
            .iter()
            .map(|partition| partition.name.clone())
            .collect::<BTreeSet<_>>(),
        "fake graph build must consume every partition, not only the changed partition"
    );
    assert_eq!(
        warm_cost.graph_build_payloads,
        expected_payloads(&partitions),
        "fake graph build must consume current payloads for all partitions, including the changed partition"
    );

    // Verify the individual artifact files exist on disk (not just counted).
    for partition in &partitions {
        let expected_path = output_warm.join(format!("fake-go-{}.scip", partition.name));
        assert!(
            expected_path.exists(),
            "artifact for partition {} must exist at {}",
            partition.name,
            expected_path.display(),
        );
    }
}

// ---------------------------------------------------------------------------
// Cache key construction correctness
// ---------------------------------------------------------------------------

/// Different source content must produce different cache keys; same content
/// must produce the same key deterministically.
#[test]
fn cache_key_changes_with_source_content_and_is_deterministic() {
    let output_root = Path::new("/tmp/djinn-key-test");
    let plan = plan_for_partition("key-test", output_root);

    let key_a = key_for_partition(&plan, b"original content");
    let key_b = key_for_partition(&plan, b"modified content");
    let key_c = key_for_partition(&plan, b"original content");

    assert_ne!(
        key_a.as_str(),
        key_b.as_str(),
        "different source content must produce different cache keys"
    );
    assert_eq!(
        key_a.as_str(),
        key_c.as_str(),
        "same source content must produce the same cache key deterministically"
    );
}

/// A tool version bump must produce a different cache key, causing a cache
/// miss even when source content is unchanged. This ensures the versioned
/// cache does not replay stale artifacts after a SCIP tool upgrade.
#[test]
fn versioned_cache_misses_on_tool_version_bump() {
    let tmp = tempfile::Builder::new()
        .prefix("djinn-version-bump-")
        .tempdir_in(".")
        .expect("tempdir");
    let cache_root = tmp.path().join("cache");
    let output_root = tmp.path().join("output");
    fs::create_dir_all(&cache_root).unwrap();
    fs::create_dir_all(&output_root).unwrap();

    let store = ScipCacheStore::new(&cache_root);
    let plan = plan_for_partition("ver-test", &output_root);
    let source = b"package ver_test\nfunc init() {}\n";

    // Build key with tool version v1 and store an artifact
    let key_v1 = {
        let mut h = BTreeMap::new();
        h.insert("main.go".to_string(), cache::content_hash(source));
        CacheKeyIngredients::from_plan(
            &plan,
            "scip-go v1.0.0",
            h,
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::new(),
        )
        .cache_key()
        .unwrap()
    };

    let fake_content = b"fake-scip-v1-artifact";
    fs::write(&plan.output_path, fake_content).unwrap();
    store
        .store_artifact(&key_v1, &plan.output_path)
        .expect("store v1");

    // Remove output file to test cache retrieval
    fs::remove_file(&plan.output_path).unwrap();

    // Same key -> cache hit
    assert_eq!(
        store.lookup(&key_v1, &plan.output_path),
        CacheLookup::Hit,
        "same version + same source must produce cache hit"
    );
    fs::remove_file(&plan.output_path).unwrap();

    // Build key with tool version v2 (same source)
    let key_v2 = {
        let mut h = BTreeMap::new();
        h.insert("main.go".to_string(), cache::content_hash(source));
        CacheKeyIngredients::from_plan(
            &plan,
            "scip-go v2.0.0",
            h,
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::new(),
        )
        .cache_key()
        .unwrap()
    };

    assert_ne!(
        key_v1.as_str(),
        key_v2.as_str(),
        "different tool versions must produce different cache keys"
    );
    assert_eq!(
        store.lookup(&key_v2, &plan.output_path),
        CacheLookup::Miss,
        "version bump must produce cache miss even with same source content"
    );
}
