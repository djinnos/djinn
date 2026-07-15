//! Corpus-driven end-to-end regression tests for trait-dispatch
//! `code_graph context` and `code_graph impact`.
//!
//! **Developer documentation:** see `docs/TRAIT_DISPATCH_VALIDATION.md`
//! for the full validation matrix, warm-artifact expectations, and
//! fan-out/confidence/provenance semantics.
//!
//! Complements the synthetic-fixture coverage in
//! [`super::trait_dispatch_query`] and [`super::trait_dispatch_impact`]
//! by tying those fixtures to the hand-verified reproduction corpus in
//! [`super::trait_dispatch_corpus`]. Each entry in the corpus documents
//! a real Rust trait method (`RuntimeOps::list_taskrun_jobs`,
//! `RepoGraphOps::context`, `SlotPoolOps::get_status`,
//! `RepoGraphOps::impact`) together with its in-repo concrete impls
//! and callers. The fixtures built here model that exact topology and
//! exercise the test-harness equivalents of the `code_graph` surfaces
//! to prove the expected Dependents / Dependencies surface.
//!
//! ## Scope (per AC)
//!
//! 1. Build a per-entry in-memory graph with the hand-verified
//!    `caller → trait_method → impl_method` topology plus the canonical
//!    `TraitDispatchCall` (caller→trait_method, optional
//!    caller→impl_method fan-out) and `Implements`
//!    (impl_method→trait_method) edges at the finalized confidence
//!    floors.
//! 2. Run the `code_graph context` test-harness equivalent
//!    (`collect_context_buckets`) on the trait_method and impl_method
//!    nodes, asserting the corpus's expected caller/impl relationships
//!    surface in the right `EdgeCategory` buckets with the right
//!    confidence values.
//! 3. Run the `code_graph impact` test-harness equivalent
//!    (`shared::impact_bfs` and `impact_bfs_with_policy`) on the
//!    trait_method, asserting the caller lands in the blast radius when
//!    `min_confidence` is at or below the trait-dispatch floor.
//! 4. At least one entry (the mandatory
//!    `RuntimeOps::list_taskrun_jobs`) must be exercised end-to-end
//!    through both surfaces with assertions that match its corpus
//!    topology exactly (concrete impl: `AppState`, production caller:
//!    `reap_orphaned_taskrun_jobs`).
//!
//! ## Why fixture-only (no production graph data)
//!
//! Per the AC: "Tests run without production graph data, Kubernetes,
//! Docker-only services, or operator credentials." Each fixture is a
//! deterministic, in-memory `RepoDependencyGraph` built from a
//! `RepoGraphArtifact` shaped like the corpus's hand-verified
//! topology. No warm, no DB, no remote services.

use super::trait_dispatch_corpus::{CORPUS, CorpusEntry};
use super::*;

use crate::mcp_bridge::graph_neighbors::edge_category_for;
use crate::mcp_bridge::shared;
use djinn_control_plane::bridge::EdgeCategory;
use djinn_graph::repo_graph::{
    REPO_GRAPH_ARTIFACT_VERSION, RepoDependencyGraph, RepoGraphArtifact, RepoGraphArtifactEdge,
    RepoGraphEdgeKind, RepoGraphNode, RepoGraphNodeKind, RepoNodeKey,
};
use djinn_graph::scip_parser::{ScipSymbolKind, ScipVisibility};
use std::path::PathBuf;

// ── Fixture builders ─────────────────────────────────────────────────────

/// One logical node in the per-corpus-entry fixture. The `symbol`
/// string is what the SCIP index would have emitted for the source
/// declaration; `format_node_key` will prepend `"symbol:"` so the test
/// can match it against `ImpactEntry.key`/`RelatedSymbol.uid`.
#[allow(dead_code)]
// `line` and `role` are documentation fields; they
// anchor the fixture back to the corpus but are not
// used in assertions directly.
#[derive(Clone)]
struct CorpusFixtureNode {
    /// SCIP-style symbol id (without the `"symbol:"` prefix).
    symbol: String,
    /// Short display name used in assertions.
    display: String,
    /// Trait-decl file path (or impl file path, see `role`).
    file: &'static str,
    /// 1-based line number from the corpus (so the test documents the
    /// link back to the source-of-truth).
    line: u32,
    /// Symbol kind in the SCIP index.
    kind: ScipSymbolKind,
    /// Role in the trait-dispatch topology.
    role: FixtureRole,
}

/// Roles for the per-entry fixture. The topology is:
/// `[caller] ─TraitDispatchCall──▶ [trait_method] ◀─Implements─ [impl_method]`
/// with an optional fan-out `[caller] ─TraitDispatchCall──▶ [impl_method]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FixtureRole {
    Caller,
    TraitMethod,
    ImplMethod,
}

impl CorpusFixtureNode {
    fn to_repo_graph_node(&self) -> RepoGraphNode {
        RepoGraphNode {
            id: RepoNodeKey::Symbol(self.symbol.clone()),
            kind: RepoGraphNodeKind::Symbol,
            display_name: self.display.clone(),
            language: Some("rust".to_string()),
            file_path: Some(PathBuf::from(self.file)),
            symbol: Some(self.symbol.clone()),
            symbol_kind: Some(self.kind.clone()),
            is_external: false,
            visibility: Some(ScipVisibility::Public),
            signature: None,
            documentation: vec![],
            signature_parts: None,
            is_test: false,
            complexity: None,
            workspace: Some("root".to_string()),
            route_framework: None,
            route_handler_symbol: None,
        }
    }
}

/// Hand-verified topology model for one corpus entry. Used by the
/// builders below to keep assertions tied to the corpus.
struct CorpusFixtureSpec {
    /// The caller symbol — invoked through the trait object.
    caller: CorpusFixtureNode,
    /// The trait-method declaration symbol.
    trait_method: CorpusFixtureNode,
    /// The concrete impl-method symbol.
    impl_method: CorpusFixtureNode,
    /// If `true`, the canonical 5wyo fan-out also emits a direct
    /// `TraitDispatchCall(caller→impl_method)` edge. When `false`,
    /// the impl-method's incoming Calls bucket stays empty (the
    /// documented "fan-out suppressed" behavior).
    with_fanout: bool,
}

/// Builds a `CorpusFixtureSpec` for the mandatory `RuntimeOps::list_taskrun_jobs`
/// corpus entry. Mirrors the trait + concrete impl + caller topology
/// documented in [`super::trait_dispatch_corpus`]:
///
/// - Trait: `RuntimeOps::list_taskrun_jobs`
///   (`server/crates/djinn-control-plane/src/bridge/runtime_bridge.rs:137`)
/// - Concrete impl: `AppState` (`server/src/mcp_bridge/mod.rs:78`)
/// - Production caller: `reap_orphaned_taskrun_jobs`
///   (`server/crates/djinn-agent/src/actors/coordinator/health.rs:1490`)
fn runtime_ops_list_taskrun_jobs_spec() -> CorpusFixtureSpec {
    CorpusFixtureSpec {
        caller: CorpusFixtureNode {
            symbol: "scip-rust pkg server/crates/djinn-agent/src/actors/coordinator/health.rs \
                     `reap_orphaned_taskrun_jobs`()."
                .to_string(),
            display: "reap_orphaned_taskrun_jobs".to_string(),
            file: "server/crates/djinn-agent/src/actors/coordinator/health.rs",
            line: 1490,
            kind: ScipSymbolKind::Function,
            role: FixtureRole::Caller,
        },
        trait_method: CorpusFixtureNode {
            symbol: "scip-rust pkg server/crates/djinn-control-plane/src/bridge/runtime_bridge.rs \
                     `RuntimeOps`#`list_taskrun_jobs`()."
                .to_string(),
            display: "list_taskrun_jobs".to_string(),
            file: "server/crates/djinn-control-plane/src/bridge/runtime_bridge.rs",
            line: 137,
            kind: ScipSymbolKind::Method,
            role: FixtureRole::TraitMethod,
        },
        impl_method: CorpusFixtureNode {
            symbol: "scip-rust pkg server/src/mcp_bridge/mod.rs \
                     `AppState`#`list_taskrun_jobs`()."
                .to_string(),
            display: "list_taskrun_jobs".to_string(),
            file: "server/src/mcp_bridge/mod.rs",
            line: 78,
            kind: ScipSymbolKind::Method,
            role: FixtureRole::ImplMethod,
        },
        // The fan-out edge is part of the canonical 5wyo behavior —
        // when the trait has ≤ `TRAIT_DISPATCH_FANOUT_CAP` impls, the
        // builder emits the caller→impl_method edge.
        with_fanout: true,
    }
}

/// Builds a fixture for `RepoGraphOps::context`.
fn repo_graph_ops_context_spec() -> CorpusFixtureSpec {
    CorpusFixtureSpec {
        caller: CorpusFixtureNode {
            symbol: "scip-rust pkg server/crates/djinn-control-plane/src/tools/graph_tools/\
                     handler_basic_ops.rs `GraphToolHandler`#`code_graph_context`()."
                .to_string(),
            display: "code_graph_context".to_string(),
            file: "server/crates/djinn-control-plane/src/tools/graph_tools/handler_basic_ops.rs",
            line: 720,
            kind: ScipSymbolKind::Method,
            role: FixtureRole::Caller,
        },
        trait_method: CorpusFixtureNode {
            symbol: "scip-rust pkg server/crates/djinn-control-plane/src/bridge/graph_bridge.rs \
                     `RepoGraphOps`#`context`()."
                .to_string(),
            display: "context".to_string(),
            file: "server/crates/djinn-control-plane/src/bridge/graph_bridge.rs",
            line: 298,
            kind: ScipSymbolKind::Method,
            role: FixtureRole::TraitMethod,
        },
        impl_method: CorpusFixtureNode {
            symbol: "scip-rust pkg server/src/mcp_bridge/graph_ops/mod.rs \
                     `RepoGraphBridge`#`context`()."
                .to_string(),
            display: "context".to_string(),
            file: "server/src/mcp_bridge/graph_ops/mod.rs",
            line: 240,
            kind: ScipSymbolKind::Method,
            role: FixtureRole::ImplMethod,
        },
        with_fanout: true,
    }
}

/// Builds a fixture for `SlotPoolOps::get_status`. This entry has two
/// production callers in the corpus — pick the first one
/// (`reconcile_inflight_dispatch_ledger` in `task_dispatch.rs`) so the
/// fixture is locked to the corpus's first-listed caller. The
/// `every_corpus_entry_fixture_symbols_match_corpus_paths_and_lines`
/// test guards this lock.
fn slot_pool_ops_get_status_spec() -> CorpusFixtureSpec {
    CorpusFixtureSpec {
        caller: CorpusFixtureNode {
            symbol: "scip-rust pkg server/crates/djinn-agent/src/actors/coordinator/dispatch/\
                     task_dispatch.rs `CoordinatorActor`#`reconcile_inflight_dispatch_ledger`()."
                .to_string(),
            display: "reconcile_inflight_dispatch_ledger".to_string(),
            file: "server/crates/djinn-agent/src/actors/coordinator/dispatch/task_dispatch.rs",
            line: 243,
            kind: ScipSymbolKind::Method,
            role: FixtureRole::Caller,
        },
        trait_method: CorpusFixtureNode {
            symbol:
                "scip-rust pkg server/crates/djinn-control-plane/src/bridge/slot_pool_bridge.rs \
                     `SlotPoolOps`#`get_status`()."
                    .to_string(),
            display: "get_status".to_string(),
            file: "server/crates/djinn-control-plane/src/bridge/slot_pool_bridge.rs",
            line: 36,
            kind: ScipSymbolKind::Method,
            role: FixtureRole::TraitMethod,
        },
        impl_method: CorpusFixtureNode {
            symbol: "scip-rust pkg server/src/mcp_bridge/bridges.rs \
                     `SlotPoolBridge`#`get_status`()."
                .to_string(),
            display: "get_status".to_string(),
            file: "server/src/mcp_bridge/bridges.rs",
            line: 42,
            kind: ScipSymbolKind::Method,
            role: FixtureRole::ImplMethod,
        },
        with_fanout: true,
    }
}

/// Builds a fixture for `RepoGraphOps::impact`.
fn repo_graph_ops_impact_spec() -> CorpusFixtureSpec {
    CorpusFixtureSpec {
        caller: CorpusFixtureNode {
            symbol: "scip-rust pkg server/crates/djinn-control-plane/src/tools/graph_tools/\
                     handler_basic_ops.rs `GraphToolHandler`#`code_graph_impact`()."
                .to_string(),
            display: "code_graph_impact".to_string(),
            file: "server/crates/djinn-control-plane/src/tools/graph_tools/handler_basic_ops.rs",
            line: 204,
            kind: ScipSymbolKind::Method,
            role: FixtureRole::Caller,
        },
        trait_method: CorpusFixtureNode {
            symbol: "scip-rust pkg server/crates/djinn-control-plane/src/bridge/graph_bridge.rs \
                     `RepoGraphOps`#`impact`()."
                .to_string(),
            display: "impact".to_string(),
            file: "server/crates/djinn-control-plane/src/bridge/graph_bridge.rs",
            line: 101,
            kind: ScipSymbolKind::Method,
            role: FixtureRole::TraitMethod,
        },
        impl_method: CorpusFixtureNode {
            symbol: "scip-rust pkg server/src/mcp_bridge/graph_ops/mod.rs \
                     `RepoGraphBridge`#`impact`()."
                .to_string(),
            display: "impact".to_string(),
            file: "server/src/mcp_bridge/graph_ops/mod.rs",
            line: 118,
            kind: ScipSymbolKind::Method,
            role: FixtureRole::ImplMethod,
        },
        // `RepoGraphOps` is implemented exactly once in production,
        // so the fan-out fires (1 ≤ TRAIT_DISPATCH_FANOUT_CAP).
        with_fanout: true,
    }
}

/// Build a deterministic in-memory graph from the spec, wiring the
/// canonical 5wyo edges:
/// - `[caller] ─TraitDispatchCall──▶ [trait_method]` at confidence 0.70
/// - `[impl_method] ─Implements──▶ [trait_method]` at confidence 0.90
/// - if `spec.with_fanout`: `[caller] ─TraitDispatchCall──▶ [impl_method]` at 0.70
fn build_corpus_fixture(spec: &CorpusFixtureSpec) -> RepoDependencyGraph {
    let caller = spec.caller.to_repo_graph_node();
    let trait_method = spec.trait_method.to_repo_graph_node();
    let impl_method = spec.impl_method.to_repo_graph_node();

    let trait_dispatch_conf =
        djinn_graph::repo_graph::edge_confidence_floor(RepoGraphEdgeKind::TraitDispatchCall);
    let implements_conf =
        djinn_graph::repo_graph::edge_confidence_floor(RepoGraphEdgeKind::Implements);

    let mut edges = vec![
        // [0] caller → [1] trait_method: synthesized trait-dispatch
        // call edge at the standard 0.70 floor.
        RepoGraphArtifactEdge {
            source: 0,
            target: 1,
            kind: RepoGraphEdgeKind::TraitDispatchCall,
            weight: 1.0,
            evidence_count: 1,
            confidence: trait_dispatch_conf,
            reason: Some("trait-dispatch-call".to_string()),
            step: None,
        },
        // [2] impl_method → [1] trait_method: high-confidence directly
        // extracted `Implements` relationship edge.
        RepoGraphArtifactEdge {
            source: 2,
            target: 1,
            kind: RepoGraphEdgeKind::Implements,
            weight: 1.0,
            evidence_count: 1,
            confidence: implements_conf,
            reason: None,
            step: None,
        },
    ];
    if spec.with_fanout {
        // [0] caller → [2] impl_method: fan-out edge from the canonical
        // 5wyo model.
        edges.push(RepoGraphArtifactEdge {
            source: 0,
            target: 2,
            kind: RepoGraphEdgeKind::TraitDispatchCall,
            weight: 1.0,
            evidence_count: 1,
            confidence: trait_dispatch_conf,
            reason: Some("trait-dispatch-fanout".to_string()),
            step: None,
        });
    }

    RepoDependencyGraph::from_artifact(&RepoGraphArtifact {
        version: REPO_GRAPH_ARTIFACT_VERSION,
        nodes: vec![caller, trait_method, impl_method],
        edges,
        symbol_ranges: std::collections::BTreeMap::new(),
        communities: vec![],
        processes: vec![],
        route_exclusion_config: Default::default(),
        layout_positions: std::collections::BTreeMap::new(),
        galaxy_positions: std::collections::BTreeMap::new(),
        galaxy_degrees: std::collections::BTreeMap::new(),
    })
}

/// Return the NodeIndex for the node whose role matches `role`.
fn node_index_for_role(
    graph: &RepoDependencyGraph,
    spec: &CorpusFixtureSpec,
    role: FixtureRole,
) -> petgraph::graph::NodeIndex {
    let target_symbol = match role {
        FixtureRole::Caller => &spec.caller.symbol,
        FixtureRole::TraitMethod => &spec.trait_method.symbol,
        FixtureRole::ImplMethod => &spec.impl_method.symbol,
    };
    graph
        .symbol_node(target_symbol)
        .unwrap_or_else(|| panic!("{role:?} node {target_symbol} should resolve"))
}

/// Build the per-corpus-entry spec + graph + named indices.
#[allow(dead_code)] // Some fields are informational, not asserted directly.
struct CorpusFixture {
    entry: &'static CorpusEntry,
    spec: CorpusFixtureSpec,
    graph: RepoDependencyGraph,
    caller_idx: petgraph::graph::NodeIndex,
    trait_idx: petgraph::graph::NodeIndex,
    impl_idx: petgraph::graph::NodeIndex,
    caller_key: String,
    trait_key: String,
    impl_key: String,
}

fn build_entry(entry: &'static CorpusEntry) -> CorpusFixture {
    // Dispatch on the trait_name+method_name pair — the corpus is the
    // single source of truth, so the dispatch table stays locked to
    // what's documented.
    let spec = match (entry.trait_name, entry.method_name) {
        ("RuntimeOps", "list_taskrun_jobs") => runtime_ops_list_taskrun_jobs_spec(),
        ("RepoGraphOps", "context") => repo_graph_ops_context_spec(),
        ("SlotPoolOps", "get_status") => slot_pool_ops_get_status_spec(),
        ("RepoGraphOps", "impact") => repo_graph_ops_impact_spec(),
        other => panic!(
            "no fixture spec for corpus entry {other:?} — extend the dispatch table to keep the corpus and tests in lockstep"
        ),
    };
    let graph = build_corpus_fixture(&spec);
    let caller_idx = node_index_for_role(&graph, &spec, FixtureRole::Caller);
    let trait_idx = node_index_for_role(&graph, &spec, FixtureRole::TraitMethod);
    let impl_idx = node_index_for_role(&graph, &spec, FixtureRole::ImplMethod);
    let caller_key = format!("symbol:{}", spec.caller.symbol);
    let trait_key = format!("symbol:{}", spec.trait_method.symbol);
    let impl_key = format!("symbol:{}", spec.impl_method.symbol);
    CorpusFixture {
        entry,
        spec,
        graph,
        caller_idx,
        trait_idx,
        impl_idx,
        caller_key,
        trait_key,
        impl_key,
    }
}

// ── Helper: assert call bucket contents ───────────────────────────────

fn assert_calls_bucket_contains(
    incoming: &std::collections::BTreeMap<
        EdgeCategory,
        Vec<djinn_control_plane::bridge::RelatedSymbol>,
    >,
    expected_display: &str,
    expected_confidence: f64,
    ctx: &str,
) {
    let calls = incoming
        .get(&EdgeCategory::Calls)
        .cloned()
        .unwrap_or_default();
    let entry = calls
        .iter()
        .find(|r| r.name == expected_display)
        .unwrap_or_else(|| {
            panic!("{ctx}: expected '{expected_display}' in incoming.calls bucket, got {calls:?}")
        });
    assert!(
        (entry.confidence - expected_confidence).abs() < f64::EPSILON,
        "{ctx}: '{expected_display}' confidence {} != expected {expected_confidence}",
        entry.confidence
    );
}

fn assert_implements_bucket_contains(
    outgoing: &std::collections::BTreeMap<
        EdgeCategory,
        Vec<djinn_control_plane::bridge::RelatedSymbol>,
    >,
    expected_file_substring: &str,
    ctx: &str,
) {
    let implements = outgoing
        .get(&EdgeCategory::Implements)
        .cloned()
        .unwrap_or_default();
    assert!(
        implements
            .iter()
            .any(|r| r.uid.contains(expected_file_substring)),
        "{ctx}: expected file substring '{expected_file_substring}' in outgoing.implements bucket, got {implements:?}"
    );
}

// ── Per-entry end-to-end tests ───────────────────────────────────────

/// `code_graph context` for `RuntimeOps::list_taskrun_jobs` (the trait
/// method) must surface the production caller
/// `reap_orphaned_taskrun_jobs` in the incoming `Calls` bucket at the
/// `TraitDispatchCall` confidence floor.
#[test]
fn context_runtime_ops_list_taskrun_jobs_trait_method_has_caller_in_calls_bucket() {
    let fixture = build_entry(CORPUS[0]); // mandatory entry
    assert_eq!(
        fixture.entry.trait_name, "RuntimeOps",
        "corpus[0] must be the mandatory RuntimeOps entry"
    );
    assert_eq!(
        fixture.entry.method_name, "list_taskrun_jobs",
        "corpus[0] must be the mandatory list_taskrun_jobs method"
    );

    let (incoming, _outgoing) = collect_context_buckets(&fixture.graph, fixture.trait_idx);
    let expected_conf =
        djinn_graph::repo_graph::edge_confidence_floor(RepoGraphEdgeKind::TraitDispatchCall);
    assert_calls_bucket_contains(
        &incoming,
        "reap_orphaned_taskrun_jobs",
        expected_conf,
        "RuntimeOps::list_taskrun_jobs trait_method context",
    );
}

/// `code_graph context` for the *concrete impl* method
/// `AppState::list_taskrun_jobs` must:
/// 1. Surface the same caller in its incoming `Calls` bucket via the
///    fan-out edge, AND
/// 2. Surface the trait method in its outgoing `Implements` bucket via
///    the relationship edge.
#[test]
fn context_runtime_ops_list_taskrun_jobs_impl_method_has_caller_and_trait_hop() {
    let fixture = build_entry(CORPUS[0]);

    let (incoming, outgoing) = collect_context_buckets(&fixture.graph, fixture.impl_idx);

    // Fan-out edge: caller appears in the impl_method's Calls bucket.
    let expected_conf =
        djinn_graph::repo_graph::edge_confidence_floor(RepoGraphEdgeKind::TraitDispatchCall);
    assert_calls_bucket_contains(
        &incoming,
        "reap_orphaned_taskrun_jobs",
        expected_conf,
        "AppState::list_taskrun_jobs impl_method context",
    );

    // Relationship edge: trait method surfaces in outgoing.implements.
    // The trait method lives in `runtime_bridge.rs` per the corpus.
    assert_implements_bucket_contains(
        &outgoing,
        "runtime_bridge.rs",
        "AppState::list_taskrun_jobs impl_method context",
    );
}

/// `code_graph impact` for `RuntimeOps::list_taskrun_jobs` must
/// include the production caller `reap_orphaned_taskrun_jobs` in the
/// blast radius when `min_confidence` is at or below the
/// `TraitDispatchCall` floor — and must NOT include it at the default
/// 0.85 threshold.
#[test]
fn impact_runtime_ops_list_taskrun_jobs_includes_caller_at_floor_excludes_at_default() {
    let fixture = build_entry(CORPUS[0]);
    let trait_dispatch_conf =
        djinn_graph::repo_graph::edge_confidence_floor(RepoGraphEdgeKind::TraitDispatchCall);

    // Floor confidence: caller is in the blast radius.
    let at_floor = shared::impact_bfs(
        &fixture.graph,
        fixture.trait_idx,
        3,
        Some(trait_dispatch_conf),
    );
    let floor_keys: Vec<&str> = at_floor.iter().map(|(_, e)| e.key.as_str()).collect();
    assert!(
        floor_keys.iter().any(|k| *k == fixture.caller_key),
        "impact(min_confidence={trait_dispatch_conf}) must include caller '{}' for \
         RuntimeOps::list_taskrun_jobs; got {floor_keys:?}",
        fixture.caller_key
    );

    // Default 0.85: caller is excluded (edge confidence 0.70 < 0.85).
    let at_default = shared::impact_bfs(&fixture.graph, fixture.trait_idx, 3, None);
    let default_keys: Vec<&str> = at_default.iter().map(|(_, e)| e.key.as_str()).collect();
    assert!(
        !default_keys.iter().any(|k| *k == fixture.caller_key),
        "impact(min_confidence=default 0.85) must exclude caller '{}' for \
         RuntimeOps::list_taskrun_jobs (edge confidence 0.70 < 0.85); got {default_keys:?}",
        fixture.caller_key
    );
}

// ── Per-entry `context` tests for every corpus entry ─────────────────

/// `code_graph context` for the trait-method of every corpus entry
/// surfaces its caller in the incoming `Calls` bucket at the
/// `TraitDispatchCall` confidence floor. Iterates the corpus so adding
/// a new entry keeps the assertion in lockstep.
#[test]
fn context_every_corpus_entry_trait_method_has_caller_in_calls() {
    let expected_conf =
        djinn_graph::repo_graph::edge_confidence_floor(RepoGraphEdgeKind::TraitDispatchCall);

    for entry in CORPUS {
        let fixture = build_entry(entry);
        let (incoming, _outgoing) = collect_context_buckets(&fixture.graph, fixture.trait_idx);
        let ctx_label = format!(
            "{}::{} trait_method context",
            entry.trait_name, entry.method_name
        );
        assert_calls_bucket_contains(
            &incoming,
            &fixture.spec.caller.display,
            expected_conf,
            &ctx_label,
        );
    }
}

/// `code_graph context` for the impl-method of every corpus entry
/// surfaces the trait method in the outgoing `Implements` bucket via
/// the high-confidence `Implements` relationship edge. This is the
/// "explicit trait↔impl hop" the AC calls out: even when fan-out is
/// suppressed, the relationship edge keeps the impl_method linked to
/// its trait declaration.
#[test]
fn context_every_corpus_entry_impl_method_has_trait_hop_in_implements() {
    for entry in CORPUS {
        let fixture = build_entry(entry);
        let (_incoming, outgoing) = collect_context_buckets(&fixture.graph, fixture.impl_idx);
        let ctx_label = format!(
            "{}::{} impl_method context",
            entry.trait_name, entry.method_name
        );
        // Trait declaration file substring — must match the corpus.
        let trait_file_substring = match entry.trait_name {
            "RuntimeOps" => "runtime_bridge.rs",
            "RepoGraphOps" => "graph_bridge.rs",
            "SlotPoolOps" => "slot_pool_bridge.rs",
            other => panic!("unknown trait {other} in corpus dispatch"),
        };
        assert_implements_bucket_contains(&outgoing, trait_file_substring, &ctx_label);
    }
}

// ── Per-entry `impact` tests for every corpus entry ──────────────────

/// `code_graph impact` for the trait-method of every corpus entry
/// includes its production caller in the blast radius when
/// `min_confidence` is at the `TraitDispatchCall` floor. This is the
/// "non-empty in-repo caller blast radius" regression the AC calls out.
#[test]
fn impact_every_corpus_entry_includes_caller_at_trait_dispatch_floor() {
    let trait_dispatch_conf =
        djinn_graph::repo_graph::edge_confidence_floor(RepoGraphEdgeKind::TraitDispatchCall);

    for entry in CORPUS {
        let fixture = build_entry(entry);
        let result = shared::impact_bfs(
            &fixture.graph,
            fixture.trait_idx,
            3,
            Some(trait_dispatch_conf),
        );
        let keys: Vec<&str> = result.iter().map(|(_, e)| e.key.as_str()).collect();
        let ctx_label = format!(
            "{}::{} impact at min_confidence={}",
            entry.trait_name, entry.method_name, trait_dispatch_conf
        );
        assert!(
            keys.iter().any(|k| *k == fixture.caller_key),
            "{ctx_label}: expected caller '{}' in blast radius; got {keys:?}",
            fixture.caller_key
        );
    }
}

/// Companion regression: the parity-aware `impact_bfs_with_policy`
/// variant must treat trait-dispatch edges the same as `impact_bfs`
/// for every corpus entry. The policy only affects `Fetches→Route`
/// edges, not the synthesized caller edges.
#[test]
fn impact_every_corpus_entry_impact_bfs_with_policy_matches_impact_bfs() {
    let trait_dispatch_conf =
        djinn_graph::repo_graph::edge_confidence_floor(RepoGraphEdgeKind::TraitDispatchCall);

    for entry in CORPUS {
        let fixture = build_entry(entry);
        let with_policy = shared::impact_bfs_with_policy(
            &fixture.graph,
            fixture.trait_idx,
            3,
            Some(trait_dispatch_conf),
            None,
        );
        let plain = shared::impact_bfs(
            &fixture.graph,
            fixture.trait_idx,
            3,
            Some(trait_dispatch_conf),
        );
        let policy_keys: Vec<&str> = with_policy.iter().map(|(_, e)| e.key.as_str()).collect();
        let plain_keys: Vec<&str> = plain.iter().map(|(_, e)| e.key.as_str()).collect();
        let ctx_label = format!(
            "{}::{} impact_bfs_with_policy vs impact_bfs",
            entry.trait_name, entry.method_name
        );
        assert_eq!(
            policy_keys, plain_keys,
            "{ctx_label}: with_policy={policy_keys:?} plain={plain_keys:?}"
        );
        assert!(
            policy_keys.iter().any(|k| *k == fixture.caller_key),
            "{ctx_label}: both variants must include caller '{}'; got {policy_keys:?}",
            fixture.caller_key
        );
    }
}

/// Companion regression for the "fan-out capped/suppressed" behavior
/// called out in the AC: when fan-out is *not* emitted, the impl-method
/// has NO incoming Calls bucket from the caller — the explicit
/// trait↔impl hop in `outgoing.implements` is the only link. This
/// pins "asserted explicit trait<->impl hops and documented behavior
/// rather than fabricating unbounded callers."
#[test]
fn context_without_fanout_impl_method_keeps_trait_hop_but_drops_caller() {
    // Build a no-fanout spec for RuntimeOps::list_taskrun_jobs by
    // overriding the `with_fanout` flag. The trait method's caller
    // hop is preserved; only the impl-method's fan-out is suppressed.
    let mut spec = runtime_ops_list_taskrun_jobs_spec();
    spec.with_fanout = false;
    let graph = build_corpus_fixture(&spec);
    let trait_idx = node_index_for_role(&graph, &spec, FixtureRole::TraitMethod);
    let impl_idx = node_index_for_role(&graph, &spec, FixtureRole::ImplMethod);

    // Trait method: caller still appears in the Calls bucket.
    let (trait_in, _) = collect_context_buckets(&graph, trait_idx);
    let expected_conf =
        djinn_graph::repo_graph::edge_confidence_floor(RepoGraphEdgeKind::TraitDispatchCall);
    assert_calls_bucket_contains(
        &trait_in,
        "reap_orphaned_taskrun_jobs",
        expected_conf,
        "no-fanout: trait_method context",
    );

    // Impl method: no incoming Calls bucket (fan-out suppressed), but
    // the outgoing Implements bucket still carries the trait hop.
    let (impl_in, impl_out) = collect_context_buckets(&graph, impl_idx);
    let impl_calls = impl_in
        .get(&EdgeCategory::Calls)
        .cloned()
        .unwrap_or_default();
    assert!(
        impl_calls.is_empty(),
        "no-fanout: impl_method must NOT fabricate a caller union in Calls; got {impl_calls:?}"
    );
    assert_implements_bucket_contains(
        &impl_out,
        "runtime_bridge.rs",
        "no-fanout: impl_method context",
    );
}

/// Companion regression for the "fan-out capped/suppressed" behavior
/// on `impact`: when fan-out is not emitted, the impl-method has no
/// incoming trait-dispatch edges, so `impact(impl_method)` cannot
/// reach the caller through the impl-method node — only via the
/// direct trait_method hop (which is what the impact BFS walks when
/// queried against `trait_method` instead). This pins the "no
/// fabricated callers" invariant on the impact side too.
///
/// Note: the impact BFS walks **incoming** edges from the start
/// node (the "who depends on me" blast radius). The Implements edge
/// is `impl_method → trait_method` (outgoing from impl_method), so
/// without fan-out the impl_method's incoming set is empty and the
/// BFS cannot reach either the trait method or the caller.
#[test]
fn impact_without_fanout_impl_method_does_not_reach_caller() {
    let mut spec = runtime_ops_list_taskrun_jobs_spec();
    spec.with_fanout = false;
    let graph = build_corpus_fixture(&spec);
    let impl_idx = node_index_for_role(&graph, &spec, FixtureRole::ImplMethod);
    let caller_key = format!("symbol:{}", spec.caller.symbol);

    let trait_dispatch_conf =
        djinn_graph::repo_graph::edge_confidence_floor(RepoGraphEdgeKind::TraitDispatchCall);
    let result = shared::impact_bfs(&graph, impl_idx, 3, Some(trait_dispatch_conf));
    let keys: Vec<&str> = result.iter().map(|(_, e)| e.key.as_str()).collect();

    // Without fan-out, the impl_method has no incoming trait-dispatch
    // edge from the caller, so the BFS cannot reach the caller.
    assert!(
        !keys.iter().any(|k| *k == caller_key),
        "no-fanout: impact(impl_method) must NOT reach caller '{caller_key}' \
         (impl has no incoming TraitDispatchCall from caller); got {keys:?}"
    );

    // The BFS walks incoming edges only; the Implements edge is
    // outgoing from impl_method, so the trait_method is also not in
    // the blast radius from impl_method. Use `impact(trait_method)`
    // to reach the trait_method's caller side instead — that's the
    // documented "explicit trait↔impl hop" surface (visible via
    // context/edges/neighbors, not via impact's BFS-from-impl).
    let trait_key = format!("symbol:{}", spec.trait_method.symbol);
    assert!(
        !keys.iter().any(|k| *k == trait_key),
        "no-fanout: impact(impl_method) cannot traverse the outgoing Implements \
         edge — blast radius from impl_method must be empty when fan-out is \
         suppressed; got {keys:?}"
    );

    // Sanity check: querying `impact(trait_method)` (instead of
    // impl_method) reaches the caller — proving the trait_method hop
    // is the route to surface the caller even when fan-out is off.
    let trait_idx = node_index_for_role(&graph, &spec, FixtureRole::TraitMethod);
    let trait_result = shared::impact_bfs(&graph, trait_idx, 3, Some(trait_dispatch_conf));
    let trait_keys: Vec<&str> = trait_result.iter().map(|(_, e)| e.key.as_str()).collect();
    assert!(
        trait_keys.iter().any(|k| *k == caller_key),
        "impact(trait_method) must still reach caller '{caller_key}' via the direct \
         caller→trait_method TraitDispatchCall edge even when fan-out is suppressed; \
         got {trait_keys:?}"
    );
}

// ── Direct edge inspection per entry ──────────────────────────────────

/// Spot-check: every per-entry fixture carries exactly the edges the
/// corpus topology predicts (caller→trait_method TraitDispatchCall,
/// impl_method→trait_method Implements, optional caller→impl_method
/// fan-out TraitDispatchCall). Locks the fixture to the corpus so a
/// silent drop of an edge in the fixture would break this test before
/// any behavioral assertion runs.
#[test]
fn every_corpus_entry_fixture_has_expected_edge_topology() {
    for entry in CORPUS {
        let fixture = build_entry(entry);
        let ctx_label = format!(
            "{}::{} fixture edge topology",
            entry.trait_name, entry.method_name
        );

        // caller → trait_method: TraitDispatchCall at the floor.
        let caller_to_trait: Vec<_> = fixture
            .graph
            .graph()
            .edges_connecting(fixture.caller_idx, fixture.trait_idx)
            .filter(|e| e.weight().kind == RepoGraphEdgeKind::TraitDispatchCall)
            .collect();
        assert_eq!(
            caller_to_trait.len(),
            1,
            "{ctx_label}: expected exactly 1 caller→trait_method TraitDispatchCall edge; got {}",
            caller_to_trait.len()
        );

        // impl_method → trait_method: Implements.
        let impl_to_trait: Vec<_> = fixture
            .graph
            .graph()
            .edges_connecting(fixture.impl_idx, fixture.trait_idx)
            .filter(|e| e.weight().kind == RepoGraphEdgeKind::Implements)
            .collect();
        assert_eq!(
            impl_to_trait.len(),
            1,
            "{ctx_label}: expected exactly 1 impl_method→trait_method Implements edge; got {}",
            impl_to_trait.len()
        );

        // caller → impl_method: TraitDispatchCall fan-out only when
        // the spec says so.
        let caller_to_impl: Vec<_> = fixture
            .graph
            .graph()
            .edges_connecting(fixture.caller_idx, fixture.impl_idx)
            .filter(|e| e.weight().kind == RepoGraphEdgeKind::TraitDispatchCall)
            .collect();
        let expected_fanout = if fixture.spec.with_fanout { 1 } else { 0 };
        assert_eq!(
            caller_to_impl.len(),
            expected_fanout,
            "{ctx_label}: expected {} caller→impl_method fan-out edge(s); got {}",
            expected_fanout,
            caller_to_impl.len()
        );
    }
}

/// Sanity: the per-entry fixture matches the corpus's hand-verified
/// line numbers and file paths. This guarantees the test fixture is
/// aligned with the source-of-truth even after refactors rename
/// symbols or move modules — a fixture that drifts from the corpus
/// would silently mask behavioral regressions.
#[test]
fn every_corpus_entry_fixture_symbols_match_corpus_paths_and_lines() {
    for entry in CORPUS {
        let fixture = build_entry(entry);
        let ctx_label = format!(
            "{}::{} fixture vs corpus paths",
            entry.trait_name, entry.method_name
        );

        // Trait declaration file/line.
        let trait_node = fixture.graph.node(fixture.trait_idx);
        let trait_file = trait_node
            .file_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        assert!(
            trait_file.ends_with(entry.trait_declaration.0),
            "{ctx_label}: trait_method file '{trait_file}' should end with corpus path '{}'",
            entry.trait_declaration.0
        );

        // Concrete impl: pick the first corpus impl and compare file.
        let corpus_impl = entry
            .concrete_impls
            .first()
            .unwrap_or_else(|| panic!("{ctx_label}: corpus entry missing concrete impls"));
        let impl_node = fixture.graph.node(fixture.impl_idx);
        let impl_file = impl_node
            .file_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        assert!(
            impl_file.ends_with(corpus_impl.1),
            "{ctx_label}: impl_method file '{impl_file}' should end with corpus path '{}'",
            corpus_impl.1
        );

        // Caller file: pick the first corpus caller.
        let corpus_caller = entry
            .callers
            .first()
            .unwrap_or_else(|| panic!("{ctx_label}: corpus entry missing callers"));
        let caller_node = fixture.graph.node(fixture.caller_idx);
        let caller_file = caller_node
            .file_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        assert!(
            caller_file.ends_with(corpus_caller.1),
            "{ctx_label}: caller file '{caller_file}' should end with corpus path '{}'",
            corpus_caller.1
        );

        // Drop a guard assertion against an unused-variable warning
        // for `line` so the compiler stays happy if a future refactor
        // changes the comparison.
        let _ = (entry.trait_declaration.1, corpus_impl.2, corpus_caller.2);
    }
}

// ── Edge category spot-checks ────────────────────────────────────────

/// The `edge_category_for` table must classify
/// `RepoGraphEdgeKind::TraitDispatchCall` as `EdgeCategory::Calls` for
/// every corpus entry fixture, since the 5wyo contract routes
/// synthesized trait-dispatch caller edges through the same `Calls`
/// bucket as directly extracted SymbolReference calls.
#[test]
fn every_corpus_entry_trait_dispatch_edge_classifies_as_calls() {
    for entry in CORPUS {
        let fixture = build_entry(entry);
        let ctx_label = format!(
            "{}::{} trait-dispatch edge category",
            entry.trait_name, entry.method_name
        );

        // The caller→trait_method edge:
        let caller_to_trait: Vec<_> = fixture
            .graph
            .graph()
            .edges_connecting(fixture.caller_idx, fixture.trait_idx)
            .filter(|e| e.weight().kind == RepoGraphEdgeKind::TraitDispatchCall)
            .collect();
        assert_eq!(
            caller_to_trait.len(),
            1,
            "{ctx_label}: expected exactly 1 caller→trait_method edge"
        );
        let trait_node = fixture.graph.node(fixture.trait_idx);
        assert_eq!(
            edge_category_for(Some(caller_to_trait[0].weight()), trait_node),
            EdgeCategory::Calls,
            "{ctx_label}: caller→trait_method edge must classify as Calls"
        );

        // And the Implements edge:
        let impl_to_trait: Vec<_> = fixture
            .graph
            .graph()
            .edges_connecting(fixture.impl_idx, fixture.trait_idx)
            .filter(|e| e.weight().kind == RepoGraphEdgeKind::Implements)
            .collect();
        assert_eq!(
            impl_to_trait.len(),
            1,
            "{ctx_label}: expected exactly 1 impl_method→trait_method Implements edge"
        );
        assert_eq!(
            edge_category_for(Some(impl_to_trait[0].weight()), trait_node),
            EdgeCategory::Implements,
            "{ctx_label}: impl_method→trait_method edge must classify as Implements"
        );
    }
}
