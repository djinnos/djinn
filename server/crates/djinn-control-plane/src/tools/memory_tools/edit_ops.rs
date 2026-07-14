use super::*;
use djinn_db::{NoteRevisionDesiredState, NoteRevisionEventKind, NoteRevisionMutation};
use rmcp::{Json, handler::server::wrapper::Parameters};
pub(super) async fn memory_edit(
    server: &DjinnMcpServer,
    Parameters(p): Parameters<EditParams>,
) -> Json<MemoryNoteResponse> {
    let (reason, attribution, provenance) = match revision_context(&p.reason) {
        Ok(context) => context,
        Err(error) => return Json(MemoryNoteResponse::error(error)),
    };
    let Some(project_id) = server.project_id_for_path(&p.project).await else {
        return Json(MemoryNoteResponse::error(format!(
            "project not found: {}",
            p.project
        )));
    };
    let repo = super::write_services::note_repository(server);
    let Some(note) = resolve_note_by_identifier(&repo, &project_id, &p.identifier).await else {
        return Json(MemoryNoteResponse::error(format!(
            "note not found: {}",
            p.identifier
        )));
    };
    let new_content = match apply_edit_operation(
        &note.content,
        &p.operation,
        &p.content,
        p.find_text.as_deref(),
        p.section.as_deref(),
    ) {
        Ok(content) => content,
        Err(error) => return Json(MemoryNoteResponse::error(error)),
    };
    match repo
        .mutate_with_revision(NoteRevisionMutation {
            project_id,
            note_id: Some(note.id),
            event_kind: NoteRevisionEventKind::Updated,
            desired: NoteRevisionDesiredState::Existing {
                content: new_content,
                confidence: note.confidence,
            },
            attribution,
            provenance,
            reason,
        })
        .await
    {
        Ok(result) => {
            let updated = result.note.expect("existing mutation returns note");
            if result.changed {
                super::lifecycle::schedule_summary_regeneration(server, &updated.id);
            }
            Json(MemoryNoteResponse::from_note(&updated))
        }
        Err(error) => Json(MemoryNoteResponse::error(error.to_string())),
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
    let start = lines
        .iter()
        .position(|line| {
            let stripped = line.trim_start_matches('#');
            line.starts_with('#') && stripped.trim().eq_ignore_ascii_case(section)
        })
        .ok_or_else(|| format!("section '{section}' not found"))?;
    let level = lines[start].chars().take_while(|&c| c == '#').count();
    let end = lines[start + 1..]
        .iter()
        .position(|line| {
            let level_here = line.chars().take_while(|&c| c == '#').count();
            level_here > 0 && level_here <= level
        })
        .map(|index| start + 1 + index)
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
