//! Test-runner classifier — mirror of `frameworks.rs` but for testing
//! frameworks. Extension set is small on purpose; new runners are a
//! one-line edit.

pub fn runner_for_dep(name: &str) -> Option<&'static str> {
    match name {
        // JS / TS
        "vitest" => Some("vitest"),
        "jest" | "@jest/core" => Some("jest"),
        "mocha" => Some("mocha"),
        "ava" => Some("ava"),
        "@playwright/test" => Some("playwright"),
        "cypress" => Some("cypress"),

        // Python
        "pytest" => Some("pytest"),
        "unittest" => Some("unittest"),

        // Ruby
        "rspec" | "rspec-rails" => Some("rspec"),
        "minitest" => Some("minitest"),

        _ => None,
    }
}

/// Rust has no dep-level signal for `cargo-nextest` — it's detected via
/// a top-level `.config/nextest.toml` file instead.
pub const NEXTEST_CONFIG_PATHS: &[&str] = &[".config/nextest.toml", "nextest.toml"];

pub fn canonicalize(mut slugs: Vec<&'static str>) -> Vec<String> {
    slugs.sort();
    slugs.dedup();
    slugs.into_iter().map(String::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_known_runners() {
        assert_eq!(runner_for_dep("vitest"), Some("vitest"));
        assert_eq!(runner_for_dep("pytest"), Some("pytest"));
        assert!(runner_for_dep("lodash").is_none());
    }
}
