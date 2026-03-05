use super::*;

// ── Note type helpers ────────────────────────────────────────────────────────

/// Return the storage folder for a given note type.
///
/// Singleton types (`brief`, `roadmap`) map to `""` (project .djinn/ root).
pub fn folder_for_type(note_type: &str) -> &'static str {
    match note_type {
        "adr" => "decisions",
        "pattern" => "patterns",
        "research" => "research",
        "requirement" => "requirements",
        "reference" => "reference",
        "design" => "design",
        "session" => "research/sessions",
        "persona" => "design/personas",
        "journey" => "design/journeys",
        "design_spec" => "design/specs",
        "competitive" => "research/competitive",
        "tech_spike" => "research/technical",
        // Singletons live at the .djinn/ root, no subfolder.
        "brief" | "roadmap" => "",
        // Unknown types fall back to reference.
        _ => "reference",
    }
}

/// Returns `true` for note types that have exactly one instance per project.
pub fn is_singleton(note_type: &str) -> bool {
    matches!(note_type, "brief" | "roadmap")
}

/// Derive the project-scoped permalink for a note.
///
/// Singletons use their type name as the permalink (`"brief"`, `"roadmap"`).
/// Other types use `"{folder}/{slug}"`.
pub fn permalink_for(note_type: &str, title: &str) -> String {
    if is_singleton(note_type) {
        return note_type.to_string();
    }
    let folder = folder_for_type(note_type);
    let slug = slugify(title);
    if folder.is_empty() {
        slug
    } else {
        format!("{folder}/{slug}")
    }
}

/// Return the absolute path where a note's markdown file should be stored.
pub fn file_path_for(project_path: &Path, note_type: &str, title: &str) -> PathBuf {
    let djinn = project_path.join(".djinn");
    if is_singleton(note_type) {
        return djinn.join(format!("{note_type}.md"));
    }
    let folder = folder_for_type(note_type);
    let slug = slugify(title);
    if folder.is_empty() {
        djinn.join(format!("{slug}.md"))
    } else {
        djinn.join(folder).join(format!("{slug}.md"))
    }
}

/// Convert a title into a URL-safe slug.
pub fn slugify(s: &str) -> String {
    let slug: String = s
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    // Collapse repeated dashes and trim leading/trailing.
    slug.split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

// ── File I/O ─────────────────────────────────────────────────────────────────

/// Write (or overwrite) a note's markdown file with YAML frontmatter.
pub(super) fn write_note_file(
    file_path: &Path,
    title: &str,
    note_type: &str,
    tags: &str,
    content: &str,
) -> Result<()> {
    if let Some(parent) = file_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::Internal(format!("create_dir_all {}: {e}", parent.display())))?;
    }
    let file_content =
        format!("---\ntitle: {title}\ntype: {note_type}\ntags: {tags}\n---\n\n{content}",);
    std::fs::write(file_path, file_content)
        .map_err(|e| Error::Internal(format!("write note file {}: {e}", file_path.display())))?;
    Ok(())
}

// ── Catalog builder ──────────────────────────────────────────────────────────

pub(super) fn build_catalog(notes: &[(String, String, String, String)]) -> String {
    if notes.is_empty() {
        return "# Knowledge Base\n\n*No notes yet.*\n".to_string();
    }

    let mut out = String::from("# Knowledge Base\n");
    let mut current_folder = String::new();

    for (folder, title, permalink, _) in notes {
        let header = if folder.is_empty() {
            "root"
        } else {
            folder.as_str()
        };
        if header != current_folder.as_str() {
            out.push('\n');
            out.push_str(&format!("## {header}\n\n"));
            current_folder = header.to_string();
        }
        out.push_str(&format!("- [{title}]({permalink})\n"));
    }

    out
}

// ── Title/type inference helpers ─────────────────────────────────────────────

pub(super) fn title_from_permalink(permalink: &str) -> String {
    let slug = permalink.rsplit('/').next().unwrap_or(permalink);
    slug.split('-')
        .filter(|part| !part.is_empty())
        .map(capitalize_first)
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

pub(super) fn infer_note_type(permalink: &str) -> String {
    if permalink == "brief" {
        return "brief".to_string();
    }
    if permalink == "roadmap" {
        return "roadmap".to_string();
    }

    let folder = permalink.split('/').next().unwrap_or_default();
    match folder {
        "decisions" => "adr",
        "patterns" => "pattern",
        "research" => "research",
        "requirements" => "requirement",
        "reference" => "reference",
        "design" => "design",
        _ => "reference",
    }
    .to_string()
}
