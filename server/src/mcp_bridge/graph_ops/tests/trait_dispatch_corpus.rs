//! Hand-verified trait-dispatch reproduction corpus.
//!
//! This module is the canonical corpus for end-to-end validation tests
//! in epic h1hn ("Validate trait-dispatch graph behavior end-to-end in
//! djinnos/djinn").  Each [`CorpusEntry`] records a Rust trait method
//! that has at least one in-repo concrete implementation **and** at
//! least one in-repo call site where the caller goes through the
//! trait object (dynamic dispatch via `dyn Trait` or generic `impl
//! Trait`).
//!
//! The corpus is **not** a graph fixture — it documents the source
//! truth that graph tests can assert against.  The companion test
//! fixtures in [`super::trait_dispatch_query`] and
//! [`super::trait_dispatch_impact`] exercise synthetic graphs that
//! model the same edge topology.
//!
//! # Verification methodology
//!
//! Each entry was hand-verified using a combination of:
//!
//! 1. **`grep`/`code_search`** — identify the trait declaration,
//!    every `impl Trait for Concrete` block, and every call site
//!    where the trait method is invoked on a trait-object or
//!    generic-typed receiver.
//! 2. **Source reading** — confirm the call goes through the trait
//!    (not a concrete inherent method) by verifying the receiver
//!    type is either `dyn Trait`, `Arc<dyn Trait>`, or a generic
//!    `T: Trait`/`&impl Trait`.
//! 3. **`grep -n`** — record exact file paths and 1-based line
//!    numbers so downstream tests can assert symbol-file
//!    correspondence without re-scanning.

/// A single hand-verified trait-dispatch reproduction entry.
#[derive(Debug, Clone)]
pub(super) struct CorpusEntry {
    /// Human-readable trait name (e.g. `"RuntimeOps"`).
    pub(super) trait_name: &'static str,
    /// Method name on the trait (e.g. `"list_taskrun_jobs"`).
    pub(super) method_name: &'static str,

    /// Trait declaration: `(file_path, line_number)`.
    pub(super) trait_declaration: (&'static str, u32),

    /// Concrete `impl Trait for ...` sites: each entry is
    /// `(impl_type_name, file_path, line_number)`.
    pub(super) concrete_impls: &'static [(&'static str, &'static str, u32)],

    /// In-repo caller sites where the method is invoked through the
    /// trait: each entry is `(caller_function_or_method, file_path, line_number)`.
    ///
    /// Only non-test production callers are listed by default; test
    /// callers are documented separately in [`test_callers`] to keep
    /// the assertion set stable across test-only refactors.
    pub(super) callers: &'static [(&'static str, &'static str, u32)],

    /// Test/mock callers (informational; not asserted by default).
    pub(super) test_callers: &'static [(&'static str, &'static str, u32)],
}

// ── Entry 1: RuntimeOps::list_taskrun_jobs (mandatory) ────────────────────────

/// `RuntimeOps::list_taskrun_jobs` — the mandatory corpus entry.
///
/// **Trait declaration:** `server/crates/djinn-control-plane/src/bridge/runtime_bridge.rs:137`
///
/// **Concrete impls:**
/// - `AppState` (production bridge) in `server/src/mcp_bridge/mod.rs:78`
/// - `StubRuntime` (test stub) in `server/crates/djinn-control-plane/src/test_support.rs:168`
/// - `StubRuntimeOps` (state.rs tests) in `server/crates/djinn-control-plane/src/state.rs:437`
///
/// **Production callers:**
/// - `reap_orphaned_taskrun_jobs` in
///   `server/crates/djinn-agent/src/actors/coordinator/health.rs:1490`
///   — invoked as `runtime_ops.list_taskrun_jobs().await` on an
///   `Option<Arc<dyn RuntimeOps>>`.
///
/// **Verification:** `code_search("list_taskrun_jobs", path="server/")`
/// followed by source reading of each hit to confirm the call goes
/// through the `RuntimeOps` trait (not an inherent method).
pub(super) const RUNTIME_OPS_LIST_TASKRUN_JOBS: CorpusEntry = CorpusEntry {
    trait_name: "RuntimeOps",
    method_name: "list_taskrun_jobs",
    trait_declaration: (
        "server/crates/djinn-control-plane/src/bridge/runtime_bridge.rs",
        137,
    ),
    concrete_impls: &[
        ("AppState", "server/src/mcp_bridge/mod.rs", 78),
        (
            "StubRuntime",
            "server/crates/djinn-control-plane/src/test_support.rs",
            168,
        ),
        (
            "StubRuntimeOps",
            "server/crates/djinn-control-plane/src/state.rs",
            437,
        ),
    ],
    callers: &[(
        "reap_orphaned_taskrun_jobs",
        "server/crates/djinn-agent/src/actors/coordinator/health.rs",
        1490,
    )],
    test_callers: &[
        (
            "SemanticRuntimeOps::list_taskrun_jobs",
            "server/crates/djinn-control-plane/src/tools/memory_tools/ops_tests.rs",
            65,
        ),
        (
            "FailingSemanticRuntimeOps::list_taskrun_jobs",
            "server/crates/djinn-control-plane/src/tools/memory_tools/ops_tests.rs",
            105,
        ),
        (
            "RecordingRuntimeOps::list_taskrun_jobs",
            "server/crates/djinn-control-plane/tests/execution_tools.rs",
            896,
        ),
    ],
};

// ── Entry 2: RepoGraphOps::context ───────────────────────────────────────────

/// `RepoGraphOps::context` — the 360° symbol view.
///
/// **Trait declaration:**
/// `server/crates/djinn-control-plane/src/bridge/graph_bridge.rs:298`
///
/// **Concrete impls:**
/// - `RepoGraphBridge` (production bridge) in
///   `server/src/mcp_bridge/graph_ops/mod.rs:240`
///
/// **Production callers:**
/// - `GraphToolHandler::code_graph_context` in
///   `server/crates/djinn-control-plane/src/tools/graph_tools/handler_basic_ops.rs:720`
///   — invoked as `self.state.repo_graph().context(ctx, key, include_content)`
///   where `repo_graph()` returns `&dyn RepoGraphOps`.
///
/// **Verification:** `code_search("context(", path="server/src/mcp_bridge/graph_ops/")`
/// plus `code_search("code_graph_context", path="server/")` to locate the tool
/// handler dispatch. Source reading confirmed `self.state.repo_graph()` returns
/// a `dyn RepoGraphOps` reference.
pub(super) const REPO_GRAPH_OPS_CONTEXT: CorpusEntry = CorpusEntry {
    trait_name: "RepoGraphOps",
    method_name: "context",
    trait_declaration: (
        "server/crates/djinn-control-plane/src/bridge/graph_bridge.rs",
        298,
    ),
    concrete_impls: &[(
        "RepoGraphBridge",
        "server/src/mcp_bridge/graph_ops/mod.rs",
        240,
    )],
    callers: &[(
        "GraphToolHandler::code_graph_context",
        "server/crates/djinn-control-plane/src/tools/graph_tools/handler_basic_ops.rs",
        720,
    )],
    test_callers: &[],
};

// ── Entry 3: SlotPoolOps::get_status ─────────────────────────────────────────

/// `SlotPoolOps::get_status` — slot-pool capacity inquiry.
///
/// **Trait declaration:**
/// `server/crates/djinn-control-plane/src/bridge/slot_pool_bridge.rs:36`
///
/// **Concrete impls:**
/// - `SlotPoolBridge` (production bridge) in
///   `server/src/mcp_bridge/bridges.rs:42`
/// - `StubSlotPool` (test stub) in
///   `server/crates/djinn-control-plane/src/test_support.rs:83`
///
/// **Production callers:**
/// - `CoordinatorActor::reconcile_inflight_dispatch_ledger` in
///   `server/crates/djinn-agent/src/actors/coordinator/dispatch/task_dispatch.rs:243`
///   — invoked as `self.pool.get_status().await` where `pool` is a
///   `SlotPoolHandle` implementing `SlotPoolOps`.
/// - `CoordinatorActor::maybe_consolidate_idle_slots` in
///   `server/crates/djinn-agent/src/actors/coordinator/actor.rs:1576`
///   — invoked as `self.pool.get_status().await`.
///
/// **Verification:** `code_search("pool.get_status", path="server/")`
/// followed by source reading to confirm `pool` is typed as
/// `SlotPoolHandle` (which wraps `SlotPoolOps`).
pub(super) const SLOT_POOL_OPS_GET_STATUS: CorpusEntry = CorpusEntry {
    trait_name: "SlotPoolOps",
    method_name: "get_status",
    trait_declaration: (
        "server/crates/djinn-control-plane/src/bridge/slot_pool_bridge.rs",
        36,
    ),
    concrete_impls: &[
        ("SlotPoolBridge", "server/src/mcp_bridge/bridges.rs", 42),
        (
            "StubSlotPool",
            "server/crates/djinn-control-plane/src/test_support.rs",
            83,
        ),
    ],
    callers: &[
        (
            "CoordinatorActor::reconcile_inflight_dispatch_ledger",
            "server/crates/djinn-agent/src/actors/coordinator/dispatch/task_dispatch.rs",
            243,
        ),
        (
            "CoordinatorActor::maybe_consolidate_idle_slots",
            "server/crates/djinn-agent/src/actors/coordinator/actor.rs",
            1576,
        ),
    ],
    test_callers: &[],
};

// ── Entry 4: RepoGraphOps::impact ────────────────────────────────────────────

/// `RepoGraphOps::impact` — transitive impact / blast-radius BFS.
///
/// **Trait declaration:**
/// `server/crates/djinn-control-plane/src/bridge/graph_bridge.rs:101`
///
/// **Concrete impls:**
/// - `RepoGraphBridge` (production bridge) in
///   `server/src/mcp_bridge/graph_ops/mod.rs:118`
///
/// **Production callers:**
/// - `GraphToolHandler::code_graph_impact` in
///   `server/crates/djinn-control-plane/src/tools/graph_tools/handler_basic_ops.rs:204`
///   — invoked as `self.state.repo_graph().impact(ctx, ...)`.
///
/// **Verification:** `code_search("code_graph_impact", path="server/")`
/// plus source reading of the handler body. The trait method signature
/// in `graph_bridge.rs:101` takes `workspace`, `key`, `depth`,
/// `group_by`, `min_confidence` — confirmed matching the call site.
pub(super) const REPO_GRAPH_OPS_IMPACT: CorpusEntry = CorpusEntry {
    trait_name: "RepoGraphOps",
    method_name: "impact",
    trait_declaration: (
        "server/crates/djinn-control-plane/src/bridge/graph_bridge.rs",
        101,
    ),
    concrete_impls: &[(
        "RepoGraphBridge",
        "server/src/mcp_bridge/graph_ops/mod.rs",
        118,
    )],
    callers: &[(
        "GraphToolHandler::code_graph_impact",
        "server/crates/djinn-control-plane/src/tools/graph_tools/handler_basic_ops.rs",
        204,
    )],
    test_callers: &[],
};

// ── Full corpus ──────────────────────────────────────────────────────────────

/// All hand-verified corpus entries, in canonical order.
///
/// Backend graph tests should iterate this slice to assert that
/// `code_graph context` and `code_graph impact` produce the expected
/// Dependents/Dependencies for each entry's trait declaration,
/// concrete impl methods, and caller symbols.
pub(super) const CORPUS: &[&CorpusEntry] = &[
    &RUNTIME_OPS_LIST_TASKRUN_JOBS,
    &REPO_GRAPH_OPS_CONTEXT,
    &SLOT_POOL_OPS_GET_STATUS,
    &REPO_GRAPH_OPS_IMPACT,
];

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// AC1: The corpus contains at least three entries including the
    /// mandatory `RuntimeOps::list_taskrun_jobs`.
    #[test]
    fn corpus_has_at_least_three_entries() {
        assert!(
            CORPUS.len() >= 3,
            "corpus must have at least 3 entries, got {}",
            CORPUS.len()
        );
    }

    /// AC1: `RuntimeOps::list_taskrun_jobs` is present in the corpus.
    #[test]
    fn corpus_includes_runtime_ops_list_taskrun_jobs() {
        let found = CORPUS
            .iter()
            .any(|e| e.trait_name == "RuntimeOps" && e.method_name == "list_taskrun_jobs");
        assert!(found, "corpus must include RuntimeOps::list_taskrun_jobs");
    }

    /// AC1: Every corpus entry has at least one concrete impl and one caller.
    #[test]
    fn every_entry_has_impls_and_callers() {
        for entry in CORPUS {
            assert!(
                !entry.concrete_impls.is_empty(),
                "{}::{} must have at least one concrete impl",
                entry.trait_name,
                entry.method_name,
            );
            assert!(
                !entry.callers.is_empty(),
                "{}::{} must have at least one production caller",
                entry.trait_name,
                entry.method_name,
            );
        }
    }

    /// AC2: Every corpus entry has a trait declaration with a
    /// non-empty file path and non-zero line number.
    #[test]
    fn every_entry_has_trait_declaration_info() {
        for entry in CORPUS {
            assert!(
                !entry.trait_declaration.0.is_empty(),
                "{}::{} trait declaration must have a file path",
                entry.trait_name,
                entry.method_name,
            );
            assert!(
                entry.trait_declaration.1 > 0,
                "{}::{} trait declaration must have a line number > 0",
                entry.trait_name,
                entry.method_name,
            );
        }
    }

    /// AC3: The corpus is documented with enough file/symbol info for
    /// graph tests to assert against without production graph data.
    /// Verify that each concrete impl has a non-empty file path and
    /// each caller has a non-empty file path.
    #[test]
    fn file_paths_are_nonempty_for_all_impls_and_callers() {
        for entry in CORPUS {
            for (name, path, line) in entry.concrete_impls {
                assert!(
                    !path.is_empty(),
                    "{}::{} impl '{}' must have a file path",
                    entry.trait_name,
                    entry.method_name,
                    name,
                );
                assert!(
                    *line > 0,
                    "{}::{} impl '{}' must have line > 0",
                    entry.trait_name,
                    entry.method_name,
                    name,
                );
            }
            for (name, path, line) in entry.callers {
                assert!(
                    !path.is_empty(),
                    "{}::{} caller '{}' must have a file path",
                    entry.trait_name,
                    entry.method_name,
                    name,
                );
                assert!(
                    *line > 0,
                    "{}::{} caller '{}' must have line > 0",
                    entry.trait_name,
                    entry.method_name,
                    name,
                );
            }
        }
    }
}
