// MCP tools for knowledge base operations: CRUD, search, graph, git history,
// health reporting, and memory↔task reference tracking.

use std::path::Path;

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};

use crate::server::DjinnMcpServer;
use djinn_db::ProjectRepository;
use djinn_db::{
    NoteRepository, NoteRevisionReason, TrustedNoteRevisionAttribution,
    TrustedNoteRevisionProvenance,
};
use djinn_memory::{GitLogEntry, Note};

pub(crate) mod types;
pub use types::*;

mod associations;
mod confirm;
pub(crate) mod contradiction;
mod delete_ops;
mod edit_ops;
mod lifecycle;
mod move_ops;
pub mod ops;
mod reads;
mod recall_trace;
mod repair_embeddings;
mod run_enrichment;
mod search;
pub(crate) mod summaries;
mod write_dedup;
mod write_dedup_prompt;
mod write_dedup_runtime;
mod write_dedup_types;
mod write_services;
pub use summaries::NoteSummaryService;
mod writes;

#[cfg(test)]
mod associations_tests;
#[cfg(test)]
mod build_context_tests;
#[cfg(test)]
mod embedding_filter_tests;
#[cfg(test)]
mod embedding_health_tests;
#[cfg(test)]
mod embedding_precedence_tests;
#[cfg(test)]
mod entity_types_tests;
#[cfg(test)]
mod ops_tests;
#[cfg(test)]
mod recall_trace_tests;
#[cfg(test)]
mod run_enrichment_tests;
#[cfg(test)]
mod search_tests;
#[cfg(test)]
mod write_dedup_prompt_tests;
#[cfg(test)]
mod write_dedup_tests;
#[cfg(test)]
mod writes_tests;

// ── Router composition ────────────────────────────────────────────────────────

impl DjinnMcpServer {
    pub fn memory_tool_router() -> rmcp::handler::server::router::tool::ToolRouter<Self> {
        Self::memory_reads_router()
            + Self::memory_confirm_router()
            + Self::memory_writes_router()
            + Self::memory_search_router()
            + Self::memory_recall_trace_router()
            + Self::memory_associations_router()
            + Self::memory_repair_embeddings_router()
            + Self::memory_run_enrichment_router()
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

impl DjinnMcpServer {
    /// Resolve a project path **or name** to its DB project_id.
    pub(crate) async fn project_id_for_path(&self, project_ref: &str) -> Option<String> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        repo.resolve(project_ref).await.ok().flatten()
    }

    /// Resolve a project path/name to ID, erroring if not found.
    pub(crate) async fn resolve_project_id(&self, project_path: &str) -> Result<String, String> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        repo.resolve(project_path)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("project not found: {project_path}"))
    }
}

/// Resolve a note by permalink (primary) or title search (fallback).
async fn resolve_note_by_identifier(
    repo: &NoteRepository,
    project_id: &str,
    identifier: &str,
) -> Option<Note> {
    if let Ok(Some(note)) = repo.get(identifier).await
        && note.project_id == project_id
    {
        return Some(note);
    }

    repo.resolve(project_id, identifier).await.ok().flatten()
}

fn note_to_view(note: &Note) -> MemoryNoteView {
    MemoryNoteView::from(note)
}

fn parse_task_ref_item(raw: serde_json::Value) -> Option<MemoryTaskRefItem> {
    serde_json::from_value(raw).ok()
}

fn parse_proposal_ref_item(raw: serde_json::Value) -> Option<MemoryProposalRefItem> {
    serde_json::from_value(raw).ok()
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
    crate::tools::git_ops::git_log_for_file(file_path, limit).await
}

// `git_diff_for_file` was deleted alongside the live memory_diff tool body;
// notes are now db-only, so there's no on-disk file to diff. The
// memory_diff tool surface still exists (for back-compat) but always
// returns an empty diff with an error message.

#[cfg(test)]
mod param_tests {
    use super::*;
    use schemars::schema_for;

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
    fn mergeable_write_and_edit_params_deserialize_case_and_pitfall_types() {
        let write_params: WriteParams = serde_json::from_value(serde_json::json!({
            "project": "/tmp/test",
            "title": "Recovered Incident",
            "content": "details",
            "type": "case"
        }))
        .unwrap();
        assert_eq!(write_params.note_type, "case");

        let edit_params: EditParams = serde_json::from_value(serde_json::json!({
            "project": "/tmp/test",
            "identifier": "reference/test",
            "operation": "append",
            "content": "details",
            "type": "pitfall"
        }))
        .unwrap();
        assert_eq!(edit_params.note_type, Some("pitfall".to_string()));
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

    #[test]
    fn move_params_accepts_proposed_adr_recovery_type() {
        let json = serde_json::json!({
            "project": "/tmp/test",
            "identifier": "decisions/adr-052",
            "type": "proposed_adr"
        });
        let params: MoveParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.note_type, "proposed_adr");
    }

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

/// Validate the wire explanation before any project lookup, and derive immutable
/// revision identity only from the server-owned authenticated execution scope.
fn revision_context(
    reason: &str,
) -> Result<
    (
        NoteRevisionReason,
        TrustedNoteRevisionAttribution,
        TrustedNoteRevisionProvenance,
    ),
    String,
> {
    let reason = NoteRevisionReason::new(reason).map_err(|_| {
        "invalid parameters: field: reason, message: reason must be non-blank".to_owned()
    })?;
    let context = djinn_core::auth_context::current_revision_caller_context()
        .ok_or_else(|| "authenticated revision caller context is required".to_owned())?;
    Ok((
        reason,
        TrustedNoteRevisionAttribution::try_from(&context).map_err(|error| error.to_string())?,
        TrustedNoteRevisionProvenance::try_from(&context).map_err(|error| error.to_string())?,
    ))
}
