// MCP tools for knowledge base operations: CRUD, search, graph, git history,
// health reporting, and memory↔task reference tracking.

use std::path::Path;

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::db::repositories::note::NoteRepository;
use crate::db::repositories::project::ProjectRepository;
use crate::mcp::server::DjinnMcpServer;
use crate::models::note::{GitLogEntry, Note, ReindexSummary};

mod types;
pub use types::*;

mod reads;
mod search;
mod writes;

// ── Router composition ────────────────────────────────────────────────────────

impl DjinnMcpServer {
    pub fn memory_tool_router() -> rmcp::handler::server::router::tool::ToolRouter<Self> {
        Self::memory_reads_router() + Self::memory_writes_router() + Self::memory_search_router()
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

impl DjinnMcpServer {
    /// Resolve a project path **or name** to its DB project_id.
    pub(crate) async fn project_id_for_path(&self, project_ref: &str) -> Option<String> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.events().clone());
        repo.resolve(project_ref).await.ok().flatten()
    }

    /// Resolve a project path to ID, creating a project entry when missing.
    pub(crate) async fn resolve_project_id(&self, project_path: &str) -> Result<String, String> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.events().clone());
        repo.resolve_or_create(project_path)
            .await
            .map_err(|e| e.to_string())
    }
}

/// Resolve a note by permalink (primary) or title search (fallback).
async fn resolve_note_by_identifier(
    repo: &NoteRepository,
    project_id: &str,
    identifier: &str,
) -> Option<Note> {
    repo.resolve(project_id, identifier).await.ok().flatten()
}

fn note_to_view(note: &Note) -> MemoryNoteView {
    MemoryNoteView::from(note)
}

fn parse_task_ref_item(raw: serde_json::Value) -> Option<MemoryTaskRefItem> {
    serde_json::from_value(raw).ok()
}

/// Apply an edit operation to the current content, returning the new content.
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
            Ok(content.replacen(find, new_content, 1))
        }
        "replace_section" => {
            let heading = section.ok_or("replace_section requires section")?;
            replace_section_in_content(content, heading, new_content)
        }
        other => Err(format!("unknown operation: '{other}'")),
    }
}

/// Replace the body under a markdown heading with `new_body`.
///
/// The heading line itself is preserved; content from the line after the heading
/// to the next heading at the same or higher level (or EOF) is replaced.
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

/// Parse a human-readable timeframe string into hours.
///
/// Supports: "Xd", "Xh", "today", "last week", and raw integers (hours).
fn parse_timeframe(s: &str) -> i64 {
    let s = s.trim().to_lowercase();
    if s == "today" {
        return 24;
    }
    if s == "last week" || s == "lastweek" {
        return 168;
    }
    if let Some(n) = s.strip_suffix('d') {
        return n.trim().parse::<i64>().unwrap_or(7) * 24;
    }
    if let Some(n) = s.strip_suffix('h') {
        return n.trim().parse::<i64>().unwrap_or(24);
    }
    s.parse::<i64>().unwrap_or(168)
}

/// Run `git log --format="%H|||%s|||%an|||%ai" -n N -- file` and parse entries.
async fn git_log_for_file(file_path: &str, limit: i64) -> Vec<GitLogEntry> {
    let output = tokio::process::Command::new("git")
        .args([
            "log",
            "--format=%H|||%s|||%an|||%ai",
            &format!("-n{limit}"),
            "--",
            file_path,
        ])
        .output()
        .await;

    match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .filter_map(|line| {
                let parts: Vec<&str> = line.splitn(4, "|||").collect();
                if parts.len() == 4 {
                    Some(GitLogEntry {
                        sha: parts[0].to_string(),
                        message: parts[1].to_string(),
                        author: parts[2].to_string(),
                        date: parts[3].to_string(),
                    })
                } else {
                    None
                }
            })
            .collect(),
        _ => vec![],
    }
}

/// Get the unified diff for a note file at a specific commit (or the latest change).
async fn git_diff_for_file(file_path: &str, sha: Option<&str>) -> String {
    let sha = match sha {
        Some(s) => s.to_owned(),
        None => {
            // Find the most recent commit that touched this file.
            let out = tokio::process::Command::new("git")
                .args(["log", "-n1", "--format=%H", "--", file_path])
                .output()
                .await;
            match out {
                Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_owned(),
                _ => return String::new(),
            }
        }
    };

    if sha.is_empty() {
        return String::new();
    }

    let out = tokio::process::Command::new("git")
        .args(["diff", &format!("{sha}^"), &sha, "--", file_path])
        .output()
        .await;

    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemars::schema_for;

    /// Verify that serde deserialization accepts the same field names as the
    /// schemars JSON schema exposes. Catches missing `#[serde(rename)]` when
    /// `#[schemars(rename)]` is present.
    #[test]
    fn write_params_deserializes_type_field() {
        let json = serde_json::json!({
            "project": "/tmp/test",
            "title": "Test Note",
            "content": "hello",
            "type": "adr"
        });
        let params: WriteParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.note_type, "adr");
    }

    #[test]
    fn edit_params_deserializes_type_field() {
        let json = serde_json::json!({
            "project": "/tmp/test",
            "identifier": "decisions/test",
            "operation": "append",
            "content": "new content",
            "type": "pattern"
        });
        let params: EditParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.note_type, Some("pattern".to_string()));
    }

    #[test]
    fn search_params_deserializes_type_field() {
        let json = serde_json::json!({
            "project": "/tmp/test",
            "query": "hello",
            "type": "research"
        });
        let params: SearchParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.note_type, Some("research".to_string()));
    }

    #[test]
    fn move_params_deserializes_type_field() {
        let json = serde_json::json!({
            "project": "/tmp/test",
            "identifier": "reference/old",
            "type": "adr"
        });
        let params: MoveParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.note_type, "adr");
    }

    /// Ensure the JSON schema exposes "type" (not "note_type") for all param
    /// structs that use the rename. If schemars rename is accidentally removed,
    /// this test catches it.
    #[test]
    fn schema_exposes_type_not_note_type() {
        fn assert_schema_has_type<T: schemars::JsonSchema>(label: &str) {
            let schema = schema_for!(T);
            let value = serde_json::to_value(&schema).unwrap();
            let props = value["properties"].as_object().expect(label);
            assert!(
                props.contains_key("type"),
                "{label}: schema should expose 'type' property"
            );
            assert!(
                !props.contains_key("note_type"),
                "{label}: schema should NOT expose 'note_type' property"
            );
        }

        assert_schema_has_type::<WriteParams>("WriteParams");
        assert_schema_has_type::<EditParams>("EditParams");
        assert_schema_has_type::<SearchParams>("SearchParams");
        assert_schema_has_type::<MoveParams>("MoveParams");
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod memory_tool_tests;
