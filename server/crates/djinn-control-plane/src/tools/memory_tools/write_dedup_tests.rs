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
    use async_trait::async_trait;
    use djinn_core::events::EventBus;
    use djinn_db::{Database, NoteRepository, ProjectRepository};
    use djinn_provider::{CompletionRequest, CompletionResponse};

    use crate::tools::memory_tools::write_dedup::{
        LlmMemoryWriteDedupDecider, apply_dedup_decision, maybe_apply_write_dedup,
    };
    use crate::tools::memory_tools::write_dedup_runtime::MemoryWriteProviderRuntime;
    use crate::tools::memory_tools::write_dedup_types::{
        MemoryWriteDedupDecider, MemoryWriteDedupDecision, MemoryWriteDedupDecisionInput,
        PendingWriteDedup,
    };

    struct StaticDecider {
        decision: MemoryWriteDedupDecision,
    }

    #[async_trait]
    impl MemoryWriteDedupDecider for StaticDecider {
        async fn decide(
            &self,
            _input: MemoryWriteDedupDecisionInput<'_>,
        ) -> Result<MemoryWriteDedupDecision, String> {
            Ok(self.decision.clone())
        }
    }

    struct StaticRuntime {
        text: String,
    }

    #[async_trait]
    impl MemoryWriteProviderRuntime for StaticRuntime {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, String> {
            Ok(CompletionResponse {
                text: self.text.clone(),
                ..CompletionResponse::default()
            })
        }
    }

    async fn create_project(db: &Database, root: &std::path::Path) -> djinn_core::models::Project {
        ProjectRepository::new(db.clone(), EventBus::noop())
            .create("test-project", root.to_str().unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn exact_hash_match_short_circuits_decider() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let project = create_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), EventBus::noop());

        let existing = repo
            .create(
                &project.id,
                tmp.path(),
                "Canonical",
                "Alpha\r\nBeta\n",
                "research",
                "[]",
            )
            .await
            .unwrap();

        let response = maybe_apply_write_dedup(
            &repo,
            &StaticDecider {
                decision: MemoryWriteDedupDecision::CreateNew,
            },
            PendingWriteDedup {
                project_path: tmp.path().to_str().unwrap(),
                project_id: &project.id,
                title: "Duplicate",
                content: "  Alpha\nBeta  ",
                note_type: "research",
                status: None,
                tags_json: "[]",
            },
        )
        .await
        .unwrap();

        assert_eq!(response.id.as_deref(), Some(existing.id.as_str()));
        assert!(response.deduplicated);
    }

    #[tokio::test]
    async fn llm_decider_can_merge_existing_candidate() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let project = create_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), EventBus::noop());

        let existing = repo
            .create(
                &project.id,
                tmp.path(),
                "Async Pattern",
                "tokio spawn",
                "pattern",
                "[]",
            )
            .await
            .unwrap();

        let response = apply_dedup_decision(
            &repo,
            PendingWriteDedup {
                project_path: tmp.path().to_str().unwrap(),
                project_id: &project.id,
                title: "Async Pattern Updated",
                content: "tokio spawn joinset",
                note_type: "pattern",
                status: None,
                tags_json: "[]",
            },
            MemoryWriteDedupDecision::MergeIntoExisting {
                candidate_id: existing.id.clone(),
                merged_title: "Async Pattern".to_string(),
                merged_content: "tokio spawn\njoinset".to_string(),
            },
        )
        .await
        .unwrap()
        .unwrap();

        let updated = repo.get(&existing.id).await.unwrap().unwrap();
        assert_eq!(response.id.as_deref(), Some(existing.id.as_str()));
        assert_eq!(updated.content, "tokio spawn\njoinset");
    }

    #[tokio::test]
    async fn llm_decider_parses_runtime_response() {
        let decider = LlmMemoryWriteDedupDecider::with_runtime(Box::new(StaticRuntime {
            text: r#"{"action":"reuse_existing","candidate_id":"note_1"}"#.to_string(),
        }));

        let decision = decider
            .decide(MemoryWriteDedupDecisionInput {
                project_path: "/tmp/project",
                title: "Title",
                content: "Body",
                note_type: "pattern",
                candidates: &[],
            })
            .await
            .unwrap();

        assert_eq!(
            decision,
            MemoryWriteDedupDecision::ReuseExisting {
                candidate_id: "note_1".to_string()
            }
        );
    }
}
