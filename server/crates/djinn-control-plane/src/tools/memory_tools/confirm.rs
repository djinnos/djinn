use djinn_db::repositories::note::USER_CONFIRM;
use rmcp::{Json, handler::server::wrapper::Parameters, tool, tool_router};

use super::*;

#[tool_router(router = memory_confirm_router, vis = "pub(super)")]
impl DjinnMcpServer {
    #[tool(
        description = "Confirm a note's accuracy by permalink or note ID, applying a strong user confirmation confidence signal."
    )]
    pub async fn memory_confirm(
        &self,
        Parameters(p): Parameters<MemoryConfirmParams>,
    ) -> Json<MemoryConfirmResponse> {
        let Some(project_id) = self.project_id_for_path(&p.project).await else {
            return Json(MemoryConfirmResponse::error(format!(
                "project not found: {}",
                p.project
            )));
        };

        let repo = NoteRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(note) = resolve_note_by_identifier(&repo, &project_id, &p.identifier).await else {
            return Json(MemoryConfirmResponse::error(format!(
                "note not found: {}",
                p.identifier
            )));
        };

        let previous_confidence = note.confidence;
        let new_confidence = match repo.update_confidence(&note.id, USER_CONFIRM).await {
            Ok(confidence) => confidence,
            Err(error) => return Json(MemoryConfirmResponse::error(error.to_string())),
        };

        if let Some(comment) = p
            .comment
            .as_deref()
            .map(str::trim)
            .filter(|comment| !comment.is_empty())
            && let Ok(Some(updated_note)) = repo.get(&note.id).await
        {
            let mut content = updated_note.content.clone();
            if !content.is_empty() && !content.ends_with('\n') {
                content.push('\n');
            }
            if !content.is_empty() {
                content.push('\n');
            }
            content.push_str("## Confirmation\n");
            content.push_str(comment);

            let tags = serde_json::to_string(&updated_note.parsed_tags())
                .unwrap_or_else(|_| updated_note.tags.clone());
            let _ = repo
                .update(&updated_note.id, &updated_note.title, &content, &tags)
                .await;
        }

        Json(MemoryConfirmResponse {
            note_id: Some(note.id),
            permalink: Some(note.permalink),
            previous_confidence: Some(previous_confidence),
            new_confidence: Some(new_confidence),
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {

    fn workspace_tempdir() -> tempfile::TempDir {
        let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("test-tmp");
        std::fs::create_dir_all(&base).expect("create server crate test tempdir base");
        tempfile::tempdir_in(base).expect("create server crate tempdir")
    }
    use djinn_core::events::EventBus;
    use djinn_db::{Database, NoteRepository, ProjectRepository, repositories::note};

    use crate::{server::DjinnMcpServer, state::stubs::test_mcp_state};

    use super::*;

    async fn make_server() -> (DjinnMcpServer, Database, String, std::path::PathBuf) {
        let tmp = workspace_tempdir();
        let project_path = tmp.keep();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = ProjectRepository::new(db.clone(), EventBus::noop())
            .create("test-project", "test", "test-project")
            .await
            .unwrap();
        (DjinnMcpServer::new(state), db, project.id, project_path)
    }

    async fn make_note(
        db: &Database,
        project_id: &str,
        path: &std::path::Path,
        title: &str,
    ) -> djinn_memory::Note {
        let repo = NoteRepository::new(db.clone(), EventBus::noop());
        repo.create(project_id, path, title, title, "reference", "[]")
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn confirm_from_half_applies_user_confirm_signal() {
        let (server, db, project_id, path) = make_server().await;
        let note = make_note(&db, &project_id, &path, "Half Confidence").await;

        NoteRepository::new(db.clone(), EventBus::noop())
            .set_confidence(&note.id, 0.5_f64)
            .await
            .unwrap();

        let response = server
            .memory_confirm(Parameters(MemoryConfirmParams {
                project: project_id.clone(),
                identifier: note.permalink.clone(),
                comment: None,
            }))
            .await
            .0;

        assert_eq!(response.note_id.as_deref(), Some(note.id.as_str()));
        let expected = note::bayesian_update(0.5, note::USER_CONFIRM);
        assert!((response.new_confidence.unwrap() - expected).abs() < 1e-9);
        assert!(response.new_confidence.unwrap() > 0.9);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn confirm_recovers_from_floor() {
        let (server, db, project_id, path) = make_server().await;
        let note = make_note(&db, &project_id, &path, "Low Confidence").await;

        NoteRepository::new(db.clone(), EventBus::noop())
            .set_confidence(&note.id, note::CONFIDENCE_FLOOR)
            .await
            .unwrap();

        let response = server
            .memory_confirm(Parameters(MemoryConfirmParams {
                project: project_id.clone(),
                identifier: note.permalink.clone(),
                comment: None,
            }))
            .await
            .0;

        let new_confidence = response.new_confidence.unwrap();
        assert!(
            new_confidence > 0.3,
            "expected substantial recovery, got {new_confidence}"
        );
        assert!(
            (new_confidence - note::bayesian_update(note::CONFIDENCE_FLOOR, note::USER_CONFIRM))
                .abs()
                < 1e-9
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn confirm_near_ceiling_stays_near_ceiling() {
        let (server, db, project_id, path) = make_server().await;
        let note = make_note(&db, &project_id, &path, "High Confidence").await;

        NoteRepository::new(db.clone(), EventBus::noop())
            .set_confidence(&note.id, 0.97_f64)
            .await
            .unwrap();

        let response = server
            .memory_confirm(Parameters(MemoryConfirmParams {
                project: project_id.clone(),
                identifier: note.permalink.clone(),
                comment: None,
            }))
            .await
            .0;

        let new_confidence = response.new_confidence.unwrap();
        assert!(new_confidence <= note::CONFIDENCE_CEILING + 1e-9);
        assert!(new_confidence >= 0.97);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn confirm_resolves_by_note_id() {
        let (server, db, project_id, path) = make_server().await;
        let note = make_note(&db, &project_id, &path, "Resolve By ID").await;

        NoteRepository::new(db.clone(), EventBus::noop())
            .set_confidence(&note.id, 0.5_f64)
            .await
            .unwrap();

        let response = server
            .memory_confirm(Parameters(MemoryConfirmParams {
                project: project_id.clone(),
                identifier: note.id.clone(),
                comment: None,
            }))
            .await
            .0;

        assert_eq!(response.note_id.as_deref(), Some(note.id.as_str()));
        assert_eq!(response.permalink.as_deref(), Some(note.permalink.as_str()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn confirm_invalid_identifier_returns_error() {
        let (server, _db, project_id, _path) = make_server().await;

        let response = server
            .memory_confirm(Parameters(MemoryConfirmParams {
                project: project_id.clone(),
                identifier: "missing-note".to_string(),
                comment: None,
            }))
            .await
            .0;

        assert!(response.error.is_some());
        assert!(response.note_id.is_none());
        assert!(response.new_confidence.is_none());
    }
}
