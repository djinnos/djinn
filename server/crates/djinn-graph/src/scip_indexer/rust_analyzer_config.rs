//! Cargo **feature parity** between `rust-analyzer scip` and the warm cargo base.
//!
//! # Why this exists
//!
//! `rust-analyzer scip` drives a `cargo metadata` + `cargo check`-shaped
//! prelude before it can analyse anything. Invoked bare — which is what this
//! planner did until now — it resolves the workspace with **default features**.
//! The warm base, meanwhile, compiles with whatever the project declared in
//! `EnvironmentConfig.workspaces[*].cargo_features` /
//! `cargo_all_features` (see `djinn_agent_worker::cargo_cache_policy`).
//!
//! Cargo's resolver-2 unifies features across the *selected package set*, and
//! the resolved feature set feeds `-C metadata` for every shared dependency.
//! Two different feature sets therefore produce two disjoint fingerprint
//! families in the same `CARGO_TARGET_DIR`: rust-analyzer's prelude cannot
//! reuse a single unit of a warm base built with a different feature set. The
//! same mechanism is already measured in `.github/workflows/quality-gate.yml`
//! (352 crates / 5m32s with a feature flag against 27s for the same package
//! without it, same cache).
//!
//! The fix is `rust-analyzer scip --config-path <json>`, which lets us hand
//! rust-analyzer the same feature selection the warm base used.
//!
//! # Config file shape (empirically pinned)
//!
//! rust-analyzer's `--config-path` file is a **nested** JSON object rooted at
//! the config namespace *without* the `rust-analyzer.` prefix:
//!
//! ```json
//! { "cargo": { "features": ["qdrant"], "noDefaultFeatures": false } }
//! ```
//!
//! The flattened LSP-client form (`{"rust-analyzer.cargo.features": [...]}`)
//! that the published config schema documents is **silently ignored** by the
//! `scip` subcommand — no error, no warning, exit 0, and an index that still
//! omits every feature-gated item. That failure mode is invisible without
//! diffing symbol counts, so `config_json_uses_the_nested_shape_not_dotted_keys`
//! below pins the shape.
//!
//! `noDefaultFeatures` is deliberately emitted as `false`: the warm base runs
//! `cargo <cmd> --features a,b` (never `--no-default-features`), so parity
//! means default features stay on.

use std::path::{Path, PathBuf};

use super::IndexerConfigFile;

/// Value of `cargo.features` that maps to `--all-features`.
const ALL_FEATURES_SENTINEL: &str = "all";

/// Directory, relative to the SCIP output root, holding generated indexer
/// config files. A dotfile directory so it can never be confused with the
/// `.scip` artifacts the collector globs out of the same root.
const CONFIG_DIR: &str = ".indexer-config";

/// The Cargo feature selection a Rust workspace was declared with.
///
/// Mirrors the two fields on [`djinn_stack::Workspace`] that
/// `djinn_agent_worker::cargo_cache_policy` turns into the warm base's
/// `--features a,b` / `--all-features` argv.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CargoFeatureSelection {
    pub features: Vec<String>,
    pub all_features: bool,
}

impl CargoFeatureSelection {
    /// True when the selection is the cargo default (no `--features`, no
    /// `--all-features`). A default selection produces **no** config file, so
    /// the planned command stays byte-identical to the pre-change one and the
    /// SCIP cache key is unchanged.
    pub(crate) fn is_default(&self) -> bool {
        !self.all_features && self.features.is_empty()
    }
}

/// Normalise a declared workspace `root` string onto the same shape
/// `discover_workspaces` produces for `DiscoveredWorkspace::root`: a path
/// relative to the project root, empty for the repo root itself.
fn normalize_rel_root(raw: &str) -> PathBuf {
    let trimmed = raw.trim().trim_start_matches("./").trim_matches('/');
    if trimmed.is_empty() || trimmed == "." {
        PathBuf::new()
    } else {
        PathBuf::from(trimmed)
    }
}

/// Resolve the Cargo feature selection for a discovered Rust workspace from
/// the project's declared `EnvironmentConfig` workspaces.
///
/// Configuration is the ONLY source: nothing here knows any feature name. A
/// project that declares no Rust workspace (or declares one with no features)
/// gets [`CargoFeatureSelection::default`], which suppresses the config file
/// entirely.
pub(crate) fn feature_selection_for_workspace(
    declared_workspaces: Option<&[djinn_stack::Workspace]>,
    workspace_rel_root: &Path,
) -> CargoFeatureSelection {
    let Some(declared) = declared_workspaces else {
        return CargoFeatureSelection::default();
    };
    declared
        .iter()
        .find(|workspace| {
            workspace.language.eq_ignore_ascii_case("rust")
                && normalize_rel_root(&workspace.root) == workspace_rel_root
        })
        .map(|workspace| CargoFeatureSelection {
            features: workspace.cargo_features.clone(),
            all_features: workspace.cargo_all_features,
        })
        .unwrap_or_default()
}

/// Render the rust-analyzer config JSON for a feature selection.
///
/// Returns `None` for the default selection so callers omit `--config-path`
/// altogether — the whole change is a no-op until a project configures
/// features.
pub(crate) fn config_json(selection: &CargoFeatureSelection) -> Option<String> {
    if selection.is_default() {
        return None;
    }
    let features = if selection.all_features {
        serde_json::Value::String(ALL_FEATURES_SENTINEL.to_string())
    } else {
        serde_json::Value::Array(
            selection
                .features
                .iter()
                .map(|feature| serde_json::Value::String(feature.clone()))
                .collect(),
        )
    };
    let document = serde_json::json!({
        "cargo": {
            "features": features,
            // Parity with the warm base, which passes `--features a,b` and
            // never `--no-default-features`.
            "noDefaultFeatures": false,
        }
    });
    // `serde_json` object key order is deterministic for a given input, so the
    // rendered bytes — and therefore the content digest that enters the SCIP
    // cache key — are stable across runs.
    serde_json::to_string_pretty(&document).ok()
}

/// Plan the rust-analyzer config file for one workspace, or `None` when the
/// selection is the cargo default.
///
/// The path is **derived**, never randomised: `<output_root>/.indexer-config/
/// <slug>-rust-analyzer.json`. A randomised temp path would change
/// `CommandShape` on every run and permanently destroy SCIP cache hits; the
/// absolute path is additionally normalised out of the cache key in
/// [`super::cache::CommandShape::from_plan`], which keys on the config
/// *content* digest instead.
pub(crate) fn config_file_for_workspace(
    output_root: &Path,
    workspace_slug: &str,
    selection: &CargoFeatureSelection,
) -> Option<IndexerConfigFile> {
    let contents = config_json(selection)?;
    Some(IndexerConfigFile {
        path: output_root
            .join(CONFIG_DIR)
            .join(format!("{workspace_slug}-rust-analyzer.json")),
        contents,
    })
}

/// RAII guard that materialises a planned indexer config file for the lifetime
/// of one indexer invocation and removes it afterwards (including on panic
/// unwind).
#[derive(Debug, Default)]
pub(crate) struct MaterializedConfig {
    path: Option<PathBuf>,
}

impl MaterializedConfig {
    /// Write `config` to its planned path. A `None` config is a no-op guard.
    pub(crate) fn write(config: Option<&IndexerConfigFile>) -> std::io::Result<Self> {
        let Some(config) = config else {
            return Ok(Self::default());
        };
        if let Some(parent) = config.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&config.path, config.contents.as_bytes())?;
        Ok(Self {
            path: Some(config.path.clone()),
        })
    }
}

impl Drop for MaterializedConfig {
    fn drop(&mut self) {
        let Some(path) = self.path.take() else {
            return;
        };
        let _ = std::fs::remove_file(&path);
        // Best-effort: succeeds only once the last workspace's config is gone.
        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selection(features: &[&str], all: bool) -> CargoFeatureSelection {
        CargoFeatureSelection {
            features: features.iter().map(|f| (*f).to_string()).collect(),
            all_features: all,
        }
    }

    fn declared(
        root: &str,
        language: &str,
        features: &[&str],
        all: bool,
    ) -> djinn_stack::Workspace {
        djinn_stack::Workspace {
            slug: None,
            name: None,
            tags: Vec::new(),
            root: root.to_string(),
            language: language.to_string(),
            toolchain: None,
            version: None,
            package_manager: None,
            cargo_features: features.iter().map(|f| (*f).to_string()).collect(),
            cargo_all_features: all,
        }
    }

    /// The default selection must render no config at all — that is what keeps
    /// the change inert for every project that has not configured features.
    #[test]
    fn default_selection_renders_no_config() {
        assert!(config_json(&CargoFeatureSelection::default()).is_none());
        assert!(
            config_file_for_workspace(
                Path::new("/out"),
                "server",
                &CargoFeatureSelection::default()
            )
            .is_none()
        );
    }

    /// EMPIRICALLY PINNED: `rust-analyzer scip --config-path` reads a NESTED
    /// object (`{"cargo": {"features": [...]}}`). The flattened
    /// `{"rust-analyzer.cargo.features": [...]}` form is silently ignored —
    /// exit 0, no warning, and an index missing every feature-gated item.
    /// Verified against rust-analyzer 1.97.1 on a two-function fixture crate:
    /// nested → the `#[cfg(feature = "extra")]` symbol is present, flattened →
    /// absent, same as no config at all.
    #[test]
    fn config_json_uses_the_nested_shape_not_dotted_keys() {
        let rendered = config_json(&selection(&["alpha"], false)).expect("config json");
        let value: serde_json::Value = serde_json::from_str(&rendered).expect("parse");
        assert_eq!(
            value["cargo"]["features"],
            serde_json::json!(["alpha"]),
            "features must live under the nested `cargo` key"
        );
        assert_eq!(
            value["cargo"]["noDefaultFeatures"],
            serde_json::json!(false)
        );
        assert!(
            value.get("rust-analyzer.cargo.features").is_none(),
            "the dotted client-config form is ignored by the scip subcommand"
        );
    }

    /// `cargo_all_features` maps to the `"all"` string sentinel, which
    /// rust-analyzer translates into `--all-features`.
    #[test]
    fn all_features_renders_the_all_sentinel() {
        let rendered = config_json(&selection(&[], true)).expect("config json");
        let value: serde_json::Value = serde_json::from_str(&rendered).expect("parse");
        assert_eq!(value["cargo"]["features"], serde_json::json!("all"));
    }

    /// Identical selections must render byte-identical JSON. The rendered
    /// bytes are what the SCIP cache key digests, so any instability here
    /// would silently destroy every future cache hit.
    #[test]
    fn identical_selections_render_identical_bytes() {
        let first = config_json(&selection(&["alpha", "beta"], false)).expect("first");
        let second = config_json(&selection(&["alpha", "beta"], false)).expect("second");
        assert_eq!(first, second);
        let reordered = config_json(&selection(&["beta", "alpha"], false)).expect("reordered");
        assert_ne!(
            first, reordered,
            "feature ORDER is part of the declared config and must not be silently normalised away"
        );
    }

    /// The derived path is stable for a fixed (output_root, slug) pair and
    /// carries no randomness.
    #[test]
    fn config_path_is_derived_and_stable() {
        let selection = selection(&["alpha"], false);
        let first = config_file_for_workspace(Path::new("/out"), "server", &selection);
        let second = config_file_for_workspace(Path::new("/out"), "server", &selection);
        assert_eq!(first, second);
        assert_eq!(
            first.expect("config").path,
            PathBuf::from("/out/.indexer-config/server-rust-analyzer.json")
        );
    }

    /// Feature lookup matches the declared Rust workspace by normalised root
    /// and ignores non-Rust workspaces entirely.
    #[test]
    fn feature_selection_matches_the_declared_rust_workspace_by_root() {
        let declared_list = vec![
            declared("ui", "typescript", &[], false),
            declared("./server/", "Rust", &["alpha"], false),
        ];

        assert_eq!(
            feature_selection_for_workspace(Some(&declared_list), Path::new("server")),
            selection(&["alpha"], false),
            "a declared rust workspace root must match the discovered rel root"
        );
        assert!(
            feature_selection_for_workspace(Some(&declared_list), Path::new("ui")).is_default(),
            "a typescript workspace must never contribute cargo features"
        );
        assert!(
            feature_selection_for_workspace(Some(&declared_list), Path::new("other")).is_default()
        );
        assert!(feature_selection_for_workspace(None, Path::new("server")).is_default());
    }

    /// A repo-root Rust workspace is declared as `"."` / `""` and discovered
    /// as an empty relative root; both must resolve to the same workspace.
    #[test]
    fn repo_root_workspace_roots_normalize_to_the_same_key() {
        for root in [".", "", "./"] {
            let declared_list = vec![declared(root, "rust", &["alpha"], false)];
            assert_eq!(
                feature_selection_for_workspace(Some(&declared_list), Path::new("")),
                selection(&["alpha"], false),
                "declared root {root:?} must match the discovered repo root"
            );
        }
    }

    /// The guard writes the planned file and removes it when dropped, so a
    /// warm never leaves generated config behind in the artifact directory.
    #[test]
    fn materialized_config_writes_then_cleans_up() {
        let tmp = tempfile::Builder::new()
            .prefix("djinn-ra-config-")
            .tempdir_in(".")
            .expect("tempdir");
        let config = config_file_for_workspace(tmp.path(), "server", &selection(&["alpha"], false))
            .expect("config");

        {
            let _guard = MaterializedConfig::write(Some(&config)).expect("write config");
            assert_eq!(
                std::fs::read_to_string(&config.path).expect("read config"),
                config.contents
            );
        }

        assert!(
            !config.path.exists(),
            "generated config must not survive the invocation"
        );
        assert!(
            !config.path.parent().expect("parent").exists(),
            "the generated config dir must be reclaimed once empty"
        );
    }

    /// A `None` config is a no-op guard — nothing written, nothing removed.
    #[test]
    fn materialized_config_is_a_noop_without_a_config() {
        let guard = MaterializedConfig::write(None).expect("noop guard");
        assert!(guard.path.is_none());
    }
}
