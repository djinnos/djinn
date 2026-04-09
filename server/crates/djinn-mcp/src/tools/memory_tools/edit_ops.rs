use super::*;

use rmcp::{Json, handler::server::wrapper::Parameters};

pub(super) async fn memory_edit_with_worktree(
    server: &DjinnMcpServer,
    Parameters(p): Parameters<EditParams>,
    worktree_root: Option<std::path::PathBuf>,
) -> Json<MemoryNoteResponse> {
    let Some(project_id) = server.project_id_for_path(&p.project).await else {
        return Json(MemoryNoteResponse::error(format!(
            "project not found: {}",
            p.project
        )));
    };

    let repo = NoteRepository::new(server.state.db().clone(), server.state.event_bus())
        .with_worktree_root(worktree_root);

    let note = match resolve_note_by_identifier(&repo, &project_id, &p.identifier).await {
        Some(n) => n,
        None => {
            return Json(MemoryNoteResponse::error(format!(
                "note not found: {}",
                p.identifier
            )));
        }
    };

    let note = if let Some(ref new_type) = p.note_type {
        if new_type != &note.note_type {
            match repo
                .move_note(&note.id, Path::new(&p.project), &note.title, new_type)
                .await
            {
                Ok(moved) => moved,
                Err(e) => return Json(MemoryNoteResponse::error(e.to_string())),
            }
        } else {
            note
        }
    } else {
        note
    };

    let new_content = match apply_edit_operation(
        &note.content,
        &p.operation,
        &p.content,
        p.find_text.as_deref(),
        p.section.as_deref(),
    ) {
        Ok(c) => c,
        Err(e) => return Json(MemoryNoteResponse::error(e)),
    };

    match repo
        .update(&note.id, &note.title, &new_content, &note.tags)
        .await
    {
        Ok(updated) => {
            super::lifecycle::schedule_summary_regeneration(server, &updated.id);
            Json(MemoryNoteResponse::from_note(&updated))
        }
        Err(e) => Json(MemoryNoteResponse::error(e.to_string())),
    }
}

fn apply_edit_operation(
    content: &str,
    operation: &str,
    new_content: &str,
    find_text: Option<&str>,
    section: Option<&str>,
) -> Result<String, String> {
    match operation {
        "append" => Ok(if content.is_empty() {
            new_content.to_string()
        } else {
            format!("{content}\n\n{new_content}")
        }),
        "prepend" => Ok(if content.is_empty() {
            new_content.to_string()
        } else {
            format!("{new_content}\n\n{content}")
        }),
        "find_replace" => {
            let find = find_text.ok_or("find_replace requires find_text")?;
            if !content.contains(find) {
                return Err(format!("text not found: '{find}'"));
            }
            if find == new_content {
                return Err(format!(
                    "find_replace no-op: find_text equals new content ('{find}'); not updating note"
                ));
            }
            Ok(content.replacen(find, new_content, 1))
        }
        "replace_section" => {
            let heading = section.ok_or("replace_section requires section")?;
            replace_section_in_content(content, heading, new_content)
        }
        other => Err(format!("unknown operation: '{other}'")),
    }
}

fn replace_section_in_content(
    content: &str,
    section: &str,
    new_body: &str,
) -> Result<String, String> {
    let lines: Vec<&str> = content.lines().collect();

    let heading_idx = lines.iter().position(|l| {
        let stripped = l.trim_start_matches('#');
        l.starts_with('#') && stripped.trim().eq_ignore_ascii_case(section)
    });

    let start = heading_idx.ok_or_else(|| format!("section '{section}' not found"))?;
    let heading_level = lines[start].chars().take_while(|&c| c == '#').count();

    let end = lines[start + 1..]
        .iter()
        .position(|l| {
            let lvl = l.chars().take_while(|&c| c == '#').count();
            lvl > 0 && lvl <= heading_level
        })
        .map(|i| start + 1 + i)
        .unwrap_or(lines.len());

    let mut result = lines[..=start].join("\n");
    result.push('\n');
    result.push_str(new_body);
    if !new_body.is_empty() && !new_body.ends_with('\n') && end < lines.len() {
        result.push('\n');
    }
    if end < lines.len() {
        result.push('\n');
        result.push_str(&lines[end..].join("\n"));
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::apply_edit_operation;

    #[test]
    fn replace_section_updates_heading_body_only() {
        let content = "# Intro\nhello\n## Details\nold\n# Next\nkeep";
        let updated =
            apply_edit_operation(content, "replace_section", "new", None, Some("Details")).unwrap();

        assert_eq!(updated, "# Intro\nhello\n## Details\nnew\n\n# Next\nkeep");
    }
}
