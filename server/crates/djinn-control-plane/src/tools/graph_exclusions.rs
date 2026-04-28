//! Exclusion filter for the `code_graph` MCP tool.
//!
//! Two tiers combined into a single predicate:
//!
//! * **Tier 1 — universal SCIP module-system artifacts.** Every Rust
//!   project indexed via rust-analyzer produces synthetic symbol nodes
//!   for the module tree (`crate/`, bare module paths ending `/`, and
//!   `.../MODULE.` markers). These are structurally guaranteed to
//!   appear in cycles with their containing file, pollute the `orphans`
//!   output with non-code "dead" entries, and inflate `ranked` hits
//!   with nodes that aren't real code locations. Filtering them is
//!   always correct, so they're hardcoded here and applied on every
//!   query regardless of project config.
//!
//! * **Tier 2 — per-project globs (+ orphan ignore list).** The
//!   [`ProjectConfig`] now carries `graph_excluded_paths` and
//!   `graph_orphan_ignore`, stored in Dolt via migration 12 and edited
//!   from the Pulse settings UI. Globs match against either the node's
//!   `file` (when present) or the node's SCIP `key` / display name —
//!   this mirrors the old client-side `isPathExcluded` semantics so
//!   patterns like `**/workspace-hack/**` continue to work without the
//!   user rewriting them. The exact-path `orphan_ignore` list is only
//!   consulted by the `orphans` op.
//!
//! All filtering happens at query time in the MCP handler, *after* the
//! graph cache has returned its precomputed results. That keeps the
//! canonical warmer cache valid across config edits — changing an
//! exclusion list is a zero-cost operation.

use djinn_core::events::EventBus;
use djinn_db::{Database, ProjectConfig, ProjectRepository};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::collections::HashSet;

/// Tier 1: returns true when `key` is a rust-analyzer SCIP node that
/// models a piece of the module tree rather than a real code symbol.
///
/// Two shapes match:
///
/// * The key ends in `/` — rust-analyzer spells every module (and the
///   crate root) with a trailing slash descriptor. Real symbols end in
///   `().` (function), `#` (type), `.` (const/field), or `!` (macro),
///   never `/`.
/// * The key ends in `/MODULE.` — SCIP's synthetic module-marker node,
///   one per module, co-referenced with the module's namespace.
///
/// File nodes are keyed `file:...rs` (or similar) and never trip these
/// suffixes, so we don't need a separate check for `kind`.
pub fn is_scip_module_artifact(key: &str) -> bool {
    key.ends_with('/') || key.ends_with("/MODULE.")
}

/// Tier 1.5: high-confidence noise patterns matched against the file
/// path of a node. Generated code, lockstep mocks, and snapshot
/// fixtures — files no human would call architecturally meaningful.
/// Always-on so first-time queries on a fresh project return clean
/// output without the user having to configure
/// `graph_excluded_paths`.
///
/// Test files are intentionally NOT in this set. Whether a test counts
/// as noise is project-specific (a user asking "who tests this?" wants
/// tests in the result), so they belong in per-project config.
///
/// Match is anchored on the FULL path (not the basename) so a file
/// legitimately named `mock.go` outside a `mock_*` / `*_mock.go` shape
/// is unaffected.
pub fn is_generated_or_mock_path(file: &str) -> bool {
    // Cheap basename extraction — last `/`-separated segment.
    let basename = file.rsplit('/').next().unwrap_or(file);

    // Generated suffixes — by convention these files are emitted by a
    // build step and editing them by hand is prohibited.
    if basename.ends_with(".pb.go")
        || basename.ends_with(".gen.go")
        || basename.ends_with(".gen.ts")
        || basename.ends_with(".gen.tsx")
        || basename.ends_with(".gen.rs")
        || basename.ends_with("_generated.go")
        || basename.ends_with("_generated.rs")
        || basename.ends_with(".g.dart")
        || basename.ends_with(".freezed.dart")
        || basename == "generated.ts"
        || basename == "schema.gen.ts"
    {
        return true;
    }

    // Mock conventions — both common Go forms and TS / Java patterns.
    if basename.ends_with("_mock.go")
        || basename.starts_with("mock_")
        || basename.ends_with("_mocks.go")
        || basename.ends_with(".mock.ts")
        || basename.ends_with(".mock.tsx")
    {
        return true;
    }

    // Snapshot fixtures — `cargo insta` and Jest both write `.snap`
    // files that capture stable JSON / structured output. They're
    // never source code; surfacing them as graph nodes is pure noise.
    if basename.ends_with(".snap") || basename.ends_with(".snap.new") {
        return true;
    }

    // Embedded path segments — covers the conventional generated /
    // mocks dirs whose contents the indexer otherwise picks up.
    if file.contains("/__mocks__/")
        || file.starts_with("__mocks__/")
        || file.contains("/__generated__/")
        || file.starts_with("__generated__/")
        || file.contains("/generated/")
        || file.starts_with("generated/")
    {
        return true;
    }

    false
}

/// Compiled predicate used by the `code_graph` MCP handler to filter
/// cycles / orphans / ranked results.
///
/// Built once per request from the project's `graph_excluded_paths`
/// (compiled to a `GlobSet`) and `graph_orphan_ignore` (kept as a hash
/// set of exact strings). Invalid globs are logged and skipped rather
/// than failing the whole query — one broken pattern shouldn't take
/// down the Pulse UI.
pub struct GraphExclusions {
    globs: GlobSet,
    orphan_ignore: HashSet<String>,
}

impl GraphExclusions {
    /// Compile a filter from raw project config. Bad globs are dropped
    /// with a `tracing::warn!` — the user will see them missing from
    /// the active set but their other patterns still apply.
    pub fn build(path_globs: &[String], orphan_ignore: &[String]) -> Self {
        let mut builder = GlobSetBuilder::new();
        for raw in path_globs {
            match Glob::new(raw) {
                Ok(g) => {
                    builder.add(g);
                }
                Err(e) => {
                    tracing::warn!(
                        pattern = %raw,
                        error = %e,
                        "skipping invalid graph_excluded_paths glob",
                    );
                }
            }
        }
        let globs = builder.build().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "graph_excluded_paths globset build failed; ignoring");
            GlobSet::empty()
        });
        Self {
            globs,
            orphan_ignore: orphan_ignore.iter().cloned().collect(),
        }
    }

    /// Convenience constructor mirroring the [`ProjectConfig`] shape so
    /// handlers can write `GraphExclusions::from_config(&config)`
    /// without digging into the two list fields.
    pub fn from_config(config: &ProjectConfig) -> Self {
        Self::build(&config.graph_excluded_paths, &config.graph_orphan_ignore)
    }

    /// Empty filter — Tier 1 is still applied via [`is_scip_module_artifact`],
    /// but no user-provided globs or orphan-ignore entries are considered.
    /// Used when the project has no row / no config, so we still strip
    /// the universal noise.
    pub fn empty() -> Self {
        Self {
            globs: GlobSet::empty(),
            orphan_ignore: HashSet::new(),
        }
    }

    /// Tier 1 + Tier 2-globs. Returns true when the node should be
    /// dropped from `cycles`, `ranked`, or `orphans` output. Matches
    /// against `file` first (when present), then against the SCIP key
    /// and display name to catch symbol nodes whose file is unset.
    pub fn excludes(&self, key: &str, file: Option<&str>, display_name: &str) -> bool {
        if is_scip_module_artifact(key) {
            return true;
        }
        // Tier 1.5: high-confidence noise (generated, mocks, snapshots).
        // Always-on, no per-project config needed for fresh projects.
        if let Some(f) = file
            && is_generated_or_mock_path(f)
        {
            return true;
        }
        if self.globs.is_empty() {
            return false;
        }
        if let Some(f) = file
            && self.globs.is_match(f)
        {
            return true;
        }
        self.globs.is_match(display_name) || self.globs.is_match(key)
    }

    /// Orphan-specific check: everything [`Self::excludes`] drops, plus
    /// any file path in the user's exact-match `graph_orphan_ignore`
    /// list. Intended for `code_graph orphans` only — the Dead-code
    /// panel uses this to mark files as "not actually dead" without
    /// writing a glob.
    pub fn excludes_orphan(&self, key: &str, file: Option<&str>, display_name: &str) -> bool {
        if self.excludes(key, file, display_name) {
            return true;
        }
        matches!(file, Some(f) if self.orphan_ignore.contains(f))
    }

    /// File-path-only predicate used by the coupling subsystem (and any
    /// caller whose inputs are bare repo-relative paths, no SCIP key /
    /// display name). Binds the path against `file`, `display_name`,
    /// and `key` so the same glob patterns that hide `workspace-hack/**`
    /// from the SCIP graph also filter the coupling index.
    pub fn excludes_path(&self, path: &str) -> bool {
        self.excludes(path, Some(path), path)
    }
}

/// Load the per-project [`GraphExclusions`] matcher from the
/// `project_graph_exclusions` config fields. On any lookup failure
/// (project row missing, DB blip) we fall back to
/// [`GraphExclusions::empty`] — Tier 1 still applies, which keeps
/// behaviour consistent with the SCIP graph's own
/// `DjinnMcpServer::load_graph_exclusions`.
///
/// Exposed as a free function so callers outside the `DjinnMcpServer`
/// struct (the agent workspace handlers live in a different crate) can
/// reuse exactly the same matcher semantics used by the `code_graph`
/// MCP ops.
pub async fn load_project_exclusion_matcher(
    db: &Database,
    event_bus: &EventBus,
    project_id: &str,
) -> GraphExclusions {
    let repo = ProjectRepository::new(db.clone(), event_bus.clone());
    match repo.get_config(project_id).await {
        Ok(Some(config)) => GraphExclusions::from_config(&config),
        Ok(None) => GraphExclusions::empty(),
        Err(e) => {
            tracing::debug!(
                project_id = %project_id,
                error = %e,
                "load_project_exclusion_matcher: config read failed; using Tier 1 only",
            );
            GraphExclusions::empty()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier1_catches_module_artifacts() {
        // Crate-root self-ref
        assert!(is_scip_module_artifact(
            "symbol:rust-analyzer cargo workspace-hack 0.1.0 crate/"
        ));
        // Bare module path
        assert!(is_scip_module_artifact(
            "symbol:rust-analyzer cargo alt-payments 0.1.0 batch/"
        ));
        // Nested module
        assert!(is_scip_module_artifact(
            "symbol:rust-analyzer cargo alt-identity 0.1.0 proto/svc_partner/"
        ));
        // SCIP module marker
        assert!(is_scip_module_artifact(
            "symbol:rust-analyzer cargo alt-payments 0.1.0 batch/MODULE."
        ));
    }

    #[test]
    fn tier1_preserves_real_symbols_and_files() {
        // Function
        assert!(!is_scip_module_artifact(
            "symbol:rust-analyzer cargo alt-identity 0.1.0 customers/users/services/create().",
        ));
        // Type
        assert!(!is_scip_module_artifact(
            "symbol:rust-analyzer cargo alt-identity 0.1.0 customers/users/models/UserModel#",
        ));
        // Const (including the CRATE_NAME/DOMAIN top-level constants — we
        // deliberately do NOT match these; once `crate/` is filtered, the
        // spurious 2/3-member SCC they participated in falls below min_size.)
        assert!(!is_scip_module_artifact(
            "symbol:rust-analyzer cargo alt-errors 0.1.0 CRATE_NAME.",
        ));
        assert!(!is_scip_module_artifact(
            "symbol:rust-analyzer cargo alt-processors 0.1.0 DOMAIN.",
        ));
        // Macro
        assert!(!is_scip_module_artifact(
            "symbol:rust-analyzer cargo test-support 0.1.0 db_test!",
        ));
        // File node
        assert!(!is_scip_module_artifact("file:crates/foo/src/lib.rs"));
    }

    #[test]
    fn tier2_globs_match_file_paths() {
        let ex = GraphExclusions::build(
            &["**/workspace-hack/**".into(), "**/test-support/**".into()],
            &[],
        );
        assert!(ex.excludes(
            "symbol:rust-analyzer cargo test-support 0.1.0 polling/with_timeout().",
            Some("crates/test-support/src/polling.rs"),
            "with_timeout",
        ));
        assert!(ex.excludes(
            "file:workspace-hack/src/lib.rs",
            Some("workspace-hack/src/lib.rs"),
            "workspace-hack/src/lib.rs",
        ));
        assert!(!ex.excludes(
            "file:crates/foo/src/lib.rs",
            Some("crates/foo/src/lib.rs"),
            "crates/foo/src/lib.rs",
        ));
    }

    #[test]
    fn tier2_globs_also_match_display_name_and_key_for_symbolic_nodes() {
        // Some symbol nodes have no file_path (external / synthetic).
        // Make sure the filter still engages via the key or display
        // name — matches the behaviour of the old UI-side
        // `isPathExcluded(m.display_name || m.key, ...)` in
        // CyclesPanel.tsx.
        let ex = GraphExclusions::build(&["**/workspace-hack/**".into()], &[]);
        assert!(ex.excludes(
            "symbol:rust-analyzer cargo workspace-hack 0.1.0 crate/main().",
            None,
            "workspace-hack/main",
        ));
    }

    #[test]
    fn orphan_ignore_matches_exact_file_only() {
        let ex = GraphExclusions::build(
            &[],
            &["crates/test-support/src/fixtures.rs".into()],
        );
        // Exact match → ignored for orphans
        assert!(ex.excludes_orphan(
            "symbol:...",
            Some("crates/test-support/src/fixtures.rs"),
            "default",
        ));
        // Substring does NOT match — orphan_ignore is exact-only.
        assert!(!ex.excludes_orphan(
            "symbol:...",
            Some("crates/test-support/src/fixtures/more.rs"),
            "default",
        ));
        // Non-orphan `excludes` isn't affected by orphan_ignore.
        assert!(!ex.excludes(
            "symbol:...",
            Some("crates/test-support/src/fixtures.rs"),
            "default",
        ));
    }

    #[test]
    fn invalid_glob_is_dropped_not_fatal() {
        // `**/[unclosed` isn't a valid glob. The filter should still
        // build (with the bad pattern silently dropped) and the good
        // pattern should still work.
        let ex = GraphExclusions::build(
            &["**/[unclosed".into(), "**/workspace-hack/**".into()],
            &[],
        );
        assert!(ex.excludes(
            "file:workspace-hack/src/lib.rs",
            Some("workspace-hack/src/lib.rs"),
            "workspace-hack",
        ));
    }

    #[test]
    fn empty_filter_still_applies_tier1() {
        let ex = GraphExclusions::empty();
        assert!(ex.excludes(
            "symbol:rust-analyzer cargo foo 0.1.0 crate/",
            None,
            "crate",
        ));
        assert!(!ex.excludes(
            "file:crates/foo/src/lib.rs",
            Some("crates/foo/src/lib.rs"),
            "lib.rs",
        ));
    }

    /// v8 Tier 1.5: high-confidence noise patterns matched against the
    /// file path. Always-on (no project config required) for
    /// generated, mocks, and snapshot fixtures.
    #[test]
    fn tier_1_5_drops_generated_and_mock_files() {
        // Generated.
        assert!(is_generated_or_mock_path("internal/proto/billing.pb.go"));
        assert!(is_generated_or_mock_path("api/handler.gen.go"));
        assert!(is_generated_or_mock_path("ui/src/api/client.gen.ts"));
        assert!(is_generated_or_mock_path("crates/foo/src/proto.gen.rs"));
        assert!(is_generated_or_mock_path("internal/types/types_generated.go"));
        assert!(is_generated_or_mock_path("lib/models/user.g.dart"));
        assert!(is_generated_or_mock_path("lib/models/user.freezed.dart"));
        assert!(is_generated_or_mock_path("ui/generated/index.ts"));
        assert!(is_generated_or_mock_path("ui/src/__generated__/queries.ts"));
        assert!(is_generated_or_mock_path("__generated__/api.ts"));
        // Mocks.
        assert!(is_generated_or_mock_path("internal/strategies/mocks/strategy_mock.go"));
        assert!(is_generated_or_mock_path("internal/strategies/mocks/mock_strategy.go"));
        assert!(is_generated_or_mock_path("internal/repository/sync_mocks.go"));
        assert!(is_generated_or_mock_path("ui/src/services/auth.mock.ts"));
        assert!(is_generated_or_mock_path("ui/src/__mocks__/api.ts"));
        // Snapshot fixtures.
        assert!(is_generated_or_mock_path(
            "server/crates/foo/src/snapshots/foo__tests__bar.snap"
        ));
        assert!(is_generated_or_mock_path(
            "server/crates/foo/src/snapshots/foo__tests__bar.snap.new"
        ));
    }

    /// v8 Tier 1.5: source files with "mock" or "generated" in their
    /// name as a normal word are NOT dropped. Match is intentionally
    /// strict on convention — a file legitimately named `mockable.go`
    /// or `generator.go` should pass.
    #[test]
    fn tier_1_5_preserves_legitimate_source_files() {
        // Words in the name but not the convention.
        assert!(!is_generated_or_mock_path("internal/util/mockable.go"));
        assert!(!is_generated_or_mock_path("internal/util/generator.go"));
        assert!(!is_generated_or_mock_path("internal/util/generation_status.go"));
        // `mock` as standalone basename without the `_mock`/`mock_` prefix.
        assert!(!is_generated_or_mock_path("internal/test_helpers/mock.go"));
        // `.proto` source files (the .pb.go is generated FROM these).
        assert!(!is_generated_or_mock_path("api/billing.proto"));
        // Tests are NOT in Tier 1.5 — that's a per-project decision.
        assert!(!is_generated_or_mock_path("internal/worker/page_worker_test.go"));
        assert!(!is_generated_or_mock_path("crates/foo/src/tests.rs"));
    }

    /// v8 Tier 1.5: integration with the public `excludes` predicate.
    /// Tier 1.5 fires WITHOUT the user configuring any globs.
    #[test]
    fn tier_1_5_excludes_without_project_config() {
        let ex = GraphExclusions::empty();
        assert!(ex.excludes(
            "file:internal/proto/billing.pb.go",
            Some("internal/proto/billing.pb.go"),
            "billing.pb.go",
        ));
        assert!(ex.excludes(
            "symbol:scip-go . pkg internal/strategies/mocks/strategy_mock.go MockStrategy#",
            Some("internal/strategies/mocks/strategy_mock.go"),
            "MockStrategy",
        ));
    }
}
