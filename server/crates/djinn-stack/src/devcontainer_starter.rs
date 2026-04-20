//! Generate a starter `devcontainer.json` from a detected `Stack`.
//!
//! The mapping is §7.2 of `phase3-devcontainer-and-warming.md`. The
//! Djinn agent-worker Feature is appended unconditionally so the image
//! built from this spec can be used as a per-task-run worker image.

use crate::schema::Stack;

/// Emit a pretty-printed `devcontainer.json` string (2-space indent, no
/// trailing newline) suitable for dropping into
/// `.devcontainer/devcontainer.json`.
pub fn generate_starter(stack: &Stack) -> String {
    let mut features = serde_json::Map::new();
    let mut post_create: Vec<String> = Vec::new();

    let primary = stack.primary_language.as_deref().unwrap_or("");
    let has_rust = primary == "Rust" || stack.languages.iter().any(|l| l.name == "Rust");
    let has_ts_or_js = stack
        .languages
        .iter()
        .any(|l| matches!(l.name.as_str(), "TypeScript" | "TSX" | "JavaScript"));
    let has_python = stack.languages.iter().any(|l| l.name == "Python");
    let has_go = stack.languages.iter().any(|l| l.name == "Go");
    let has_ruby = stack.languages.iter().any(|l| l.name == "Ruby");
    let has_java = stack
        .languages
        .iter()
        .any(|l| matches!(l.name.as_str(), "Java" | "Kotlin" | "Scala"));

    if has_rust {
        features.insert(
            "ghcr.io/devcontainers/features/rust:1".into(),
            serde_json::json!({ "version": stack.runtimes.rust.clone().unwrap_or_else(|| "stable".into()) }),
        );
        post_create.push("cargo fetch".into());
    }

    if has_ts_or_js {
        let node_version = stack.runtimes.node.clone().unwrap_or_else(|| "22".into());
        features.insert(
            "ghcr.io/devcontainers/features/node:1".into(),
            serde_json::json!({ "version": node_version }),
        );
        let pm = first_slug(&stack.package_managers, &["pnpm", "yarn", "bun", "npm"]);
        match pm.as_deref() {
            Some("pnpm") => {
                features.insert(
                    "ghcr.io/devcontainers-contrib/features/pnpm:2".into(),
                    serde_json::json!({}),
                );
                post_create.push("pnpm install".into());
            }
            Some("yarn") => {
                features.insert(
                    "ghcr.io/devcontainers-contrib/features/yarn-version:1".into(),
                    serde_json::json!({}),
                );
                post_create.push("yarn install".into());
            }
            Some("bun") => {
                features.insert(
                    "ghcr.io/shyim/devcontainers-features/bun:0".into(),
                    serde_json::json!({}),
                );
                post_create.push("bun install".into());
            }
            _ => {
                post_create.push("npm install".into());
            }
        }
    }

    if has_python {
        let py_version = stack.runtimes.python.clone().unwrap_or_else(|| "3.12".into());
        features.insert(
            "ghcr.io/devcontainers/features/python:1".into(),
            serde_json::json!({ "version": py_version }),
        );
        if stack.package_managers.iter().any(|p| p == "uv") {
            post_create.push("pip install uv && uv sync".into());
        } else if stack.package_managers.iter().any(|p| p == "poetry") {
            post_create.push("pipx install poetry && poetry install".into());
        } else if stack.package_managers.iter().any(|p| p == "pdm") {
            post_create.push("pipx install pdm && pdm install".into());
        } else {
            post_create.push("pip install -r requirements.txt || true".into());
        }
    }

    if has_go {
        let go_version = stack.runtimes.go.clone().unwrap_or_else(|| "latest".into());
        features.insert(
            "ghcr.io/devcontainers/features/go:1".into(),
            serde_json::json!({ "version": go_version }),
        );
    }

    if has_java {
        features.insert(
            "ghcr.io/devcontainers/features/java:1".into(),
            serde_json::json!({ "version": "21" }),
        );
    }

    if has_ruby {
        features.insert(
            "ghcr.io/devcontainers/features/ruby:1".into(),
            serde_json::json!({ "version": "3.3" }),
        );
        post_create.push("bundle install".into());
    }

    // Djinn worker Feature is always appended.
    features.insert(
        "ghcr.io/djinnos/djinn-agent-worker:1".into(),
        serde_json::json!({}),
    );

    let mut root = serde_json::Map::new();
    root.insert("name".into(), serde_json::json!("djinn-project"));
    root.insert(
        "image".into(),
        serde_json::json!("mcr.microsoft.com/devcontainers/base:ubuntu-22.04"),
    );
    root.insert("features".into(), serde_json::Value::Object(features));
    if !post_create.is_empty() {
        root.insert(
            "postCreateCommand".into(),
            serde_json::json!(post_create.join(" && ")),
        );
    }

    serde_json::to_string_pretty(&serde_json::Value::Object(root))
        .expect("devcontainer.json is always serializable")
}

fn first_slug(haystack: &[String], priority: &[&str]) -> Option<String> {
    for want in priority {
        if haystack.iter().any(|s| s == want) {
            return Some(want.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{LanguageStat, Runtimes, Stack};

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

    #[test]
    fn rust_only_emits_rust_feature_plus_djinn() {
        let s = stack_with(
            "Rust",
            &["Rust"],
            &["cargo"],
            Runtimes {
                rust: Some("stable".into()),
                ..Runtimes::default()
            },
        );
        let out = generate_starter(&s);
        assert!(out.contains("features/rust:1"));
        assert!(out.contains("djinn-agent-worker:1"));
        assert!(out.contains("cargo fetch"));
    }

    #[test]
    fn ts_pnpm_emits_node_and_pnpm() {
        let s = stack_with(
            "TypeScript",
            &["TypeScript"],
            &["pnpm"],
            Runtimes {
                node: Some("22".into()),
                ..Runtimes::default()
            },
        );
        let out = generate_starter(&s);
        assert!(out.contains("features/node:1"));
        assert!(out.contains("features/pnpm:2"));
        assert!(out.contains("pnpm install"));
    }

    #[test]
    fn polyglot_rust_plus_ts_has_both_features() {
        let s = stack_with(
            "TypeScript",
            &["TypeScript", "Rust"],
            &["pnpm", "cargo"],
            Runtimes {
                node: Some("22".into()),
                rust: Some("stable".into()),
                ..Runtimes::default()
            },
        );
        let out = generate_starter(&s);
        assert!(out.contains("features/rust:1"));
        assert!(out.contains("features/node:1"));
        assert!(out.contains("features/pnpm:2"));
    }
}
