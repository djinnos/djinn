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

    #[tokio::test]
    async fn production_create_new_records_the_caller_owned_created_revision() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(test_mcp_state(db.clone()));
        let caller =
            djinn_core::auth_context::TrustedRevisionCallerContext::authenticated_human("caller-1")
                .unwrap();

        let Json(response) = djinn_core::auth_context::REVISION_CALLER_CONTEXT
            .scope(
                Some(caller),
                server.memory_write_with_decider(
                    Parameters(WriteParams {
                        project: project.slug(),
                        title: "Caller owned note".to_owned(),
                        content: "caller content".to_owned(),
                        reason: "mcp:create_note".to_owned(),
                        note_type: "research".to_owned(),
                        status: None,
                        tags: None,
                        scope_paths: None,
                        retrieval_anchor: None,
                    }),
                    &StaticDecider {
                        decision: MemoryWriteDedupDecision::CreateNew,
                    },
                ),
            )
            .await;

        assert!(
            response.error.is_none(),
            "create error: {:?}",
            response.error
        );
        let revisions = NoteRepository::new(db, EventBus::noop())
            .revision_events_for_test(&project.id)
            .await
            .unwrap();
        assert_eq!(revisions.len(), 1);
        assert_eq!(revisions[0].actor_kind, "human");
        assert_eq!(revisions[0].subsystem, None);
        assert_eq!(revisions[0].event_kind, "created");
        assert_eq!(revisions[0].content_before, None);
        assert_eq!(
            revisions[0].content_after.as_deref(),
            Some("caller content")
        );
        assert_eq!(revisions[0].confidence_before, None);
        assert_eq!(revisions[0].confidence_after, Some(0.5));
        assert_eq!(revisions[0].reason, "mcp:create_note");
    }
    use async_trait::async_trait;
    use djinn_core::events::EventBus;
    use djinn_db::{
        Database, NoteRepository, ProjectRepository, SettingsRepository, UserRepository,
    };
    use djinn_provider::provider::AuthMethod;
    use djinn_provider::repos::CredentialRepository;
    use djinn_provider::{CompletionRequest, CompletionResponse};
    use rmcp::{Json, handler::server::wrapper::Parameters};

    use crate::server::DjinnMcpServer;
    use crate::state::stubs::test_mcp_state;
    use crate::tools::memory_tools::WriteParams;
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
        create_project_named(db, "test-project").await
    }

    async fn create_project_named(db: &Database, name: &str) -> djinn_core::models::Project {
        ProjectRepository::new(db.clone(), EventBus::noop())
            .create(name, "test", name)
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
    async fn merge_persists_title_tags_and_dedup_revision() {
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
                tags_json: r#"["rust"]"#,
            },
            MemoryWriteDedupDecision::MergeIntoExisting {
                candidate_id: existing.id.clone(),
                merged_title: "Async Pattern Updated".to_string(),
                merged_content: "tokio spawn".to_string(),
            },
        )
        .await
        .unwrap();
        let WriteDedupOutcome::Respond(response) = response else {
            panic!("merge should return the existing note response");
        };

        let updated = repo.get(&existing.id).await.unwrap().unwrap();
        assert_eq!(response.id.as_deref(), Some(existing.id.as_str()));
        assert_eq!(updated.content, "tokio spawn");
        assert_eq!(updated.title, "Async Pattern Updated");
        assert_eq!(updated.tags, r#"["rust"]"#);

        let revisions = repo.revision_events_for_test(&project.id).await.unwrap();
        assert_eq!(revisions.len(), 1);
        assert_eq!(revisions[0].actor_kind, "system");
        assert_eq!(revisions[0].subsystem.as_deref(), Some("dedup"));
        assert_eq!(revisions[0].event_kind, "updated");
        assert_eq!(revisions[0].content_before.as_deref(), Some("tokio spawn"));
        assert_eq!(revisions[0].content_after.as_deref(), Some("tokio spawn"));
        assert_eq!(revisions[0].confidence_before, Some(existing.confidence));
        assert_eq!(revisions[0].confidence_after, Some(existing.confidence));
        assert_eq!(revisions[0].reason, "dedup:merge_into_existing");
    }

    #[tokio::test]
    async fn title_only_merge_persists_a_dedup_revision() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let project = create_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), EventBus::noop());
        let existing = repo
            .create(&project.id, "Old", "same", "pattern", "[]")
            .await
            .unwrap();

        let response = apply_dedup_decision(
            &repo,
            PendingWriteDedup {
                project_path: tmp.path().to_str().unwrap(),
                project_id: &project.id,
                title: "New",
                content: "same",
                note_type: "pattern",
                status: None,
                tags_json: "[]",
            },
            MemoryWriteDedupDecision::MergeIntoExisting {
                candidate_id: existing.id.clone(),
                merged_title: "New".to_owned(),
                merged_content: "same".to_owned(),
            },
        )
        .await
        .unwrap();

        assert!(matches!(response, WriteDedupOutcome::Respond(_)));
        let updated = repo.get(&existing.id).await.unwrap().unwrap();
        assert_eq!(updated.title, "New");
        assert_eq!(updated.content, "same");
        assert_eq!(updated.tags, "[]");
        let revisions = repo.revision_events_for_test(&project.id).await.unwrap();
        assert_eq!(revisions.len(), 1);
        assert_eq!(revisions[0].event_kind, "updated");
        assert_eq!(revisions[0].reason, "dedup:merge_into_existing");
    }

    #[tokio::test]
    async fn unchanged_merge_suppresses_dedup_revision() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let project = create_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), EventBus::noop());
        let existing = repo
            .create(
                &project.id,
                "Canonical",
                "unchanged content",
                "pattern",
                "[]",
            )
            .await
            .unwrap();
        let pending = PendingWriteDedup {
            project_path: tmp.path().to_str().unwrap(),
            project_id: &project.id,
            title: "Duplicate",
            content: "unchanged content",
            note_type: "pattern",
            status: None,
            tags_json: "[]",
        };

        let merged = apply_dedup_decision(
            &repo,
            pending,
            MemoryWriteDedupDecision::MergeIntoExisting {
                candidate_id: existing.id.clone(),
                merged_title: "Canonical".to_owned(),
                merged_content: "unchanged content".to_owned(),
            },
        )
        .await
        .unwrap();
        assert!(matches!(merged, WriteDedupOutcome::Respond(_)));
        assert!(
            repo.revision_events_for_test(&project.id)
                .await
                .unwrap()
                .is_empty(),
            "an unchanged canonical merge must not append a dedup revision"
        );
    }

    #[tokio::test]
    async fn reuse_existing_suppresses_dedup_revision() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let project = create_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), EventBus::noop());
        let existing = repo
            .create(
                &project.id,
                "Canonical",
                "existing content",
                "pattern",
                "[]",
            )
            .await
            .unwrap();

        let outcome = apply_dedup_decision(
            &repo,
            PendingWriteDedup {
                project_path: tmp.path().to_str().unwrap(),
                project_id: &project.id,
                title: "Duplicate",
                content: "existing content",
                note_type: "pattern",
                status: None,
                tags_json: "[]",
            },
            MemoryWriteDedupDecision::ReuseExisting {
                candidate_id: existing.id.clone(),
            },
        )
        .await
        .unwrap();

        assert!(matches!(outcome, WriteDedupOutcome::Respond(_)));
        assert!(
            repo.revision_events_for_test(&project.id)
                .await
                .unwrap()
                .is_empty(),
            "reuse must not append a dedup content or confidence revision"
        );
    }

    #[tokio::test]
    async fn create_new_leaves_revision_ownership_to_the_caller() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let project = create_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), EventBus::noop());

        let outcome = apply_dedup_decision(
            &repo,
            PendingWriteDedup {
                project_path: tmp.path().to_str().unwrap(),
                project_id: &project.id,
                title: "New note",
                content: "new content",
                note_type: "pattern",
                status: None,
                tags_json: "[]",
            },
            MemoryWriteDedupDecision::CreateNew,
        )
        .await
        .unwrap();

        assert!(matches!(outcome, WriteDedupOutcome::CreateNew));
        assert!(
            repo.revision_events_for_test(&project.id)
                .await
                .unwrap()
                .is_empty(),
            "CreateNew must not be misattributed as a dedup revision before caller creation"
        );
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
    async fn production_supersede_rejects_a_candidate_from_another_project() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let project_a = create_project_named(&db, "project-a").await;
        let project_b = create_project_named(&db, "project-b").await;
        let repo = NoteRepository::new(db.clone(), EventBus::noop());
        let local_candidate = repo
            .create(
                &project_a.id,
                "Local isolation pattern",
                "Keep memory writes isolated within the current tenant project.",
                "pattern",
                "[]",
            )
            .await
            .unwrap();
        let foreign_candidate = repo
            .create_with_status_and_retrieval_anchor(
                &project_b.id,
                "Foreign isolation pattern",
                "Foreign tenant content must remain untouched.",
                "pattern",
                Some("active"),
                "[]",
                None,
            )
            .await
            .unwrap();
        let foreign_before = repo.get(&foreign_candidate.id).await.unwrap().unwrap();
        let foreign_revision_count_before = repo
            .revision_events_for_test(&project_b.id)
            .await
            .unwrap()
            .len();
        let server = DjinnMcpServer::new(test_mcp_state(db.clone()));
        let caller =
            djinn_core::auth_context::TrustedRevisionCallerContext::authenticated_human("caller-a")
                .unwrap();

        let Json(response) = djinn_core::auth_context::REVISION_CALLER_CONTEXT
            .scope(
                Some(caller),
                server.memory_write_with_decider(
                    Parameters(WriteParams {
                        project: project_a.slug(),
                        title: "Expanded isolation pattern".to_owned(),
                        content:
                            "Keep memory writes isolated within the current tenant project boundary."
                                .to_owned(),
                        reason: "document tenant isolation".to_owned(),
                        note_type: "pattern".to_owned(),
                        status: None,
                        tags: None,
                        scope_paths: None,
                        retrieval_anchor: None,
                    }),
                    &StaticDecider {
                        decision: MemoryWriteDedupDecision::SupersedeExisting {
                            candidate_id: foreign_candidate.id.clone(),
                            reason: "more complete".to_owned(),
                        },
                    },
                ),
            )
            .await;

        assert!(
            response.error.is_some(),
            "foreign candidate must be unavailable"
        );
        let foreign_after = repo.get(&foreign_candidate.id).await.unwrap().unwrap();
        assert_eq!(foreign_after.status, foreign_before.status);
        assert_eq!(foreign_after.content, foreign_before.content);
        assert_eq!(foreign_after.confidence, foreign_before.confidence);
        assert_eq!(
            repo.revision_events_for_test(&project_b.id)
                .await
                .unwrap()
                .len(),
            foreign_revision_count_before
        );
        assert_eq!(
            repo.get_association_kind(&local_candidate.id, &foreign_candidate.id)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            repo.get_association_kind(&foreign_candidate.id, &local_candidate.id)
                .await
                .unwrap(),
            None
        );

        let follow_up = apply_created_note_supersede(
            &repo,
            &project_a.id,
            &local_candidate.id,
            &foreign_candidate.id,
            "unauthorized follow-up",
        )
        .await;
        assert!(
            follow_up.is_err(),
            "follow-up must recheck project ownership"
        );
        assert_eq!(
            repo.get_association_kind(&local_candidate.id, &foreign_candidate.id)
                .await
                .unwrap(),
            None
        );
        let foreign_final = repo.get(&foreign_candidate.id).await.unwrap().unwrap();
        assert_eq!(foreign_final.status, foreign_before.status);
        assert_eq!(foreign_final.content, foreign_before.content);
        assert_eq!(foreign_final.confidence, foreign_before.confidence);
        assert_eq!(
            repo.revision_events_for_test(&project_b.id)
                .await
                .unwrap()
                .len(),
            foreign_revision_count_before
        );

        drop(tmp);
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

            apply_created_note_supersede(
                &repo,
                &project.id,
                &incoming.id,
                &target.id,
                "replacement",
            )
            .await
            .unwrap();
            // A repeated decision is an association upsert and an inactive-status no-op.
            apply_created_note_supersede(
                &repo,
                &project.id,
                &incoming.id,
                &target.id,
                "replacement",
            )
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
            let revisions = repo.revision_events_for_test(&project.id).await.unwrap();
            assert_eq!(
                revisions.len(),
                match status {
                    "active" => 1,
                    "archived" => 2,
                    "deprecated" => 3,
                    _ => unreachable!(),
                }
            );
            for revision in revisions {
                assert_eq!(revision.actor_kind, "system");
                assert_eq!(revision.subsystem.as_deref(), Some("dedup"));
                assert_eq!(revision.event_kind, "updated");
                assert_eq!(revision.content_before.as_deref(), Some("prior coverage"));
                assert_eq!(revision.content_after.as_deref(), Some("prior coverage"));
                assert_eq!(revision.confidence_before, Some(0.5));
                assert_eq!(revision.confidence_after, Some(0.5));
                assert_eq!(revision.reason, "dedup:supersede_existing");
            }
        }
    }
}
