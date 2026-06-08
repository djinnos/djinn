use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::{DiscoveredWorkspace, SupportedIndexer};

// Keep the legacy `scip_indexer::workspaces::workspace_slug` path available for
// graph-internal callers while the canonical implementation lives in
// `djinn_stack`. This submodule is intentionally mostly an implementation
// detail, so the crate-level `unreachable_pub` lint cannot see the re-export as
// part of the public API even though downstream code may still import it via the
// public `scip_indexer` module.
#[allow(unreachable_pub)]
pub use djinn_stack::workspace_slug;

pub(crate) fn discover_workspaces(
    project_root: &Path,
    indexer: SupportedIndexer,
) -> Vec<DiscoveredWorkspace> {
    let mut roots = HashSet::new();
    let mut discovered = Vec::new();

    if let Err(error) = visit_dirs(project_root, &mut |path| {
        let Some(file_name) = path.file_name().and_then(OsStr::to_str) else {
            return Ok(());
        };

        if !indexer.marker_files().contains(&file_name) {
            return Ok(());
        }

        let Some(parent) = path.parent() else {
            return Ok(());
        };

        if !matches_workspace_marker(indexer, path)? {
            return Ok(());
        }

        let relative_root = parent
            .strip_prefix(project_root)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| parent.to_path_buf());
        if roots.insert(relative_root.clone()) {
            discovered.push(DiscoveredWorkspace {
                indexer,
                slug: workspace_slug(&relative_root),
                root: relative_root,
            });
        }
        Ok(())
    }) {
        tracing::warn!(
            project_root = %project_root.display(),
            language = indexer.language(),
            error = %error,
            "failed to discover workspace roots; falling back to project root"
        );
    }

    if discovered.is_empty() && !project_root.exists() {
        discovered.push(DiscoveredWorkspace {
            indexer,
            root: PathBuf::new(),
            slug: workspace_slug(Path::new("")),
        });
    }

    discovered.sort_by(|left, right| left.root.cmp(&right.root));
    discovered
}

fn matches_workspace_marker(indexer: SupportedIndexer, path: &Path) -> Result<bool> {
    match indexer {
        SupportedIndexer::RustAnalyzer => file_contains(path, "[workspace]"),
        SupportedIndexer::TypeScript => {
            // scip-typescript needs a `tsconfig.json` in the working dir, so
            // that is the only valid target marker. A bare `package.json`
            // (even one declaring `workspaces`) without a sibling tsconfig is
            // NOT a usable target — feeding scip-typescript such a dir fails
            // with "missing tsconfig.json". A monorepo whose root carries
            // both is still picked up via its `tsconfig.json` marker; each
            // sub-package that has its own tsconfig is picked up the same way.
            let file_name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
            if file_name == "tsconfig.json" {
                Ok(true)
            } else {
                // `package.json` marker: only a target if a sibling
                // tsconfig.json exists in the same dir.
                Ok(path
                    .parent()
                    .map(|dir| dir.join("tsconfig.json").is_file())
                    .unwrap_or(false))
            }
        }
        SupportedIndexer::Python
        | SupportedIndexer::Go
        | SupportedIndexer::Java
        | SupportedIndexer::Clang
        | SupportedIndexer::Ruby
        | SupportedIndexer::DotNet => Ok(true),
    }
}

fn file_contains(path: &Path, needle: &str) -> Result<bool> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("read workspace marker {}", path.display()))?;
    Ok(content.contains(needle))
}

/// Directory names that target discovery must never descend into.
///
/// Workspace markers (`tsconfig.json`, `package.json`, `Cargo.toml`, …) that
/// live under these dirs belong to vendored / generated / build-output trees,
/// never to real source. The pnpm monorepo failure mode ("all 151 SCIP
/// indexers failed") came from descending into `node_modules/.pnpm/*` and
/// treating every nested package as a separate scip-typescript target: each
/// lacks a usable `tsconfig.json`, so every invocation failed and the warm
/// pod exited 1. Pruning these segments keeps discovery scoped to the real
/// workspaces. Mirrors `scip_parser`'s `FORBIDDEN_SEGMENTS`.
const IGNORED_DIR_SEGMENTS: &[&str] = &[
    ".git",         // VCS metadata
    "node_modules", // npm / pnpm / yarn (the `.pnpm` virtual store lives here)
    "target",       // cargo
    "dist",         // bundlers (webpack, vite, rollup, parcel)
    "build",        // generic build output
    "_build",       // ocaml dune, rebar3
    ".next",        // nextjs
    ".nuxt",        // nuxt
    "__pycache__",  // cpython bytecode
    ".venv",        // python venv (PEP 405 convention)
    "venv",         // python venv (older convention)
    ".gradle",      // gradle local cache
    ".tox",         // python tox
    "vendor",       // go modules vendor / php composer / ruby vendor
    "Pods",         // CocoaPods
    ".cache",       // generic on-disk cache
];

/// True when a directory name should be pruned from target discovery.
fn is_ignored_dir(name: &OsStr) -> bool {
    name.to_str()
        .is_some_and(|n| IGNORED_DIR_SEGMENTS.contains(&n))
}

pub(crate) fn visit_dirs(root: &Path, visitor: &mut dyn FnMut(&Path) -> Result<()>) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }

    let metadata =
        fs::metadata(root).with_context(|| format!("read metadata for {}", root.display()))?;
    if metadata.is_file() {
        visitor(root)?;
        return Ok(());
    }

    for entry in fs::read_dir(root).with_context(|| format!("read dir {}", root.display()))? {
        let entry = entry.with_context(|| format!("read dir entry under {}", root.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("read file type for {}", path.display()))?;
        if file_type.is_dir() {
            // Prune vendored / generated / build-output trees so we never
            // enumerate their nested marker files as indexer targets.
            if is_ignored_dir(&entry.file_name()) {
                continue;
            }
            visit_dirs(&path, visitor)?;
        } else if file_type.is_file() {
            visitor(&path)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;

    fn tempdir_in_tmp() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("djinn-repo-map-")
            .tempdir_in(".")
            .expect("create test tempdir")
    }

    #[test]
    fn discovers_monorepo_workspaces_per_language() {
        let tmp = tempdir_in_tmp();
        let project_root = tmp.path().join("djinn");
        fs::create_dir_all(project_root.join("server")).expect("create server dir");
        fs::create_dir_all(project_root.join("desktop")).expect("create desktop dir");
        fs::create_dir_all(project_root.join("website")).expect("create website dir");
        fs::write(
            project_root.join("server/Cargo.toml"),
            "[workspace]\nmembers = []\n",
        )
        .expect("write rust workspace");
        fs::write(project_root.join("desktop/tsconfig.json"), "{}\n")
            .expect("write desktop tsconfig");
        // A monorepo root that declares workspaces AND carries a tsconfig is a
        // valid scip-typescript target.
        fs::write(
            project_root.join("website/package.json"),
            "{\"private\": true, \"workspaces\": [\"apps/*\"]}\n",
        )
        .expect("write website package.json");
        fs::write(project_root.join("website/tsconfig.json"), "{}\n")
            .expect("write website tsconfig");

        let rust_workspaces = discover_workspaces(&project_root, SupportedIndexer::RustAnalyzer);
        assert_eq!(rust_workspaces.len(), 1);
        assert_eq!(rust_workspaces[0].root, PathBuf::from("server"));
        assert_eq!(rust_workspaces[0].slug, workspace_slug(Path::new("server")));

        let ts_workspaces = discover_workspaces(&project_root, SupportedIndexer::TypeScript);
        assert_eq!(
            ts_workspaces
                .iter()
                .map(|workspace| workspace.root.clone())
                .collect::<Vec<_>>(),
            vec![PathBuf::from("desktop"), PathBuf::from("website")]
        );
        assert_eq!(
            ts_workspaces
                .iter()
                .map(|workspace| workspace.slug.clone())
                .collect::<Vec<_>>(),
            vec![
                workspace_slug(Path::new("desktop")),
                workspace_slug(Path::new("website"))
            ]
        );
    }

    #[test]
    fn falls_back_to_root_for_missing_synthetic_project_path() {
        let workspaces = discover_workspaces(
            Path::new("/tmp/djinn-nonexistent-synthetic-project"),
            SupportedIndexer::Python,
        );

        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].root, PathBuf::new());
        assert_eq!(workspaces[0].slug, "root");
        assert_eq!(workspaces[0].indexer, SupportedIndexer::Python);
    }

    /// BUG: a pnpm monorepo's `node_modules/.pnpm/*` tree carries hundreds of
    /// vendored packages, many with their own `tsconfig.json` / `package.json`.
    /// Discovery must NOT descend into `node_modules` (nor `.pnpm` under it),
    /// or every nested package becomes a doomed scip-typescript target →
    /// "all N SCIP indexers failed". Real workspaces under `packages/*` are
    /// still discovered.
    #[test]
    fn prunes_node_modules_and_pnpm_from_target_discovery() {
        let tmp = tempdir_in_tmp();
        let project_root = tmp.path().join("alt-front-end");

        // Real workspace with its own tsconfig.
        fs::create_dir_all(project_root.join("packages/acqua")).expect("create acqua dir");
        fs::write(project_root.join("packages/acqua/tsconfig.json"), "{}\n")
            .expect("write acqua tsconfig");

        // Root tsconfig (legit target).
        fs::write(project_root.join("tsconfig.json"), "{}\n").expect("write root tsconfig");

        // Vendored packages buried under node_modules/.pnpm — each has a
        // tsconfig and a package.json the OLD discovery would have planned.
        let pnpm_pkg = project_root
            .join("node_modules/.pnpm/typed-array-length-1.0.7/node_modules/typed-array-length");
        fs::create_dir_all(&pnpm_pkg).expect("create pnpm pkg dir");
        fs::write(pnpm_pkg.join("tsconfig.json"), "{}\n").expect("write vendored tsconfig");
        fs::write(pnpm_pkg.join("package.json"), "{}\n").expect("write vendored package.json");
        // Also a plain top-level node_modules package.
        let nm_pkg = project_root.join("node_modules/react");
        fs::create_dir_all(&nm_pkg).expect("create node_modules pkg dir");
        fs::write(nm_pkg.join("tsconfig.json"), "{}\n").expect("write nm tsconfig");

        let ts_workspaces = discover_workspaces(&project_root, SupportedIndexer::TypeScript);
        let roots: Vec<PathBuf> = ts_workspaces.iter().map(|w| w.root.clone()).collect();

        // Exactly the root + the real workspace package — nothing from
        // node_modules.
        assert_eq!(
            roots,
            vec![PathBuf::new(), PathBuf::from("packages/acqua")],
            "discovery must prune node_modules / .pnpm targets"
        );
        assert!(
            !roots
                .iter()
                .any(|r| r.components().any(|c| c.as_os_str() == "node_modules")),
            "no discovered target may live under node_modules"
        );
    }

    /// scip-typescript needs a `tsconfig.json`. A workspace declared only via a
    /// `package.json` (no sibling tsconfig) is NOT a usable target — including
    /// it produces the "missing tsconfig.json" failure seen in the field.
    #[test]
    fn typescript_target_requires_tsconfig() {
        let tmp = tempdir_in_tmp();
        let project_root = tmp.path().join("repo");

        // package.json-only dir → not a target.
        fs::create_dir_all(project_root.join("packages/lib")).expect("create lib dir");
        fs::write(
            project_root.join("packages/lib/package.json"),
            "{\"name\": \"lib\"}\n",
        )
        .expect("write lib package.json");

        // package.json + tsconfig dir → a target.
        fs::create_dir_all(project_root.join("packages/app")).expect("create app dir");
        fs::write(
            project_root.join("packages/app/package.json"),
            "{\"name\": \"app\"}\n",
        )
        .expect("write app package.json");
        fs::write(project_root.join("packages/app/tsconfig.json"), "{}\n")
            .expect("write app tsconfig");

        let ts_workspaces = discover_workspaces(&project_root, SupportedIndexer::TypeScript);
        let roots: Vec<PathBuf> = ts_workspaces.iter().map(|w| w.root.clone()).collect();
        assert_eq!(
            roots,
            vec![PathBuf::from("packages/app")],
            "only the tsconfig-bearing package should be a TS target"
        );
    }
}
