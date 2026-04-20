//! `Cargo.toml` + `rust-toolchain.toml` parsing. We only care about two
//! signals from the root manifest: does a `[workspace]` section exist,
//! and is a Rust toolchain pinned?

use serde::Deserialize;

#[derive(Debug, Clone, Default)]
pub struct CargoInfo {
    /// `true` when the root Cargo.toml has a `[workspace]` table.
    pub is_workspace: bool,
    /// Whatever was in the root manifest's `package.rust-version` or
    /// the sibling `rust-toolchain.toml` (`toolchain.channel`). Normalized
    /// to the raw string — we don't resolve `"stable"` vs `"1.84"` here.
    pub rust_version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawCargoToml {
    #[serde(default)]
    workspace: Option<toml::Value>,
    #[serde(default)]
    package: Option<RawPackage>,
}

#[derive(Debug, Deserialize)]
struct RawPackage {
    #[serde(rename = "rust-version", default)]
    rust_version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawToolchainFile {
    #[serde(default)]
    toolchain: Option<RawToolchain>,
}

#[derive(Debug, Deserialize)]
struct RawToolchain {
    #[serde(default)]
    channel: Option<String>,
}

/// Parse a `Cargo.toml` body. `rust_toolchain_toml` is optional: pass
/// `Some(body)` when a sibling `rust-toolchain.toml` exists.
pub fn parse_cargo_toml(cargo_toml: &str, rust_toolchain_toml: Option<&str>) -> CargoInfo {
    let raw: RawCargoToml = match toml::from_str(cargo_toml) {
        Ok(v) => v,
        Err(err) => {
            tracing::debug!(%err, "Cargo.toml parse failed, returning default");
            return CargoInfo::default();
        }
    };
    let is_workspace = raw.workspace.is_some();
    let from_cargo = raw.package.and_then(|p| p.rust_version);
    let from_toolchain = rust_toolchain_toml.and_then(|body| {
        toml::from_str::<RawToolchainFile>(body)
            .ok()
            .and_then(|f| f.toolchain.and_then(|t| t.channel))
    });
    // Sibling `rust-toolchain.toml` is the more authoritative signal —
    // it's what rustup actually honors — so prefer it when present.
    let rust_version = from_toolchain.or(from_cargo);

    CargoInfo {
        is_workspace,
        rust_version,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_workspace_and_rust_version() {
        let cargo = r#"
[workspace]
members = ["crates/*"]
resolver = "2"

[package]
name = "x"
version = "0.1.0"
rust-version = "1.84"
"#;
        let info = parse_cargo_toml(cargo, None);
        assert!(info.is_workspace);
        assert_eq!(info.rust_version.as_deref(), Some("1.84"));
    }

    #[test]
    fn toolchain_file_wins_over_cargo_rust_version() {
        let cargo = r#"
[package]
name = "x"
version = "0.1.0"
rust-version = "1.80"
"#;
        let toolchain = r#"
[toolchain]
channel = "stable"
"#;
        let info = parse_cargo_toml(cargo, Some(toolchain));
        assert_eq!(info.rust_version.as_deref(), Some("stable"));
    }
}
