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

use djinn_db::ProjectConfig;
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
        if self.globs.is_empty() {
            return false;
        }
        if let Some(f) = file {
            if self.globs.is_match(f) {
                return true;
            }
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
        match file {
            Some(f) if self.orphan_ignore.contains(f) => true,
            _ => false,
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
}
