#[cfg(test)]
mod stop_build_tests {
    use super::*;
    use crate::server::DjinnMcpServer;
    use crate::state::stubs::test_mcp_state;
    use djinn_core::events::EventBus;
    use djinn_db::{
        Database, EpicCreateInput, EpicRepository, ProjectRepository, ProposalCreateInput,
    };

    /// A `building` proposal: one graduated epic with two open worker tasks,
    /// plus a recorded breakdown task. The slot pool is the test stub (no live
    /// sessions), so the cascade exercises the DB-observable teardown.
    async fn building_proposal() -> (
        DjinnMcpServer,
        Database,
        String,
        String,
        Vec<String>,
        String,
    ) {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let bus = EventBus::noop();
        let project = ProjectRepository::new(db.clone(), bus.clone())
            .create("svc-stop", "test", "svc-stop")
            .await
            .unwrap();

        let prepo = ProposalRepository::new(db.clone(), bus.clone());
        let proposal = prepo
            .create(ProposalCreateInput {
                title: "Stop me",
                body: "",
                acceptance_criteria: None,
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let epic = EpicRepository::new(db.clone(), bus.clone())
            .create_for_project(
                &project.id,
                EpicCreateInput {
                    title: "E",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: None,
                    auto_breakdown: Some(false),
                    originating_adr_id: None,
                    blocked_by: None,
                },
            )
            .await
            .unwrap();

        let trepo = TaskRepository::new(db.clone(), bus.clone());
        let mut task_ids = Vec::new();
        for i in 0..2 {
            let t = trepo
                .create_in_project(
                    &project.id,
                    Some(&epic.id),
                    &format!("t{i}"),
                    "",
                    "",
                    "task",
                    0,
                    "",
                    Some("open"),
                    Some("[\"do\"]"),
                )
                .await
                .unwrap();
            task_ids.push(t.id);
        }
        let breakdown = trepo
            .create_in_project(
                &project.id,
                None,
                "breakdown",
                "",
                "",
                "epic_breakdown",
                0,
                "planner",
                Some("open"),
                None,
            )
            .await
            .unwrap();

        prepo
            .link_epic(&proposal.id, &epic.id, &project.id)
            .await
            .unwrap();
        prepo
            .set_breakdown_task(&proposal.id, &breakdown.id)
            .await
            .unwrap();
        prepo.set_building(&proposal.id, "owner").await.unwrap();

        (
            DjinnMcpServer::new(test_mcp_state(db.clone())),
            db,
            proposal.id,
            epic.id,
            task_ids,
            breakdown.id,
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn freeze_and_unfreeze_toggle_the_flag() {
        let (server, db, pid, _e, _t, _b) = building_proposal().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());

        let r = server
            .proposal_stop_build(Parameters(ProposalStopBuildParams {
                id: pid.clone(),
                mode: "freeze".into(),
                reason: None,
                preview: None,
            }))
            .await
            .0;
        assert!(r.ok);
        assert!(repo.get(&pid).await.unwrap().unwrap().build_frozen);

        let r = server
            .proposal_stop_build(Parameters(ProposalStopBuildParams {
                id: pid.clone(),
                mode: "unfreeze".into(),
                reason: None,
                preview: None,
            }))
            .await
            .0;
        assert!(r.ok);
        assert!(!repo.get(&pid).await.unwrap().unwrap().build_frozen);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn preview_reports_blast_radius_without_mutating() {
        let (server, db, pid, epic_id, _t, _b) = building_proposal().await;
        let r = server
            .proposal_stop_build(Parameters(ProposalStopBuildParams {
                id: pid.clone(),
                mode: "abort".into(),
                reason: None,
                preview: Some(true),
            }))
            .await
            .0;
        assert!(r.ok);
        assert!(r.preview);
        assert_eq!(r.epics_closed, 1);
        assert_eq!(r.tasks_closed, 2, "only linked-epic children are disposed");

        // Nothing was mutated.
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        assert_eq!(repo.get(&pid).await.unwrap().unwrap().status, "building");
        let epic = EpicRepository::new(db.clone(), EventBus::noop())
            .get(&epic_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(epic.status, "open");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scoped_teardown_preview_reports_blast_radius_without_mutating() {
        let (server, db, pid, epic_id, task_ids, breakdown_id) = building_proposal().await;
        let r = server
            .dispatch_tool(
                "proposal_reconcile_obsolete_epic",
                serde_json::json!({
                    "proposal_id": pid.clone(),
                    "epic_id": epic_id.clone(),
                    "preview": true,
                }),
            )
            .await
            .unwrap();
        assert_eq!(r.get("ok").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(r.get("preview").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(r.get("epics_closed").and_then(|v| v.as_i64()), Some(1));
        assert_eq!(
            r.get("tasks_closed").and_then(|v| v.as_i64()),
            Some(task_ids.len() as i64)
        );

        let bus = EventBus::noop();
        let prepo = ProposalRepository::new(db.clone(), bus.clone());
        assert_eq!(prepo.get(&pid).await.unwrap().unwrap().status, "building");
        let links = prepo.graduated_epics(&pid).await.unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].0, epic_id);

        let erepo = EpicRepository::new(db.clone(), bus.clone());
        assert_eq!(erepo.get(&epic_id).await.unwrap().unwrap().status, "open");
        let trepo = TaskRepository::new(db.clone(), bus.clone());
        for tid in task_ids.iter().chain(std::iter::once(&breakdown_id)) {
            assert_eq!(trepo.get(tid).await.unwrap().unwrap().status, "open");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scoped_teardown_closes_only_target_epic_and_preserves_build() {
        let (server, db, pid, target_epic_id, target_task_ids, breakdown_id) =
            building_proposal().await;
        let bus = EventBus::noop();
        let project_repo = ProjectRepository::new(db.clone(), bus.clone());
        let project_id = project_repo
            .resolve("test/svc-stop")
            .await
            .unwrap()
            .unwrap();
        let project = project_repo.get(&project_id).await.unwrap().unwrap();
        let other_epic = EpicRepository::new(db.clone(), bus.clone())
            .create_for_project(
                &project.id,
                EpicCreateInput {
                    title: "Other",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: None,
                    auto_breakdown: Some(false),
                    originating_adr_id: None,
                    blocked_by: None,
                },
            )
            .await
            .unwrap();
        let trepo = TaskRepository::new(db.clone(), bus.clone());
        let other_task = trepo
            .create_in_project(
                &project.id,
                Some(&other_epic.id),
                "other-task",
                "",
                "",
                "task",
                0,
                "",
                Some("open"),
                Some("[\"do\"]"),
            )
            .await
            .unwrap();
        let prepo = ProposalRepository::new(db.clone(), bus.clone());
        prepo
            .link_epic(&pid, &other_epic.id, &project.id)
            .await
            .unwrap();
        let r = server
            .dispatch_tool(
                "proposal_reconcile_obsolete_epic",
                serde_json::json!({
                    "proposal_id": pid.clone(),
                    "epic_id": target_epic_id.clone(),
                    "reason": "obsolete after reconcile",
                }),
            )
            .await
            .unwrap();
        assert_eq!(r.get("ok").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(r.get("epics_closed").and_then(|v| v.as_i64()), Some(1));
        assert_eq!(
            r.get("tasks_closed").and_then(|v| v.as_i64()),
            Some(target_task_ids.len() as i64)
        );
        assert_eq!(r.get("blocked").and_then(|v| v.as_bool()), Some(false));

        let p = prepo.get(&pid).await.unwrap().unwrap();
        assert_eq!(p.status, "building");
        assert_eq!(
            p.build_breakdown_task_id.as_deref(),
            Some(breakdown_id.as_str())
        );
        assert_eq!(p.build_owner_user_id.as_deref(), Some("owner"));
        assert_eq!(
            prepo.graduated_epics(&pid).await.unwrap(),
            vec![(other_epic.id.clone(), project.id.clone())]
        );

        let erepo = EpicRepository::new(db.clone(), bus.clone());
        assert_eq!(
            erepo.get(&target_epic_id).await.unwrap().unwrap().status,
            "closed"
        );
        assert_eq!(
            erepo.get(&other_epic.id).await.unwrap().unwrap().status,
            "open"
        );

        for tid in &target_task_ids {
            assert_eq!(trepo.get(tid).await.unwrap().unwrap().status, "closed");
        }
        assert_eq!(
            trepo.get(&other_task.id).await.unwrap().unwrap().status,
            "open"
        );
        assert_eq!(
            trepo.get(&breakdown_id).await.unwrap().unwrap().status,
            "open"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scoped_teardown_blocks_merged_work_before_preview_or_mutation() {
        let (server, db, pid, target_epic_id, target_task_ids, breakdown_id) =
            building_proposal().await;
        let bus = EventBus::noop();
        let trepo = TaskRepository::new(db.clone(), bus.clone());
        trepo
            .set_merge_commit_sha(&target_task_ids[0], "abc123")
            .await
            .unwrap();
        let prepo = ProposalRepository::new(db.clone(), bus.clone());
        let r = server
            .dispatch_tool(
                "proposal_reconcile_obsolete_epic",
                serde_json::json!({
                    "id": pid.clone(),
                    "epic_id": target_epic_id.clone(),
                    "preview": true,
                }),
            )
            .await
            .unwrap();
        assert_eq!(r.get("ok").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(r.get("blocked").and_then(|v| v.as_bool()), Some(true));
        assert!(
            r.get("error")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .contains("blocked by merged work")
        );
        assert_eq!(r.get("epics_closed").and_then(|v| v.as_i64()), Some(0));
        assert_eq!(r.get("tasks_closed").and_then(|v| v.as_i64()), Some(0));
        assert!(
            r.get("blocked_feedback_body")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .contains("contains merged work")
        );

        let p = prepo.get(&pid).await.unwrap().unwrap();
        assert_eq!(p.status, "building");
        assert_eq!(
            p.build_breakdown_task_id.as_deref(),
            Some(breakdown_id.as_str())
        );
        assert_eq!(
            prepo.graduated_epics(&pid).await.unwrap(),
            vec![(
                target_epic_id.clone(),
                trepo
                    .get(&target_task_ids[0])
                    .await
                    .unwrap()
                    .unwrap()
                    .project_id
            )]
        );
        assert_eq!(
            EpicRepository::new(db.clone(), bus.clone())
                .get(&target_epic_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "open"
        );
        for tid in &target_task_ids {
            assert_eq!(trepo.get(tid).await.unwrap().unwrap().status, "open");
        }
        let feedback = prepo.feedback(&pid).await.unwrap();
        assert_eq!(feedback.len(), 1);
        assert_eq!(feedback[0].author_kind, "ai");
        assert!(feedback[0].body.contains("contains merged work"));
        assert!(feedback[0].body.contains(&target_task_ids[0]));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abort_tears_down_and_reverts_to_approved() {
        let (server, db, pid, epic_id, task_ids, breakdown_id) = building_proposal().await;
        let r = server
            .proposal_stop_build(Parameters(ProposalStopBuildParams {
                id: pid.clone(),
                mode: "abort".into(),
                reason: Some("changed my mind".into()),
                preview: Some(false),
            }))
            .await
            .0;
        assert!(r.ok, "abort failed: {:?}", r.error);
        assert_eq!(r.status.as_deref(), Some("approved"));
        assert_eq!(r.epics_closed, 1);
        assert_eq!(r.tasks_closed, 2);

        let bus = EventBus::noop();
        let prepo = ProposalRepository::new(db.clone(), bus.clone());
        let p = prepo.get(&pid).await.unwrap().unwrap();
        assert_eq!(p.status, "approved");
        assert!(p.build_owner_user_id.is_none());
        assert!(p.build_breakdown_task_id.is_none());
        assert!(prepo.graduated_epics(&pid).await.unwrap().is_empty());

        let epic = EpicRepository::new(db.clone(), bus.clone())
            .get(&epic_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(epic.status, "closed");

        let trepo = TaskRepository::new(db.clone(), bus.clone());
        for tid in &task_ids {
            let t = trepo.get(tid).await.unwrap().unwrap();
            assert_eq!(t.status, "closed", "linked task {tid} should be disposed");
        }
        assert_eq!(
            trepo.get(&breakdown_id).await.unwrap().unwrap().status,
            "open"
        );

        // A second abort is rejected — the proposal is no longer building.
        let r2 = server
            .proposal_stop_build(Parameters(ProposalStopBuildParams {
                id: pid,
                mode: "abort".into(),
                reason: Some("again".into()),
                preview: Some(false),
            }))
            .await
            .0;
        assert!(!r2.ok);
    }

    // ── Proposal terminal disposition integration regressions ──────────────

    /// Resolve the project created by `building_proposal()`.
    async fn resolve_project(db: &Database) -> djinn_core::models::Project {
        let bus = EventBus::noop();
        let project_repo = ProjectRepository::new(db.clone(), bus);
        let project_id = project_repo
            .resolve("test/svc-stop")
            .await
            .unwrap()
            .unwrap();
        project_repo.get(&project_id).await.unwrap().unwrap()
    }

    /// Create an epic in the given project using the shared test input shape.
    async fn make_epic(db: &Database, project_id: &str, title: &str) -> djinn_core::models::Epic {
        let bus = EventBus::noop();
        EpicRepository::new(db.clone(), bus)
            .create_for_project(
                project_id,
                EpicCreateInput {
                    title,
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: None,
                    auto_breakdown: Some(false),
                    originating_adr_id: None,
                    blocked_by: None,
                },
            )
            .await
            .unwrap()
    }

    /// Count persisted, non-archived activity for a fixed set of task IDs.
    async fn activity_count(repo: &TaskRepository, task_ids: &[String]) -> usize {
        let mut count = 0;
        for task_id in task_ids {
            count += repo.list_activity(task_id).await.unwrap().len();
        }
        count
    }

    /// Create an open worker task in a project/epic.
    async fn make_open_task(
        db: &Database,
        project_id: &str,
        epic_id: &str,
        title: &str,
    ) -> djinn_core::models::Task {
        let bus = EventBus::noop();
        TaskRepository::new(db.clone(), bus)
            .create_in_project(
                project_id,
                Some(epic_id),
                title,
                "",
                "",
                "task",
                0,
                "",
                Some("open"),
                Some("[\"do\"]"),
            )
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abort_evaluates_every_linked_epic() {
        // Abort must scope ALL linked epics: close every one, dispose every
        // direct child, unlink all, and revert the proposal to approved.
        let (server, db, pid, epic_a_id, task_a_ids, _breakdown_id) = building_proposal().await;
        let bus = EventBus::noop();
        let project = resolve_project(&db).await;

        // Second linked epic with its own open children.
        let epic_b = make_epic(&db, &project.id, "EB").await;
        let trepo = TaskRepository::new(db.clone(), bus.clone());
        let mut epic_b_tasks = Vec::new();
        for i in 0..2 {
            let t = make_open_task(&db, &project.id, &epic_b.id, &format!("eb{i}")).await;
            epic_b_tasks.push(t.id);
        }
        let prepo = ProposalRepository::new(db.clone(), bus.clone());
        prepo
            .link_epic(&pid, &epic_b.id, &project.id)
            .await
            .unwrap();

        let r = server
            .proposal_stop_build(Parameters(ProposalStopBuildParams {
                id: pid.clone(),
                mode: "abort".into(),
                reason: Some("multi-epic abort".into()),
                preview: Some(false),
            }))
            .await
            .0;
        assert!(r.ok, "abort failed: {:?}", r.error);
        assert_eq!(r.status.as_deref(), Some("approved"));
        assert_eq!(r.epics_closed, 2, "both linked epics must be closed");
        assert_eq!(r.tasks_closed, 4, "all four children disposed");
        assert_eq!(r.disposition.disposed, 4);

        // Proposal reverted and all epics unlinked.
        let p = prepo.get(&pid).await.unwrap().unwrap();
        assert_eq!(p.status, "approved");
        assert!(prepo.graduated_epics(&pid).await.unwrap().is_empty());

        // Both epics closed.
        let erepo = EpicRepository::new(db.clone(), bus.clone());
        assert_eq!(
            erepo.get(&epic_a_id).await.unwrap().unwrap().status,
            "closed"
        );
        assert_eq!(
            erepo.get(&epic_b.id).await.unwrap().unwrap().status,
            "closed"
        );

        // All children across both epics disposed.
        for tid in task_a_ids.iter().chain(epic_b_tasks.iter()) {
            assert_eq!(
                trepo.get(tid).await.unwrap().unwrap().status,
                "closed",
                "task {tid} should be disposed"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abort_retains_child_with_other_open_proposal_parent() {
        // When the same epic is linked to another building proposal, aborting
        // one proposal retains the child with parent_child_retained evidence.
        // The scoped proposal itself is excluded from the guard (self-exclusion).
        let (server, db, pid_a, epic_id, task_ids, _breakdown_id) = building_proposal().await;
        let bus = EventBus::noop();
        let project = resolve_project(&db).await;

        // Second building proposal linked to the SAME epic.
        let prepo = ProposalRepository::new(db.clone(), bus.clone());
        let proposal_b = prepo
            .create(ProposalCreateInput {
                title: "Other parent",
                body: "",
                acceptance_criteria: None,
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        prepo
            .link_epic(&proposal_b.id, &epic_id, &project.id)
            .await
            .unwrap();
        prepo.set_building(&proposal_b.id, "owner").await.unwrap();

        // Abort proposal A — children must be retained, not disposed.
        let r = server
            .proposal_stop_build(Parameters(ProposalStopBuildParams {
                id: pid_a.clone(),
                mode: "abort".into(),
                reason: Some("abort A".into()),
                preview: Some(false),
            }))
            .await
            .0;
        assert!(r.ok, "retention must not make abort fail: {:?}", r.error);
        assert_eq!(r.status.as_deref(), Some("approved"));
        assert_eq!(r.epics_closed, 1);
        assert_eq!(r.tasks_closed, 0, "retained children are not disposed");
        assert_eq!(
            r.disposition.retained_other_parent,
            task_ids.len() as i64,
            "parent_child_retained evidence for each child"
        );
        assert_eq!(r.disposition.disposed, 0);

        // Children remain open (retained by other open parent).
        let trepo = TaskRepository::new(db.clone(), bus.clone());
        for tid in &task_ids {
            assert_eq!(
                trepo.get(tid).await.unwrap().unwrap().status,
                "open",
                "retained child {tid} should stay open"
            );
        }

        // Proposal A reverted and unlinked; B still building and linked.
        assert_eq!(prepo.get(&pid_a).await.unwrap().unwrap().status, "approved");
        assert!(prepo.graduated_epics(&pid_a).await.unwrap().is_empty());
        let pb = prepo.get(&proposal_b.id).await.unwrap().unwrap();
        assert_eq!(pb.status, "building");
        let b_links = prepo.graduated_epics(&proposal_b.id).await.unwrap();
        assert_eq!(b_links.len(), 1);
        assert_eq!(b_links[0].0, epic_id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abort_cascades_internal_only_blocker_chain_across_scope() {
        // A blocker chain entirely within the abort scope (both blocker and
        // dependent in scoped epics) must not prevent disposition.
        let (server, db, pid, _epic_a_id, task_a_ids, _breakdown_id) = building_proposal().await;
        let bus = EventBus::noop();
        let project = resolve_project(&db).await;

        // Second linked epic so the scope spans two epics.
        let epic_b = make_epic(&db, &project.id, "EB").await;
        let trepo = TaskRepository::new(db.clone(), bus.clone());
        let task_b1 = make_open_task(&db, &project.id, &epic_b.id, "eb0").await;
        // Internal-only blocker: task_b1 is blocked by task_a_ids[0]; both
        // are inside the abort scope.
        trepo
            .add_blocker(&task_b1.id, &task_a_ids[0])
            .await
            .unwrap();
        let prepo = ProposalRepository::new(db.clone(), bus.clone());
        prepo
            .link_epic(&pid, &epic_b.id, &project.id)
            .await
            .unwrap();

        let r = server
            .proposal_stop_build(Parameters(ProposalStopBuildParams {
                id: pid.clone(),
                mode: "abort".into(),
                reason: Some("cascade abort".into()),
                preview: Some(false),
            }))
            .await
            .0;
        assert!(r.ok, "abort failed: {:?}", r.error);
        // No external-dependent retention — the chain is internal.
        assert_eq!(r.disposition.retained_external_dependent, 0);
        // Three open children total (2 in epic A + 1 in epic B), all disposed.
        assert_eq!(r.tasks_closed, 3);
        assert_eq!(r.disposition.disposed, 3);

        for tid in task_a_ids.iter().chain(std::iter::once(&task_b1.id)) {
            assert_eq!(
                trepo.get(tid).await.unwrap().unwrap().status,
                "closed",
                "internal-only chain member {tid} should be disposed"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abort_retains_external_open_dependent_with_evidence() {
        // A child in scope that blocks an open task OUTSIDE the scope is
        // retained with dependency evidence. The abort still succeeds.
        let (server, db, pid, _epic_id, task_ids, _breakdown_id) = building_proposal().await;
        let bus = EventBus::noop();
        let project = resolve_project(&db).await;

        // External epic NOT linked to the proposal.
        let external_epic = make_epic(&db, &project.id, "Ext").await;
        let trepo = TaskRepository::new(db.clone(), bus.clone());
        let external_task = make_open_task(&db, &project.id, &external_epic.id, "ext-dep").await;
        // external_task is blocked by task_ids[0] (the scoped child).
        trepo
            .add_blocker(&external_task.id, &task_ids[0])
            .await
            .unwrap();

        let r = server
            .proposal_stop_build(Parameters(ProposalStopBuildParams {
                id: pid.clone(),
                mode: "abort".into(),
                reason: Some("external dep abort".into()),
                preview: Some(false),
            }))
            .await
            .0;
        assert!(r.ok, "retention must not make abort fail: {:?}", r.error);
        // One child retained as external-dependent; the other disposed.
        assert_eq!(r.disposition.retained_external_dependent, 1);
        assert_eq!(r.disposition.disposed, 1);
        assert_eq!(r.tasks_closed, 1);

        // Retained child stays open; disposed child is closed.
        assert_eq!(
            trepo.get(&task_ids[0]).await.unwrap().unwrap().status,
            "open",
            "child blocking external task should be retained"
        );
        assert_eq!(
            trepo.get(&task_ids[1]).await.unwrap().unwrap().status,
            "closed"
        );
        // External task stays open.
        assert_eq!(
            trepo.get(&external_task.id).await.unwrap().unwrap().status,
            "open"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconcile_retains_external_dependent_and_is_non_fatal() {
        // Reconcile one epic; a child that blocks an open task outside the
        // scope is retained with dependency evidence. The reconcile still
        // succeeds: ok=true, epic closed, proposal still building.
        let (server, db, pid, epic_id, task_ids, _breakdown_id) = building_proposal().await;
        let bus = EventBus::noop();
        let project = resolve_project(&db).await;

        // External epic NOT in the reconcile scope.
        let external_epic = make_epic(&db, &project.id, "Ext").await;
        let trepo = TaskRepository::new(db.clone(), bus.clone());
        let external_task = make_open_task(&db, &project.id, &external_epic.id, "ext-dep").await;
        trepo
            .add_blocker(&external_task.id, &task_ids[0])
            .await
            .unwrap();

        let r = server
            .dispatch_tool(
                "proposal_reconcile_obsolete_epic",
                serde_json::json!({
                    "proposal_id": pid.clone(),
                    "epic_id": epic_id.clone(),
                }),
            )
            .await
            .unwrap();
        assert_eq!(r.get("ok").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(r.get("blocked").and_then(|v| v.as_bool()), Some(false));
        // One child retained, one disposed.
        let disp = r.get("disposition").unwrap();
        assert_eq!(
            disp.get("retained_external_dependent")
                .and_then(|v| v.as_i64()),
            Some(1)
        );
        assert_eq!(r.get("tasks_closed").and_then(|v| v.as_i64()), Some(1));

        // Retained child open; disposed child closed.
        assert_eq!(
            trepo.get(&task_ids[0]).await.unwrap().unwrap().status,
            "open"
        );
        assert_eq!(
            trepo.get(&task_ids[1]).await.unwrap().unwrap().status,
            "closed"
        );
        // Epic closed; proposal still building; external task open.
        assert_eq!(
            EpicRepository::new(db.clone(), bus.clone())
                .get(&epic_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "closed"
        );
        assert_eq!(
            ProposalRepository::new(db.clone(), bus.clone())
                .get(&pid)
                .await
                .unwrap()
                .unwrap()
                .status,
            "building"
        );
        assert_eq!(
            trepo.get(&external_task.id).await.unwrap().unwrap().status,
            "open"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abort_preview_leaves_state_and_activity_unchanged() {
        // Preview reports findings while proposal, epic, task, activity, and
        // link state all remain unchanged.
        let (server, db, pid, epic_id, task_ids, breakdown_id) = building_proposal().await;
        let bus = EventBus::noop();
        let trepo = TaskRepository::new(db.clone(), bus.clone());
        let prepo = ProposalRepository::new(db.clone(), bus.clone());

        // Snapshot link count and activity count before preview.
        let links_before = prepo.graduated_epics(&pid).await.unwrap().len();
        let all_task_ids: Vec<String> = task_ids
            .iter()
            .cloned()
            .chain(std::iter::once(breakdown_id.clone()))
            .collect();
        let activity_before = activity_count(&trepo, &all_task_ids).await;

        let r = server
            .proposal_stop_build(Parameters(ProposalStopBuildParams {
                id: pid.clone(),
                mode: "abort".into(),
                reason: None,
                preview: Some(true),
            }))
            .await
            .0;
        assert!(r.ok);
        assert!(r.preview);
        assert_eq!(r.tasks_closed, 2, "preview reports findings");
        assert_eq!(r.disposition.disposed, 2);

        // No state mutation.
        assert_eq!(prepo.get(&pid).await.unwrap().unwrap().status, "building");
        assert_eq!(
            EpicRepository::new(db.clone(), bus.clone())
                .get(&epic_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "open"
        );
        for tid in &all_task_ids {
            assert_eq!(
                trepo.get(tid).await.unwrap().unwrap().status,
                "open",
                "preview must not mutate task {tid}"
            );
        }
        // No link mutation.
        assert_eq!(
            prepo.graduated_epics(&pid).await.unwrap().len(),
            links_before
        );
        // No new activity_log rows.
        let activity_after = activity_count(&trepo, &all_task_ids).await;
        assert_eq!(
            activity_after, activity_before,
            "preview must not write activity_log rows"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconcile_preview_leaves_state_and_activity_unchanged() {
        // Preview reports findings while proposal, epic, task, activity, and
        // link state all remain unchanged.
        let (server, db, pid, epic_id, task_ids, breakdown_id) = building_proposal().await;
        let bus = EventBus::noop();
        let trepo = TaskRepository::new(db.clone(), bus.clone());
        let prepo = ProposalRepository::new(db.clone(), bus.clone());

        let links_before = prepo.graduated_epics(&pid).await.unwrap().len();
        let all_task_ids: Vec<String> = task_ids
            .iter()
            .cloned()
            .chain(std::iter::once(breakdown_id.clone()))
            .collect();
        let activity_before = activity_count(&trepo, &all_task_ids).await;

        let r = server
            .dispatch_tool(
                "proposal_reconcile_obsolete_epic",
                serde_json::json!({
                    "proposal_id": pid.clone(),
                    "epic_id": epic_id.clone(),
                    "preview": true,
                }),
            )
            .await
            .unwrap();
        assert_eq!(r.get("ok").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(r.get("preview").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(r.get("epics_closed").and_then(|v| v.as_i64()), Some(1));
        assert_eq!(
            r.get("tasks_closed").and_then(|v| v.as_i64()),
            Some(task_ids.len() as i64)
        );

        // No state mutation.
        assert_eq!(prepo.get(&pid).await.unwrap().unwrap().status, "building");
        assert_eq!(
            EpicRepository::new(db.clone(), bus.clone())
                .get(&epic_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "open"
        );
        for tid in &all_task_ids {
            assert_eq!(
                trepo.get(tid).await.unwrap().unwrap().status,
                "open",
                "preview must not mutate task {tid}"
            );
        }
        assert_eq!(
            prepo.graduated_epics(&pid).await.unwrap().len(),
            links_before
        );
        let activity_after = activity_count(&trepo, &all_task_ids).await;
        assert_eq!(
            activity_after, activity_before,
            "preview must not write activity_log rows"
        );
    }
}
