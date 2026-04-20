//! Manifest parsers — one module per ecosystem.
//!
//! Each parser is deliberately tolerant: malformed manifests emit a
//! trace log and return a default `Info` struct rather than erroring
//! out. Stack detection is best-effort diagnostic data; it must never
//! block the mirror-fetch pipeline.

pub mod cargo_toml;
pub mod gemfile;
pub mod go_mod;
pub mod java;
pub mod package_json;
pub mod pyproject;

pub use cargo_toml::{CargoInfo, parse_cargo_toml};
pub use gemfile::{GemfileInfo, parse_gemfile};
pub use go_mod::{GoModInfo, parse_go_mod};
pub use java::{JavaInfo, parse_gradle, parse_pom};
pub use package_json::{PackageJsonInfo, parse_package_json};
pub use pyproject::{PyprojectInfo, parse_pyproject};
