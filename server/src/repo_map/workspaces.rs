use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

use super::{DiscoveredWorkspace, SupportedIndexer};

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
            slug: "root".to_string(),
        });
    }

    discovered.sort_by(|left, right| left.root.cmp(&right.root));
    discovered
}

fn matches_workspace_marker(indexer: SupportedIndexer, path: &Path) -> Result<bool> {
    match indexer {
        SupportedIndexer::RustAnalyzer => file_contains(path, "[workspace]"),
        SupportedIndexer::TypeScript => {
            let file_name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
            if file_name == "tsconfig.json" {
                Ok(true)
            } else {
                package_json_has_workspaces(path)
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

fn package_json_has_workspaces(path: &Path) -> Result<bool> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("read package.json {}", path.display()))?;
    let json: Value = serde_json::from_str(&content)
        .with_context(|| format!("parse package.json {}", path.display()))?;
    Ok(match json.get("workspaces") {
        Some(Value::Array(values)) => !values.is_empty(),
        Some(Value::Object(map)) => map
            .get("packages")
            .and_then(Value::as_array)
            .is_some_and(|values| !values.is_empty()),
        Some(_) => true,
        None => false,
    })
}

fn workspace_slug(root: &Path) -> String {
    if root.as_os_str().is_empty() {
        return "root".to_string();
    }

    let slug = root
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .flat_map(|segment| {
            segment
                .chars()
                .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
                .collect::<String>()
                .split('-')
                .filter(|part| !part.is_empty())
                .map(str::to_ascii_lowercase)
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>()
        .join("-");

    if slug.is_empty() {
        "root".to_string()
    } else {
        slug
    }
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
        fs::write(
            project_root.join("website/package.json"),
            "{\"private\": true, \"workspaces\": [\"apps/*\"]}\n",
        )
        .expect("write website package.json");

        let rust_workspaces = discover_workspaces(&project_root, SupportedIndexer::RustAnalyzer);
        assert_eq!(rust_workspaces.len(), 1);
        assert_eq!(rust_workspaces[0].root, PathBuf::from("server"));
        assert_eq!(rust_workspaces[0].slug, "server");

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
                .map(|workspace| workspace.slug.as_str())
                .collect::<Vec<_>>(),
            vec!["desktop", "website"]
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
}
