use std::path::Path;

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
