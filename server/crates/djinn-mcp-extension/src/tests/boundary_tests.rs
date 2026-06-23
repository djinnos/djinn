//! Boundary enforcement tests for `djinn-mcp-extension`.
//!
//! These tests verify that the crate's `Cargo.toml` does not declare
//! forbidden dependencies.  Source-level import checks live in
//! `context.rs`'s `#[cfg(test)]` module (which properly excludes test
//! files); these tests complement them by checking the manifest boundary
//! itself — a harder constraint that catches both direct `use` and
//! transitive `extern crate` leaks.

use std::path::Path;
use std::sync::OnceLock;

/// Path to this crate's Cargo.toml.
fn manifest_path() -> &'static Path {
    static PATH: OnceLock<std::path::PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        Path::new(manifest_dir).join("Cargo.toml")
    })
    .as_path()
}

/// Read and parse this crate's Cargo.toml.
fn manifest_content() -> &'static str {
    static CONTENT: OnceLock<String> = OnceLock::new();
    CONTENT
        .get_or_init(|| std::fs::read_to_string(manifest_path()).expect("read Cargo.toml"))
        .as_str()
}

/// Forbidden dependency names in `[dependencies]` or `[dev-dependencies]`.
const FORBIDDEN_DEPS: &[&str] = &["djinn-agent", "sqlx"];

#[test]
fn cargo_toml_has_no_forbidden_dependencies() {
    let content = manifest_content();

    // Quick parse: look for `name =` lines in dependency sections.
    let mut in_deps_section = false;
    let mut violations = Vec::new();

    for (line_num, line) in content.lines().enumerate() {
        let trimmed = line.trim();

        // Track which TOML section we're in.
        if trimmed.starts_with('[') {
            in_deps_section = trimmed == "[dependencies]"
                || trimmed == "[dev-dependencies]"
                || trimmed == "[build-dependencies]";
            continue;
        }

        if !in_deps_section {
            continue;
        }

        // Check if this line declares a forbidden dependency.
        for &forbidden in FORBIDDEN_DEPS {
            // Match patterns like: `djinn-agent = { ... }` or `sqlx = "..."`
            if trimmed.starts_with(forbidden)
                && trimmed[forbidden.len()..].trim_start().starts_with('=')
            {
                violations.push(format!("  line {}: {}", line_num + 1, trimmed));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "djinn-mcp-extension Cargo.toml declares forbidden dependencies:\n{}\n\n\
         The crate must not depend on: {:?}",
        violations.join("\n"),
        FORBIDDEN_DEPS
    );
}

/// The crate's `[dependencies]` section must list the expected capability
/// crates and nothing else project-internal that violates the boundary.
#[test]
fn cargo_toml_dependency_list_is_stable() {
    let content = manifest_content();

    // Extract all dependency names (both [dependencies] and [dev-dependencies]).
    let mut in_section = false;
    let mut deps = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_section = trimmed == "[dependencies]" || trimmed == "[dev-dependencies]";
            continue;
        }
        if !in_section {
            continue;
        }
        // Match `name = ...` pattern.
        if let Some(eq_pos) = trimmed.find('=')
            && !trimmed.starts_with('#')
        {
            let name = trimmed[..eq_pos].trim();
            if !name.is_empty() && !name.contains(' ') {
                deps.push(name.to_string());
            }
        }
    }

    deps.sort();
    deps.dedup();

    // Snapshot the dependency list to catch accidental additions.
    insta::assert_json_snapshot!("dependency_list", deps);
}
