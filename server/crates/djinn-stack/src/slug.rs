use std::path::Path;

/// Derive the stable warm-pipeline slug for a workspace root path.
pub fn workspace_slug(root: &Path) -> String {
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::workspace_slug;

    #[test]
    fn workspace_slug_uses_root_for_empty_paths() {
        assert_eq!(workspace_slug(Path::new("")), "root");
    }

    #[test]
    fn workspace_slug_normalizes_path_segments() {
        assert_eq!(
            workspace_slug(Path::new("Crates/djinn_graph/scip-indexer")),
            "crates-djinn-graph-scip-indexer"
        );
    }

    #[test]
    fn workspace_slug_falls_back_to_root_when_no_slug_parts_remain() {
        assert_eq!(workspace_slug(Path::new("---")), "root");
    }
}
