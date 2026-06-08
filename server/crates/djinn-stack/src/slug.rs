use std::path::{Component, Path};

/// Derive a stable, collision-resistant workspace slug from a repo-relative root.
///
/// The human-readable prefix is the path sanitized to lowercase ASCII words. A
/// short hash of the full path is appended for every non-root path so distinct
/// roots that sanitize to the same prefix (for example `foo/bar`, `foo-bar`,
/// and `foo bar`) do not collapse onto the same workspace name.
pub fn workspace_slug(root: &Path) -> String {
    let fingerprint = fingerprint_path(root);
    let base = sanitized_path(root);

    if fingerprint.is_empty() {
        return "root".to_string();
    }

    let base = if base.is_empty() {
        "root"
    } else {
        base.as_str()
    };
    format!("{base}-{:08x}", fnv1a32(fingerprint.as_bytes()))
}

fn sanitized_path(root: &Path) -> String {
    root.components()
        .filter_map(component_slug)
        .flat_map(|segment| {
            segment
                .chars()
                .map(|ch| {
                    if ch.is_ascii_alphanumeric() {
                        ch.to_ascii_lowercase()
                    } else {
                        '-'
                    }
                })
                .collect::<String>()
                .split('-')
                .filter(|part| !part.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>()
        .join("-")
}

fn component_slug(component: Component<'_>) -> Option<String> {
    match component {
        Component::Normal(segment) => Some(segment.to_string_lossy().into_owned()),
        Component::CurDir => Some(".".to_string()),
        Component::ParentDir => Some("..".to_string()),
        Component::RootDir | Component::Prefix(_) => None,
    }
}

fn fingerprint_path(root: &Path) -> String {
    root.components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

fn fnv1a32(bytes: &[u8]) -> u32 {
    let mut hash = 0x811c_9dc5u32;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_path_maps_to_root() {
        assert_eq!(workspace_slug(Path::new("")), "root");
    }

    #[test]
    fn root_path_is_distinct_from_empty_and_root_named_workspace() {
        let empty = workspace_slug(Path::new(""));
        let absolute_root = workspace_slug(Path::new("/"));
        let root_dir = workspace_slug(Path::new("root"));

        assert_eq!(empty, "root");
        assert_ne!(absolute_root, empty);
        assert_ne!(root_dir, empty);
        assert_ne!(absolute_root, root_dir);
        assert!(absolute_root.starts_with("root-"));
        assert!(root_dir.starts_with("root-"));
    }

    #[test]
    fn colliding_sanitized_paths_get_distinct_suffixes() {
        let nested = workspace_slug(Path::new("packages/api"));
        let dashed = workspace_slug(Path::new("packages-api"));
        let spaced = workspace_slug(Path::new("packages api"));

        assert!(nested.starts_with("packages-api-"));
        assert!(dashed.starts_with("packages-api-"));
        assert!(spaced.starts_with("packages-api-"));
        assert_ne!(nested, dashed);
        assert_ne!(nested, spaced);
        assert_ne!(dashed, spaced);
    }

    #[test]
    fn unicode_segments_fall_back_to_hashed_root_prefix() {
        let first = workspace_slug(Path::new("服务"));
        let second = workspace_slug(Path::new("工具"));

        assert!(first.starts_with("root-"));
        assert!(second.starts_with("root-"));
        assert_ne!(first, second);
    }
}
