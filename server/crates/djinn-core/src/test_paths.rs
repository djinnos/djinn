//! Canonical "is this path a test file" classification.
//!
//! This is the single source of truth shared by every layer that needs
//! to reason about whether a source file is a test:
//! - `djinn-graph` stamps [`is_test_path`] onto every File / Symbol node
//!   (`RepoGraphNode::is_test`) at graph-build time.
//! - `djinn-control-plane` re-exports it for the `code_graph tests=`
//!   filter and orphan cleanup.
//! - `djinn-agent` uses it for blast-radius (runtime / tests / e2e)
//!   categorisation.
//!
//! Keeping one implementation here means the `/code-graph` "hide tests"
//! toggle, the MCP filter, and the dispatch categoriser never drift on
//! what counts as a test.

/// True for test files by file-naming and directory convention.
///
/// Heuristics (file-path conventional, language-aware):
/// - Basename suffixes: `*_test.{go,rs,py,kt,scala}`,
///   `*.test.{ts,tsx,js,jsx,mjs}`, `*.spec.{ts,tsx,js,jsx}`, `*_spec.rb`,
///   `tests.rs` (Rust convention).
/// - Dir segments: `/tests/`, `/test/`, `/__tests__/` (anchored on `/`
///   so a file legitimately named `protests.rs` outside such a dir is
///   unaffected).
///
/// Conservative: when unsure, returns false so the file falls into the
/// non-test bucket.
pub fn is_test_path(path: &str) -> bool {
    let basename = path.rsplit('/').next().unwrap_or(path);
    if basename.ends_with("_test.go")
        || basename.ends_with("_test.rs")
        || basename.ends_with("_test.py")
        || basename.ends_with("_test.kt")
        || basename.ends_with("_test.scala")
        || basename.ends_with(".test.ts")
        || basename.ends_with(".test.tsx")
        || basename.ends_with(".test.js")
        || basename.ends_with(".test.jsx")
        || basename.ends_with(".test.mjs")
        || basename.ends_with("_spec.rb")
        || basename.ends_with(".spec.ts")
        || basename.ends_with(".spec.tsx")
        || basename.ends_with(".spec.js")
        || basename.ends_with(".spec.jsx")
        || basename == "tests.rs"
    {
        return true;
    }
    if path.contains("/tests/")
        || path.starts_with("tests/")
        || path.contains("/test/")
        || path.starts_with("test/")
        || path.contains("/__tests__/")
    {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::is_test_path;

    #[test]
    fn matches_language_test_suffixes() {
        assert!(is_test_path("internal/worker/page_worker_test.go"));
        assert!(is_test_path("crates/foo/src/lib_test.rs"));
        assert!(is_test_path("crates/foo/src/tests.rs"));
        assert!(is_test_path("backend/handlers_test.py"));
        assert!(is_test_path("src/main/kotlin/foo_test.kt"));
        assert!(is_test_path("ui/src/components/Button.test.tsx"));
        assert!(is_test_path("ui/src/utils/parser.test.ts"));
        assert!(is_test_path("ui/src/utils/parser.spec.tsx"));
        assert!(is_test_path("scripts/util.test.mjs"));
        assert!(is_test_path("app/models/user_spec.rb"));
    }

    #[test]
    fn matches_conventional_dirs() {
        assert!(is_test_path("tests/integration/foo.go"));
        assert!(is_test_path("crates/foo/tests/integration.rs"));
        assert!(is_test_path("test/unit/foo.py"));
        assert!(is_test_path("ui/src/__tests__/parser.ts"));
    }

    #[test]
    fn does_not_match_lookalikes() {
        // Substring `test` in a basename outside a test dir must not fire.
        assert!(!is_test_path("internal/handler/protests_handler.go"));
        assert!(!is_test_path("crates/contest/src/lib.rs"));
        assert!(!is_test_path("internal/util/testify_helper.go"));
        assert!(!is_test_path("internal/handler/handler.go"));
        assert!(!is_test_path("crates/foo/src/lib.rs"));
    }
}
