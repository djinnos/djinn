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
    use djinn_db::{
        Database, NoteRepository, ProjectRepository, SettingsRepository, UserRepository,
    };
    use djinn_provider::provider::AuthMethod;
    use djinn_provider::repos::CredentialRepository;
    use djinn_provider::{CompletionRequest, CompletionResponse};

    use crate::tools::memory_tools::write_dedup::{
        LlmMemoryWriteDedupDecider, WriteDedupOutcome, apply_created_note_supersede,
        apply_dedup_decision, maybe_apply_write_dedup,
    };
    use crate::tools::memory_tools::write_dedup_runtime::LlmMemoryWriteProviderRuntime;
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

    async fn create_project(db: &Database, _root: &std::path::Path) -> djinn_core::models::Project {
        ProjectRepository::new(db.clone(), EventBus::noop())
            .create("test-project", "test", "test-project")
            .await
            .unwrap()
    }

    async fn seed_memory_model(db: &Database) {
        SettingsRepository::new(db.clone(), EventBus::noop())
            .set("settings.raw", r#"{"models":["openai/gpt-4.1-mini"]}"#)
            .await
            .unwrap();
    }

    async fn seed_user(db: &Database, github_id: i64, login: &str) -> String {
        UserRepository::new(db.clone())
            .upsert_from_github(github_id, login, None, None)
            .await
            .unwrap()
            .id
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
        .await;

        let WriteDedupOutcome::Respond(response) = response else {
            panic!("exact hash match should reuse the existing note");
        };
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
            .create(&project.id, "Async Pattern", "tokio spawn", "pattern", "[]")
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
        .unwrap();
        let WriteDedupOutcome::Respond(response) = response else {
            panic!("merge should return the existing note response");
        };

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

    #[tokio::test]
    async fn llm_decider_parses_supersede_runtime_response() {
        let decider = LlmMemoryWriteDedupDecider::with_runtime(Box::new(StaticRuntime {
            text: r#"{"action":"supersede_existing","candidate_id":"note_1","reason":"More comprehensive coverage"}"#.to_string(),
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
            MemoryWriteDedupDecision::SupersedeExisting {
                candidate_id: "note_1".to_string(),
                reason: "More comprehensive coverage".to_string(),
            }
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn llm_runtime_resolves_private_credential_for_scoped_user_and_attaches_telemetry() {
        let db = Database::open_in_memory().unwrap();
        seed_memory_model(&db).await;
        let user_a = seed_user(&db, 4001, "dedup-a").await;
        let user_b = seed_user(&db, 4002, "dedup-b").await;
        let credentials = CredentialRepository::new(db.clone(), EventBus::noop());
        credentials
            .set_with_owner("openai", "OPENAI_API_KEY", "cred_a", Some(&user_a))
            .await
            .unwrap();
        credentials
            .set_with_owner("openai", "OPENAI_API_KEY", "cred_b", Some(&user_b))
            .await
            .unwrap();

        for (user_id, expected_key) in [(&user_a, "cred_a"), (&user_b, "cred_b")] {
            let runtime = LlmMemoryWriteProviderRuntime::new(db.clone(), Some(user_id.clone()));
            let provider = runtime.resolve_provider().await.unwrap();
            let config = provider.config_snapshot().unwrap();

            match config.auth {
                AuthMethod::BearerToken(key) => assert_eq!(key, expected_key),
                _ => panic!("expected openai bearer-token auth"),
            }

            let telemetry = config.telemetry.expect("dedup telemetry attached");
            assert_eq!(telemetry.operation.as_deref(), Some("memory_write_dedup"));
            assert_eq!(telemetry.user_id.as_deref(), Some(user_id.as_str()));
            assert_eq!(telemetry.agent_type.as_deref(), Some("memory_write_dedup"));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_session_user_without_org_shared_credential_falls_back_to_create_new() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        seed_memory_model(&db).await;
        let private_user = seed_user(&db, 4003, "dedup-private-only").await;
        CredentialRepository::new(db.clone(), EventBus::noop())
            .set_with_owner(
                "openai",
                "OPENAI_API_KEY",
                "private-only",
                Some(&private_user),
            )
            .await
            .unwrap();
        assert!(
            LlmMemoryWriteProviderRuntime::new(db.clone(), None)
                .resolve_provider()
                .await
                .is_err(),
            "background scope must not resolve a private-only credential"
        );

        let project = create_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), EventBus::noop());
        repo.create(
            &project.id,
            "Existing Pattern",
            "Use channels to coordinate background workers.",
            "pattern",
            "[]",
        )
        .await
        .unwrap();

        let decider = LlmMemoryWriteDedupDecider::new(db.clone(), None);
        let response = maybe_apply_write_dedup(
            &repo,
            &decider,
            PendingWriteDedup {
                project_path: tmp.path().to_str().unwrap(),
                project_id: &project.id,
                title: "Background Worker Pattern",
                content: "Use channels to coordinate background workers and shutdown.",
                note_type: "pattern",
                status: None,
                tags_json: "[]",
            },
        )
        .await;

        assert!(
            matches!(response, WriteDedupOutcome::CreateNew),
            "unscoped dedup must safely choose CreateNew instead of using another user's credential"
        );
    }

    #[tokio::test]
    async fn supersede_deprecates_active_target_and_is_benign_for_stale_targets() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let project = create_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db, EventBus::noop());

        for status in ["active", "archived", "deprecated"] {
            let target = repo
                .create_with_status_and_retrieval_anchor(
                    &project.id,
                    &format!("Target {status}"),
                    "prior coverage",
                    "pattern",
                    Some(status),
                    "[]",
                    None,
                )
                .await
                .unwrap();
            let incoming = repo
                .create(
                    &project.id,
                    &format!("Incoming {status}"),
                    "replacement coverage",
                    "pattern",
                    "[]",
                )
                .await
                .unwrap();

            apply_created_note_supersede(&repo, &incoming.id, &target.id, "replacement")
                .await
                .unwrap();
            // A repeated decision is an association upsert and an inactive-status no-op.
            apply_created_note_supersede(&repo, &incoming.id, &target.id, "replacement")
                .await
                .unwrap();

            let stored = repo.get(&target.id).await.unwrap().unwrap();
            let expected = if status == "active" {
                "deprecated"
            } else {
                status
            };
            assert_eq!(stored.status, expected);
            assert_eq!(
                repo.get_association_kind(&incoming.id, &target.id)
                    .await
                    .unwrap(),
                Some((1.0, "supersedes".to_string()))
            );
        }
    }
}
