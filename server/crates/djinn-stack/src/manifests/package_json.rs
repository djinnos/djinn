//! `package.json` parser — extracts the fields downstream detection
//! actually uses (package manager hint, workspace presence, runtime
//! pin, dependency set for framework / test-runner detection).
//!
//! Fields beyond this set (`bin`, `files`, `exports`, ...) are ignored.

use std::collections::BTreeMap;

use serde::Deserialize;

#[derive(Debug, Clone, Default)]
pub struct PackageJsonInfo {
    /// Slug parsed from the `packageManager` field, e.g. `pnpm` /
    /// `yarn` / `npm` / `bun`. `None` when field absent or unparseable.
    pub package_manager: Option<String>,
    /// Has a non-empty `workspaces` array or object.
    pub has_workspaces: bool,
    /// `engines.node` pin (e.g. `">=20"` → `"20"`). Only major is kept.
    pub node_engine: Option<String>,
    /// Flat dependency name set (dependencies + devDependencies +
    /// peerDependencies + optionalDependencies). Used by
    /// `frameworks.rs` / `test_runners.rs`.
    pub dep_names: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawPackageJson {
    #[serde(rename = "packageManager", default)]
    package_manager: Option<String>,
    #[serde(default)]
    workspaces: Option<WorkspacesField>,
    #[serde(default)]
    engines: Option<Engines>,
    #[serde(default)]
    dependencies: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(rename = "devDependencies", default)]
    dev_dependencies: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(rename = "peerDependencies", default)]
    peer_dependencies: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(rename = "optionalDependencies", default)]
    optional_dependencies: Option<BTreeMap<String, serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum WorkspacesField {
    Globs(Vec<String>),
    Config { packages: Option<Vec<String>> },
}

#[derive(Debug, Deserialize)]
struct Engines {
    #[serde(default)]
    node: Option<String>,
}

/// Parse a `package.json` body. Returns default on malformed input — see
/// module docs.
pub fn parse_package_json(body: &str) -> PackageJsonInfo {
    let raw: RawPackageJson = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(err) => {
            tracing::debug!(%err, "package.json parse failed, returning default");
            return PackageJsonInfo::default();
        }
    };

    let package_manager = raw.package_manager.as_deref().and_then(parse_package_manager_slug);
    let has_workspaces = match raw.workspaces {
        Some(WorkspacesField::Globs(v)) => !v.is_empty(),
        Some(WorkspacesField::Config { packages }) => packages.is_some_and(|p| !p.is_empty()),
        None => false,
    };
    let node_engine = raw.engines.and_then(|e| e.node).map(normalize_node_version);

    let mut dep_names: Vec<String> = Vec::new();
    for map in [
        raw.dependencies,
        raw.dev_dependencies,
        raw.peer_dependencies,
        raw.optional_dependencies,
    ]
    .into_iter()
    .flatten()
    {
        for key in map.into_keys() {
            dep_names.push(key);
        }
    }
    dep_names.sort();
    dep_names.dedup();

    PackageJsonInfo {
        package_manager,
        has_workspaces,
        node_engine,
        dep_names,
    }
}

/// `pnpm@9.5.0` → `Some("pnpm")`, `yarn@...` → `Some("yarn")`, etc.
fn parse_package_manager_slug(raw: &str) -> Option<String> {
    let name = raw.split('@').next()?.trim();
    match name {
        "pnpm" | "npm" | "yarn" | "bun" => Some(name.to_string()),
        _ => None,
    }
}

/// `">=20.10"` / `"20.x"` / `"^22"` → `"20"` / `"20"` / `"22"`.
/// Best-effort — if no digit is found, return the string as-is.
fn normalize_node_version(raw: String) -> String {
    let digits: String = raw.chars().skip_while(|c| !c.is_ascii_digit()).take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() { raw } else { digits }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pnpm_workspace_with_deps() {
        let body = r#"{
            "name": "root",
            "packageManager": "pnpm@9.5.0",
            "workspaces": ["packages/*"],
            "engines": { "node": ">=22" },
            "dependencies": { "react": "18.3.1", "next": "14" },
            "devDependencies": { "vitest": "2.0.0" }
        }"#;
        let info = parse_package_json(body);
        assert_eq!(info.package_manager.as_deref(), Some("pnpm"));
        assert!(info.has_workspaces);
        assert_eq!(info.node_engine.as_deref(), Some("22"));
        assert!(info.dep_names.contains(&"react".to_string()));
        assert!(info.dep_names.contains(&"next".to_string()));
        assert!(info.dep_names.contains(&"vitest".to_string()));
    }

    #[test]
    fn malformed_returns_default() {
        let info = parse_package_json("{ not json");
        assert!(info.package_manager.is_none());
        assert!(!info.has_workspaces);
    }
}
