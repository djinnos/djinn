//! Shared commit-safety path classification and filtering primitives.
//!
//! This module provides the reusable, pure path-filtering layer that both
//! checkpoint safety (in `djinn-agent-worker`) and the workspace commit path
//! use to decide which files should be excluded from staging/committing.
//!
//! # Design
//!
//! All functions are pure — they take a path string and a config, returning a
//! classification without touching the filesystem or running git. This makes
//! them trivially unit-testable and safe to call from any context.
//!
//! # Scope
//!
//! This module covers:
//! - Default generated/cache/build/editor exclusions
//! - Root-level worker scratch file rejection (`patch.txt`, `test.txt`, etc.)
//! - Fixture/testdata allowlist (so intentional files in fixture dirs are not
//!   rejected merely because their basename looks scratch-like)
//!
//! It does **not** cover:
//! - Content-based secret scanning (stays in `djinn-agent-worker/checkpoint_safety`)
//! - Large-binary size checks (stays in checkpoint safety config)
//! - Git-ignored/submodule/LFS detection (git-status-dependent, stays in
//!   checkpoint safety's async scan)
//!
//! # Relationship to checkpoint safety
//!
//! `djinn-agent-worker/src/checkpoint_safety` delegates its generated-path
//! decisions to the predicates exposed here, preventing semantic drift between
//! the checkpoint path and the WorkerDone auto-commit path.

// ─── Default exclusion lists ───────────────────────────────────────────────

/// Default excluded path patterns — generated caches, build outputs, logs,
/// coverage reports, dependency directories, and LFS/submodule payloads.
///
/// Each entry is matched as a prefix (directory, ending with `/`), a glob
/// suffix (starting with `*`), or a bare path segment. See
/// [`is_generated_path`] for the matching semantics.
pub const DEFAULT_EXCLUDED_PATH_PATTERNS: &[&str] = &[
    "target/",
    "node_modules/",
    ".next/",
    ".nuxt/",
    ".svelte-kit/",
    "dist/",
    "build/",
    ".build/",
    "out/",
    ".output/",
    ".turbo/",
    ".parcel-cache/",
    "coverage/",
    ".nyc_output/",
    "__pycache__/",
    ".pytest_cache/",
    ".mypy_cache/",
    ".ruff_cache/",
    ".tox/",
    ".venv/",
    "venv/",
    ".cache/",
    ".gradle/",
    ".mvn/",
    "vendor/",
    ".cargo/registry/",
    ".local/share/pnpm/",
    ".pnpm-store/",
    ".yarn/cache/",
    ".turbo/cache/",
    "logs/",
    "log/",
    "*.log",
    "*.tmp",
    "*.swp",
    "*.bak",
    "*.lock.json",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "Cargo.lock",
    "*.lcov",
    "*.profdata",
    "*.profraw",
    "*.gcno",
    "*.gcda",
    "*.o",
    "*.a",
    "*.so",
    "*.dylib",
    "*.dll",
    "*.exe",
    "*.wasm",
    "*.pdb",
    "*.class",
    "*.jar",
    "*.war",
    "*.pyc",
    "*.pyo",
    "*.DS_Store",
    "*.thumbs.db",
];

/// Default excluded file extensions (binaries and compiled artifacts).
pub const DEFAULT_EXCLUDED_EXTENSIONS: &[&str] = &[
    "pyc", "pyo", "class", "o", "a", "so", "dylib", "dll", "exe", "wasm", "pdb", "jar", "war",
    "log", "lcov", "profdata", "profraw", "gcno", "gcda", "tmp", "swp", "bak", "DS_Store",
];

/// Default directory components that cause entire subtrees to be excluded.
pub const DEFAULT_EXCLUDED_DIR_COMPONENTS: &[&str] = &[
    "target",
    "node_modules",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
    ".venv",
    "venv",
    "env",
    ".cache",
    ".gradle",
    ".mvn",
    "dist",
    "build",
    ".build",
    "out",
    ".output",
    ".turbo",
    ".parcel-cache",
    "coverage",
    ".nyc_output",
    ".next",
    ".nuxt",
    ".svelte-kit",
    "vendor",
    ".cargo",
    ".pnpm-store",
    ".yarn",
    "logs",
    "log",
    ".idea",
    ".vscode",
];

// ─── Root-level scratch file names ─────────────────────────────────────────

/// Root-level worker scratch file names that should be excluded from commits
/// when they appear at the repository root (depth 0).
///
/// These are files the worker produces as intermediate artifacts (e.g.
/// `patch.txt` from tool output, `test.txt` / `test2.txt` / `test3.txt` from
/// quick test runs) that should never be committed.
pub const ROOT_SCRATCH_NAMES: &[&str] = &["patch.txt", "test.txt", "test2.txt", "test3.txt"];

/// Root-level scratch filename prefixes. Files at the repo root whose name
/// starts with one of these prefixes (case-sensitive) are excluded.
///
/// Covers `patch.anything`, `patch`, etc.
pub const ROOT_SCRATCH_PREFIXES: &[&str] = &["patch"];

/// Common editor/cache droppings that should be excluded at the repo root.
///
/// These are temporary files editors and tools leave behind (swap files,
/// autosave files, etc.) that are distinct from the extension-based patterns
/// in [`DEFAULT_EXCLUDED_EXTENSIONS`] because they are specific to root-level
/// placement.
pub const ROOT_EDITOR_DROPPINGS: &[&str] = &[".DS_Store", "Thumbs.db", ".gitattributes.bak"];

// ─── Fixture/testdata allowlist ────────────────────────────────────────────

/// Directory components that mark a path as being inside a fixture/testdata
/// directory. Files under any of these directories are allowlisted — they are
/// not rejected as scratch/generated even if their basename would normally
/// match an exclusion pattern.
///
/// This covers `tests/fixtures/`, `testdata/`, `fixtures/`, and similar
/// project-specific fixture directories.
pub const FIXTURE_DIR_COMPONENTS: &[&str] = &[
    "fixtures",
    "testdata",
    "test_fixtures",
    "test-data",
    "test-data-fixtures",
];

// ─── Configuration ─────────────────────────────────────────────────────────

/// Configuration for path-level commit safety filtering.
///
/// Controls which path patterns are excluded from staging/committing. All
/// fields have sensible defaults via [`CommitSafetyConfig::default`]; callers
/// can override individual fields for project-specific policies.
#[derive(Debug, Clone)]
pub struct CommitSafetyConfig {
    /// Glob-style path patterns (matched against the repo-relative POSIX path)
    /// whose files are excluded from staging as generated/cache/build output.
    pub excluded_path_patterns: Vec<&'static str>,

    /// File extensions whose files are treated as generated/build output and
    /// excluded from staging.
    pub excluded_extensions: Vec<&'static str>,

    /// Directory names that, when they appear as a path component, cause the
    /// entire subtree to be excluded.
    pub excluded_dir_components: Vec<&'static str>,

    /// Root-level scratch file basenames to reject at depth 0.
    pub root_scratch_names: Vec<&'static str>,

    /// Root-level scratch filename prefixes to reject at depth 0.
    pub root_scratch_prefixes: Vec<&'static str>,

    /// Root-level editor/cache droppings to reject at depth 0.
    pub root_editor_droppings: Vec<&'static str>,

    /// Directory components that mark a path as a fixture/testdata allowlist.
    pub fixture_dir_components: Vec<&'static str>,
}

impl Default for CommitSafetyConfig {
    fn default() -> Self {
        Self {
            excluded_path_patterns: DEFAULT_EXCLUDED_PATH_PATTERNS.to_vec(),
            excluded_extensions: DEFAULT_EXCLUDED_EXTENSIONS.to_vec(),
            excluded_dir_components: DEFAULT_EXCLUDED_DIR_COMPONENTS.to_vec(),
            root_scratch_names: ROOT_SCRATCH_NAMES.to_vec(),
            root_scratch_prefixes: ROOT_SCRATCH_PREFIXES.to_vec(),
            root_editor_droppings: ROOT_EDITOR_DROPPINGS.to_vec(),
            fixture_dir_components: FIXTURE_DIR_COMPONENTS.to_vec(),
        }
    }
}

// ─── Classification result ─────────────────────────────────────────────────

/// The result of classifying a path for commit safety.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PathClassification {
    /// The path passes all filters and is eligible for staging/committing.
    Allowed,
    /// The path should be excluded from staging/committing.
    Excluded(PathExclusionReason),
}

/// Why a path was excluded from the commit safety filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PathExclusionReason {
    /// Matches a generated/cache/build output path pattern.
    GeneratedPath,
    /// Matches a generated/build output file extension.
    GeneratedExtension,
    /// Inside a generated/cache/build output directory component.
    GeneratedDir,
    /// A root-level worker scratch file (e.g. `patch.txt`, `test.txt`).
    RootScratch,
    /// A root-level editor/cache dropping (e.g. `.DS_Store`).
    RootEditorDrop,
}

// ─── Pure classification functions ─────────────────────────────────────────

/// Classify a repo-relative path against the commit safety config.
///
/// This is the primary entry point for path classification. It checks
/// generated-path patterns, generated directories, generated extensions,
/// root-level scratch files, and fixture/testdata allowlists — in that order.
///
/// # Parameters
/// - `path`: repo-relative POSIX path (forward slashes, no leading `./`).
/// - `config`: the commit safety policy.
///
/// # Fixture/testdata allowlist
///
/// If a path is inside a fixture/testdata directory (as determined by
/// [`is_in_fixture_dir`]), root-level scratch checks are skipped — the file
/// is allowed even if its basename would otherwise match a scratch pattern.
/// Generated-path/directory/extension checks still apply to fixture files
/// because those patterns (e.g. `target/`, `node_modules/`) are almost never
/// intentional inside fixtures.
pub fn classify_path(path: &str, config: &CommitSafetyConfig) -> PathClassification {
    // 1. Generated path patterns (highest priority — always applies).
    if is_generated_path(path, config) {
        return PathClassification::Excluded(PathExclusionReason::GeneratedPath);
    }

    // 2. Generated directory component.
    if has_generated_dir_component(path, config) {
        return PathClassification::Excluded(PathExclusionReason::GeneratedDir);
    }

    // 3. Generated file extension.
    if has_generated_extension(path, config) {
        return PathClassification::Excluded(PathExclusionReason::GeneratedExtension);
    }

    // 4. Root-level scratch/editor checks (only at depth 0, skipped for fixtures).
    if !is_in_fixture_dir(path, config) {
        if is_root_scratch(path, config) {
            return PathClassification::Excluded(PathExclusionReason::RootScratch);
        }
        if is_root_editor_dropping(path, config) {
            return PathClassification::Excluded(PathExclusionReason::RootEditorDrop);
        }
    }

    PathClassification::Allowed
}

/// Check whether a path matches any of the configured generated-path patterns.
///
/// A pattern matches if:
/// - It ends with `/` and the path starts with it (directory prefix), or
/// - It starts with `*` and the path ends with the pattern's suffix (glob), or
/// - The path *contains* the pattern as a path segment.
pub fn is_generated_path(path: &str, config: &CommitSafetyConfig) -> bool {
    for pat in &config.excluded_path_patterns {
        if pat.ends_with('/') {
            // Directory prefix: `target/` matches `target/foo` and `foo/target/bar`.
            let dir = pat.trim_end_matches('/');
            if path == dir
                || path.starts_with(&format!("{dir}/"))
                || path.contains(&format!("/{dir}/"))
            {
                return true;
            }
        } else if let Some(suffix) = pat.strip_prefix('*') {
            // Glob suffix: `*.log` matches `app.log` and `logs/app.log`.
            if path.ends_with(suffix) {
                return true;
            }
        } else {
            // Bare segment match.
            if path == *pat
                || path.contains(&format!("/{pat}/"))
                || path.starts_with(&format!("{pat}/"))
            {
                return true;
            }
        }
    }
    false
}

/// Check whether any path *component* is a generated directory name.
pub fn has_generated_dir_component(path: &str, config: &CommitSafetyConfig) -> bool {
    for component in path.split('/') {
        if config.excluded_dir_components.contains(&component) {
            return true;
        }
    }
    false
}

/// Check whether the file extension is in the generated-extensions list.
pub fn has_generated_extension(path: &str, config: &CommitSafetyConfig) -> bool {
    if let Some(ext) = path.rsplit('.').next()
        && path.contains('.')
        && ext.len() <= 10
        && ext != path
        && config.excluded_extensions.contains(&ext)
    {
        return true;
    }
    false
}

/// Check whether a path is a root-level worker scratch file.
///
/// A path is considered root-level scratch if:
/// - It has no `/` (i.e. it's at the repo root), AND
/// - Its basename matches an exact scratch name OR starts with a scratch prefix.
///
/// Paths inside subdirectories are never considered root-level scratch.
pub fn is_root_scratch(path: &str, config: &CommitSafetyConfig) -> bool {
    // Must be at the root (no directory component).
    if path.contains('/') {
        return false;
    }
    let basename = path;
    // Exact name match.
    if config.root_scratch_names.contains(&basename) {
        return true;
    }
    // Prefix match.
    for prefix in &config.root_scratch_prefixes {
        if basename.starts_with(prefix) && basename.len() >= prefix.len() {
            return true;
        }
    }
    false
}

/// Check whether a path is a root-level editor/cache dropping.
///
/// Similar to [`is_root_scratch`] but for editor artifacts like `.DS_Store`.
pub fn is_root_editor_dropping(path: &str, config: &CommitSafetyConfig) -> bool {
    if path.contains('/') {
        return false;
    }
    config.root_editor_droppings.contains(&path)
}

/// Check whether a path is inside a fixture/testdata directory.
///
/// Returns `true` if any component of the path (before the final filename)
/// matches one of the configured fixture directory components. This is used
/// as an allowlist: files in fixture directories are not rejected as scratch
/// even if their basename looks scratch-like.
///
/// Examples:
/// - `tests/fixtures/patch.txt` → `true` (contains `fixtures`)
/// - `testdata/test.txt` → `true` (contains `testdata`)
/// - `src/fixtures.rs` → `false` (the file IS `fixtures.rs`, not inside a
///   `fixtures/` directory — the final component is a filename, not a dir)
/// - `fixtures/` → `true` (the path itself is a fixture directory)
pub fn is_in_fixture_dir(path: &str, config: &CommitSafetyConfig) -> bool {
    let components: Vec<&str> = path.split('/').collect();
    // Check all components except the last one (which is the filename).
    // If the path ends with `/`, the last component is empty and all real
    // components are directory names.
    let dir_components = if path.ends_with('/') {
        &components[..]
    } else if components.len() > 1 {
        &components[..components.len() - 1]
    } else {
        // Single component (root-level file) — not in any fixture dir.
        return false;
    };
    for component in dir_components {
        if config.fixture_dir_components.contains(component) {
            return true;
        }
    }
    false
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: default config for tests.
    fn default_config() -> CommitSafetyConfig {
        CommitSafetyConfig::default()
    }

    // ── Generated path pattern tests ──────────────────────────────────

    #[test]
    fn is_generated_path_matches_target_prefix() {
        let config = default_config();
        assert!(is_generated_path("target/debug/foo", &config));
        assert!(is_generated_path("target/foo", &config));
    }

    #[test]
    fn is_generated_path_matches_nested_target() {
        let config = default_config();
        assert!(is_generated_path("workspace/target/debug/foo", &config));
    }

    #[test]
    fn is_generated_path_matches_log_glob() {
        let config = default_config();
        assert!(is_generated_path("app.log", &config));
        assert!(is_generated_path("logs/debug.log", &config));
    }

    #[test]
    fn is_generated_path_does_not_match_source() {
        let config = default_config();
        assert!(!is_generated_path("src/main.rs", &config));
        assert!(!is_generated_path("README.md", &config));
    }

    #[test]
    fn has_generated_dir_component_detects_nested() {
        let config = default_config();
        assert!(has_generated_dir_component("foo/node_modules/bar", &config));
        assert!(has_generated_dir_component("node_modules/bar", &config));
        assert!(!has_generated_dir_component("src/main.rs", &config));
    }

    #[test]
    fn has_generated_extension_handles_dotfiles() {
        let config = default_config();
        // `.gitignore` should not match the `ignore` extension (it's a dotfile).
        assert!(!has_generated_extension(".gitignore", &config));
        // But `.pyc` should match even without a directory.
        assert!(has_generated_extension("foo.pyc", &config));
    }

    // ── Root-level scratch rejection tests ─────────────────────────────

    #[test]
    fn root_scratch_exact_name_match() {
        let config = default_config();
        assert!(is_root_scratch("patch.txt", &config));
        assert!(is_root_scratch("test.txt", &config));
        assert!(is_root_scratch("test2.txt", &config));
        assert!(is_root_scratch("test3.txt", &config));
    }

    #[test]
    fn root_scratch_prefix_match() {
        let config = default_config();
        // "patch" prefix matches "patch.txt", "patch.json", "patch_anything"
        assert!(is_root_scratch("patch.txt", &config));
        assert!(is_root_scratch("patch.json", &config));
        assert!(is_root_scratch("patch_output", &config));
    }

    #[test]
    fn root_scratch_rejects_only_at_root() {
        let config = default_config();
        // Nested paths are NOT root-level scratch.
        assert!(!is_root_scratch("src/patch.txt", &config));
        assert!(!is_root_scratch("tests/test.txt", &config));
        assert!(!is_root_scratch("dir/patch.json", &config));
    }

    #[test]
    fn root_scratch_prefix_matches_bare_patch() {
        let config = default_config();
        // Bare "patch" at root is rejected: the "patch" prefix covers
        // "patch" exactly (via `>=`), "patch.txt", "patch.json", etc.
        assert!(is_root_scratch("patch", &config));
        assert!(is_root_scratch("patch.txt", &config));
        assert!(is_root_scratch("patch_output", &config));
    }

    #[test]
    fn root_scratch_does_not_match_source_files() {
        let config = default_config();
        assert!(!is_root_scratch("main.rs", &config));
        assert!(!is_root_scratch("README.md", &config));
        assert!(!is_root_scratch("Cargo.toml", &config));
    }

    #[test]
    fn root_editor_dropping_match() {
        let config = default_config();
        assert!(is_root_editor_dropping(".DS_Store", &config));
        assert!(is_root_editor_dropping("Thumbs.db", &config));
    }

    #[test]
    fn root_editor_dropping_only_at_root() {
        let config = default_config();
        assert!(!is_root_editor_dropping("dir/.DS_Store", &config));
    }

    // ── Fixture/testdata allowlist tests ───────────────────────────────

    #[test]
    fn fixture_dir_allows_scratch_basename() {
        let config = default_config();
        // Files inside a fixtures directory should be allowed even if their
        // basename looks scratch-like.
        assert!(is_in_fixture_dir("tests/fixtures/patch.txt", &config));
        assert!(is_in_fixture_dir("testdata/test.txt", &config));
        assert!(is_in_fixture_dir("fixtures/test2.txt", &config));
        assert!(is_in_fixture_dir("test_fixtures/data.json", &config));
    }

    #[test]
    fn fixture_dir_nested_path() {
        let config = default_config();
        assert!(is_in_fixture_dir(
            "server/tests/fixtures/sample.json",
            &config
        ));
        assert!(is_in_fixture_dir(
            "project/testdata/expected_output.txt",
            &config
        ));
    }

    #[test]
    fn fixture_dir_not_triggered_by_fixture_filename() {
        let config = default_config();
        // `fixtures.rs` is a file, not a directory entry — should not match.
        assert!(!is_in_fixture_dir("src/fixtures.rs", &config));
        // `testdata` as a filename (single component) — not in a fixture dir.
        assert!(!is_in_fixture_dir("testdata", &config));
    }

    #[test]
    fn fixture_dir_root_level_file_is_not_in_fixture() {
        let config = default_config();
        // A root-level file like `patch.txt` is not in any fixture directory.
        assert!(!is_in_fixture_dir("patch.txt", &config));
    }

    #[test]
    fn fixture_dir_empty_dir_component() {
        let config = default_config();
        // Path ending with `/` — all real components are checked as dirs.
        assert!(is_in_fixture_dir("tests/fixtures/", &config));
    }

    // ── Full classify_path tests ───────────────────────────────────────

    #[test]
    fn classify_generated_path_is_excluded() {
        let config = default_config();
        assert_eq!(
            classify_path("target/debug/foo", &config),
            PathClassification::Excluded(PathExclusionReason::GeneratedPath)
        );
    }

    #[test]
    fn classify_generated_dir_is_excluded() {
        let config = default_config();
        // `node_modules/` is in both path patterns and dir components;
        // the path pattern check runs first, so it's reported as GeneratedPath.
        assert_eq!(
            classify_path("node_modules/react/index.js", &config),
            PathClassification::Excluded(PathExclusionReason::GeneratedPath)
        );
    }

    #[test]
    fn classify_generated_extension_is_excluded() {
        let config = default_config();
        // `*.pyc` is in path patterns, so it matches as GeneratedPath first.
        assert_eq!(
            classify_path("app.pyc", &config),
            PathClassification::Excluded(PathExclusionReason::GeneratedPath)
        );
    }

    #[test]
    fn classify_root_scratch_is_excluded() {
        let config = default_config();
        assert_eq!(
            classify_path("patch.txt", &config),
            PathClassification::Excluded(PathExclusionReason::RootScratch)
        );
        assert_eq!(
            classify_path("test.txt", &config),
            PathClassification::Excluded(PathExclusionReason::RootScratch)
        );
    }

    #[test]
    fn classify_root_editor_dropping_is_excluded() {
        let config = default_config();
        // `.DS_Store` matches `*.DS_Store` in path patterns, so it's GeneratedPath.
        assert_eq!(
            classify_path(".DS_Store", &config),
            PathClassification::Excluded(PathExclusionReason::GeneratedPath)
        );
    }

    #[test]
    fn classify_source_file_is_allowed() {
        let config = default_config();
        assert_eq!(
            classify_path("src/main.rs", &config),
            PathClassification::Allowed
        );
    }

    #[test]
    fn classify_nested_scratch_is_allowed() {
        let config = default_config();
        // Nested scratch files are allowed (only root-level is rejected).
        assert_eq!(
            classify_path("src/patch.txt", &config),
            PathClassification::Allowed
        );
    }

    #[test]
    fn classify_fixture_scratch_is_allowed() {
        let config = default_config();
        // Files in fixture directories are allowlisted.
        assert_eq!(
            classify_path("tests/fixtures/patch.txt", &config),
            PathClassification::Allowed
        );
        assert_eq!(
            classify_path("testdata/test.txt", &config),
            PathClassification::Allowed
        );
    }

    #[test]
    fn classify_fixture_generated_is_still_excluded() {
        let config = default_config();
        // Even inside fixtures, generated patterns still apply.
        // `target/` is in path patterns, so it matches as GeneratedPath.
        assert_eq!(
            classify_path("tests/fixtures/target/debug/foo", &config),
            PathClassification::Excluded(PathExclusionReason::GeneratedPath)
        );
    }

    #[test]
    fn classify_source_with_scratch_like_extension_is_allowed() {
        let config = default_config();
        // A `.rs` file with "test" in the name is fine.
        assert_eq!(
            classify_path("src/test_helpers.rs", &config),
            PathClassification::Allowed
        );
    }

    #[test]
    fn classify_deeply_nested_source_is_allowed() {
        let config = default_config();
        assert_eq!(
            classify_path("crates/djinn-workspace/src/commit_safety.rs", &config),
            PathClassification::Allowed
        );
    }

    // ── Custom config tests ────────────────────────────────────────────

    #[test]
    fn custom_config_overrides_defaults() {
        let config = CommitSafetyConfig {
            excluded_path_patterns: vec!["mybuild/"],
            excluded_dir_components: vec!["mybuild"],
            ..Default::default()
        };
        assert!(is_generated_path("mybuild/output.txt", &config));
        assert!(!is_generated_path("target/foo", &config));
    }

    #[test]
    fn custom_config_adds_scratch_names() {
        let config = CommitSafetyConfig {
            root_scratch_names: vec!["patch.txt", "custom_scratch.txt"],
            ..Default::default()
        };
        assert!(is_root_scratch("custom_scratch.txt", &config));
    }
}
