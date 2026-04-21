//! Generate a starter `devcontainer.json` from a detected `Stack`.
//!
//! Phase 3.5 mapping (see `/home/fernando/.claude/plans/phase3.5-devcontainer-features.md`):
//! the generator emits *only* `djinnos/djinn-agent-worker` + per-language
//! `djinnos/djinn-<lang>` Features. No upstream `devcontainers/features/*` or
//! `devcontainers-contrib/features/*` references remain — Djinn now bundles its
//! own toolchain Features that version-sync with the worker binary and ship the
//! matching SCIP indexer.
//!
//! Keep the function signature stable (`pub fn generate_starter(stack: &Stack) -> String`);
//! the UI banner, `board_health` MCP tool, and tests all depend on it.

use crate::schema::Stack;

/// Emit a pretty-printed `devcontainer.json` string (2-space indent, no
/// trailing newline) suitable for dropping into
/// `.devcontainer/devcontainer.json`. Output is intentionally minimal — no
/// `postCreateCommand`; the user owns post-create logic.
pub fn generate_starter(stack: &Stack) -> String {
    let mut features = serde_json::Map::new();

    // Worker Feature: always first, always required.
    features.insert(
        "ghcr.io/djinnos/djinn-agent-worker:1".into(),
        serde_json::json!({}),
    );

    // Language Features: add one per detected language family. The mapping
    // follows §"Starter-JSON generator update" in the Phase 3.5 plan.
    let langs: Vec<&str> = stack.languages.iter().map(|l| l.name.as_str()).collect();

    if langs.contains(&"Rust") {
        features.insert(
            "ghcr.io/djinnos/djinn-rust:1".into(),
            serde_json::json!({
                "toolchain": stack.runtimes.rust.clone().unwrap_or_else(|| "stable".into()),
            }),
        );
    }

    if langs
        .iter()
        .any(|n| matches!(*n, "TypeScript" | "JavaScript" | "TSX" | "JSX"))
    {
        let node_version = stack.runtimes.node.clone().unwrap_or_else(|| "24".into());
        let pm = first_slug(&stack.package_managers, &["pnpm", "yarn", "bun", "deno", "npm"])
            .unwrap_or_else(|| "pnpm".into());
        features.insert(
            "ghcr.io/djinnos/djinn-typescript:1".into(),
            serde_json::json!({
                "node_version": node_version,
                "pm": pm,
            }),
        );
    }

    if langs.contains(&"Python") {
        let py_version = stack.runtimes.python.clone().unwrap_or_else(|| "3.14".into());
        let pm = first_slug(&stack.package_managers, &["uv", "poetry", "pdm", "pip"])
            .unwrap_or_else(|| "uv".into());
        features.insert(
            "ghcr.io/djinnos/djinn-python:1".into(),
            serde_json::json!({
                "version": py_version,
                "pm": pm,
            }),
        );
    }

    if langs.contains(&"Go") {
        let go_version = stack.runtimes.go.clone().unwrap_or_else(|| "1.26.2".into());
        features.insert(
            "ghcr.io/djinnos/djinn-go:1".into(),
            serde_json::json!({ "version": go_version }),
        );
    }

    if langs
        .iter()
        .any(|n| matches!(*n, "Java" | "Kotlin" | "Scala"))
    {
        let build_tool =
            first_slug(&stack.package_managers, &["gradle", "maven"]).unwrap_or_else(|| "gradle".into());
        features.insert(
            "ghcr.io/djinnos/djinn-java:1".into(),
            serde_json::json!({
                "jdk_version": "25-tem",
                "build_tool": build_tool,
            }),
        );
    }

    if langs
        .iter()
        .any(|n| matches!(*n, "C" | "C++" | "Objective-C" | "CUDA"))
    {
        features.insert(
            "ghcr.io/djinnos/djinn-clang:1".into(),
            serde_json::json!({ "version": "22" }),
        );
    }

    if langs.contains(&"Ruby") {
        features.insert(
            "ghcr.io/djinnos/djinn-ruby:1".into(),
            serde_json::json!({ "version": "3.4" }),
        );
    }

    if langs.iter().any(|n| matches!(*n, "C#" | "F#")) {
        features.insert(
            "ghcr.io/djinnos/djinn-dotnet:1".into(),
            serde_json::json!({ "sdk_version": "10.0" }),
        );
    }

    let mut root = serde_json::Map::new();
    root.insert("name".into(), serde_json::json!("djinn-project"));
    root.insert(
        "image".into(),
        serde_json::json!("mcr.microsoft.com/devcontainers/base:ubuntu-22.04"),
    );
    root.insert("features".into(), serde_json::Value::Object(features));

    serde_json::to_string_pretty(&serde_json::Value::Object(root))
        .expect("devcontainer.json is always serializable")
}

fn first_slug(haystack: &[String], priority: &[&str]) -> Option<String> {
    for want in priority {
        if haystack.iter().any(|s| s == want) {
            return Some((*want).to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{LanguageStat, Runtimes, Stack};
    use serde_json::Value;

    fn stack_with(primary: &str, langs: &[&str], pms: &[&str], runtimes: Runtimes) -> Stack {
        let mut s = Stack::empty();
        s.primary_language = Some(primary.into());
        s.languages = langs
            .iter()
            .map(|n| LanguageStat {
                name: (*n).into(),
                bytes: 1,
                pct: 100.0 / langs.len() as f64,
            })
            .collect();
        s.package_managers = pms.iter().map(|p| (*p).into()).collect();
        s.runtimes = runtimes;
        s
    }

    fn parse(out: &str) -> Value {
        serde_json::from_str(out).expect("generator emits valid JSON")
    }

    #[test]
    fn rust_only_emits_worker_plus_djinn_rust() {
        let s = stack_with(
            "Rust",
            &["Rust"],
            &["cargo"],
            Runtimes {
                rust: Some("stable".into()),
                ..Runtimes::default()
            },
        );
        let v = parse(&generate_starter(&s));
        let features = v["features"].as_object().expect("features object");
        assert!(features.contains_key("ghcr.io/djinnos/djinn-agent-worker:1"));
        assert!(features.contains_key("ghcr.io/djinnos/djinn-rust:1"));
        assert_eq!(features.len(), 2, "only worker + rust");
        assert_eq!(
            features["ghcr.io/djinnos/djinn-rust:1"]["toolchain"],
            "stable"
        );
        assert!(
            !v.as_object()
                .unwrap()
                .contains_key("postCreateCommand"),
            "starter must not emit postCreateCommand"
        );
    }

    #[test]
    fn ts_pnpm_emits_djinn_typescript_with_pnpm() {
        let s = stack_with(
            "TypeScript",
            &["TypeScript"],
            &["pnpm"],
            Runtimes {
                node: Some("24".into()),
                ..Runtimes::default()
            },
        );
        let v = parse(&generate_starter(&s));
        let features = v["features"].as_object().expect("features object");
        assert!(features.contains_key("ghcr.io/djinnos/djinn-agent-worker:1"));
        let ts = &features["ghcr.io/djinnos/djinn-typescript:1"];
        assert_eq!(ts["pm"], "pnpm");
        assert_eq!(ts["node_version"], "24");
    }

    #[test]
    fn ts_without_detected_pm_defaults_to_pnpm() {
        let s = stack_with("JavaScript", &["JavaScript"], &[], Runtimes::default());
        let v = parse(&generate_starter(&s));
        let features = v["features"].as_object().unwrap();
        assert_eq!(
            features["ghcr.io/djinnos/djinn-typescript:1"]["pm"],
            "pnpm"
        );
        assert_eq!(
            features["ghcr.io/djinnos/djinn-typescript:1"]["node_version"],
            "24"
        );
    }

    #[test]
    fn python_uv_defaults() {
        let s = stack_with("Python", &["Python"], &["uv"], Runtimes::default());
        let v = parse(&generate_starter(&s));
        let features = v["features"].as_object().unwrap();
        let py = &features["ghcr.io/djinnos/djinn-python:1"];
        assert_eq!(py["pm"], "uv");
        assert_eq!(py["version"], "3.14");
    }

    #[test]
    fn polyglot_rust_ts_python_emits_all_three_plus_worker() {
        let s = stack_with(
            "TypeScript",
            &["TypeScript", "Rust", "Python"],
            &["pnpm", "cargo", "uv"],
            Runtimes {
                node: Some("24".into()),
                rust: Some("stable".into()),
                python: Some("3.14".into()),
                ..Runtimes::default()
            },
        );
        let v = parse(&generate_starter(&s));
        let features = v["features"].as_object().unwrap();
        for want in [
            "ghcr.io/djinnos/djinn-agent-worker:1",
            "ghcr.io/djinnos/djinn-rust:1",
            "ghcr.io/djinnos/djinn-typescript:1",
            "ghcr.io/djinnos/djinn-python:1",
        ] {
            assert!(features.contains_key(want), "missing {want}");
        }
        assert_eq!(features.len(), 4);
    }

    #[test]
    fn go_java_clang_ruby_dotnet_all_map() {
        // Sweep the long tail in one stack so we lock in the mapping table.
        let s = stack_with(
            "Go",
            &["Go", "Java", "C++", "Ruby", "C#"],
            &["gradle"],
            Runtimes {
                go: Some("1.26.2".into()),
                ..Runtimes::default()
            },
        );
        let v = parse(&generate_starter(&s));
        let features = v["features"].as_object().unwrap();
        for want in [
            "ghcr.io/djinnos/djinn-agent-worker:1",
            "ghcr.io/djinnos/djinn-go:1",
            "ghcr.io/djinnos/djinn-java:1",
            "ghcr.io/djinnos/djinn-clang:1",
            "ghcr.io/djinnos/djinn-ruby:1",
            "ghcr.io/djinnos/djinn-dotnet:1",
        ] {
            assert!(features.contains_key(want), "missing {want}");
        }
        assert_eq!(features.len(), 6);
        assert_eq!(features["ghcr.io/djinnos/djinn-java:1"]["build_tool"], "gradle");
    }

    #[test]
    fn empty_stack_still_emits_worker_only() {
        let s = Stack::empty();
        let v = parse(&generate_starter(&s));
        let features = v["features"].as_object().unwrap();
        assert_eq!(features.len(), 1);
        assert!(features.contains_key("ghcr.io/djinnos/djinn-agent-worker:1"));
    }
}
