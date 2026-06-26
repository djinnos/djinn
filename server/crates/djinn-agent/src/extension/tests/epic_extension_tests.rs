use super::*;

#[tokio::test]
async fn epic_extension_handlers_match_shared_epic_ops_behavior() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let _project_path = crate::extension::tests::project_fs_path(&project)
        .to_string_lossy()
        .into_owned();
    let epic_repo = EpicRepository::new(db.clone(), EventBus::noop());
    let epic = epic_repo
        .update(
            &create_test_epic(&db, &project.id).await.id,
            djinn_db::EpicUpdateInput {
                title: "test-epic",
                description: "test epic description",
                emoji: "🧪",
                color: "#0000ff",
                owner: "test-owner",
                memory_refs: Some("[]"),
                status: None,
                auto_breakdown: None,
                originating_adr_id: None,
                blocked_by: None,
            },
        )
        .await
        .expect("normalize test epic color");
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let state = agent_context_from_db(db, CancellationToken::new());

    let show_args = Some(
        serde_json::json!({
            "project": project.slug(),
            "id": epic.short_id,
        })
        .as_object()
        .expect("show args object")
        .clone(),
    );
    let show_value = call_epic_show(&state, &show_args, None)
        .await
        .expect("epic_show succeeds");
    assert_eq!(show_value["id"], epic.id);
    assert_eq!(show_value["task_count"], serde_json::json!(1));
    assert!(show_value.get("error").is_none());

    let update_args = Some(
        serde_json::json!({
            "project": project.slug(),
            "id": epic.short_id,
            "title": "updated epic title",
            "description": "updated epic description",
            "status": "open",
            "memory_refs_add": ["notes/adr-041"],
        })
        .as_object()
        .expect("update args object")
        .clone(),
    );
    let update_value = call_epic_update(&state, &update_args, None)
        .await
        .expect("epic_update succeeds");
    let epic_model: djinn_control_plane::tools::epic_ops::EpicSingleResponse =
        serde_json::from_value(update_value.clone()).expect("parse epic update response");
    let epic_model = epic_model.epic.expect("updated epic payload");
    assert_eq!(epic_model.title, "updated epic title");
    assert_eq!(epic_model.description, "updated epic description");
    assert_eq!(epic_model.memory_refs, vec!["notes/adr-041".to_string()]);
    assert!(update_value.get("error").is_none());

    let tasks_args = Some(
        serde_json::json!({
            "project": project.slug(),
            "id": epic.short_id,
            "limit": 10,
            "offset": 0,
        })
        .as_object()
        .expect("tasks args object")
        .clone(),
    );
    let tasks_value = call_epic_tasks(&state, &tasks_args, None)
        .await
        .expect("epic_tasks succeeds");
    assert_eq!(tasks_value["total"], serde_json::json!(1));
    assert_eq!(tasks_value["limit"], serde_json::json!(10));
    assert_eq!(tasks_value["offset"], serde_json::json!(0));
    assert_eq!(tasks_value["has_more"], serde_json::json!(false));
    assert_eq!(tasks_value["tasks"][0]["id"], task.id);
    assert!(tasks_value.get("total_count").is_none());
    assert!(tasks_value.get("error").is_none());
}

#[tokio::test]
async fn proposal_ac_amend_validates_and_uses_repository_primitive() {
    let db = create_test_db();
    let proposal_repo = djinn_db::ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal = proposal_repo
        .create(djinn_db::ProposalCreateInput {
            title: "amendable proposal",
            body: "body",
            acceptance_criteria: Some(
                r#"[{"criterion":"Old text","met":false},{"criterion":"Drop me","met":true},{"criterion":"Waive me","met":false}]"#,
            ),
            status: Some("building"),
            body_format: None,
        })
        .await
        .expect("create proposal");
    let state = agent_context_from_db(db.clone(), CancellationToken::new());

    let missing_reason = Some(
        serde_json::json!({
            "id": proposal.short_id,
            "reason": "   ",
            "amendments": [{"index": 0, "operation": "rewrite", "criterion": "New text"}],
        })
        .as_object()
        .expect("args object")
        .clone(),
    );
    let err = call_proposal_ac_amend(&state, &missing_reason)
        .await
        .expect_err("blank reason rejected");
    assert!(err.contains("non-empty reason"));

    let omitted_reason = Some(
        serde_json::json!({
            "id": proposal.short_id,
            "amendments": [{"index": 0, "operation": "rewrite", "criterion": "New text"}],
        })
        .as_object()
        .expect("args object")
        .clone(),
    );
    let err = call_proposal_ac_amend(&state, &omitted_reason)
        .await
        .expect_err("missing reason rejected");
    assert!(err.contains("non-empty reason"));

    let invalid_operation = Some(
        serde_json::json!({
            "id": proposal.short_id,
            "reason": "make verifiable",
            "amendments": [{"index": 0, "operation": "replace", "criterion": "New text"}],
        })
        .as_object()
        .expect("args object")
        .clone(),
    );
    let err = call_proposal_ac_amend(&state, &invalid_operation)
        .await
        .expect_err("invalid operation rejected");
    assert!(err.contains("invalid operation `replace`"));

    let missing_criterion = Some(
        serde_json::json!({
            "id": proposal.id,
            "reason": "make verifiable",
            "amendments": [{"index": 0, "operation": "rewrite"}],
        })
        .as_object()
        .expect("args object")
        .clone(),
    );
    let err = call_proposal_ac_amend(&state, &missing_criterion)
        .await
        .expect_err("rewrite text required");
    assert!(err.contains("requires non-empty `criterion`"));

    let empty_criterion = Some(
        serde_json::json!({
            "id": proposal.id,
            "reason": "make verifiable",
            "amendments": [{"index": 0, "operation": "rewrite", "criterion": "   "}],
        })
        .as_object()
        .expect("args object")
        .clone(),
    );
    let err = call_proposal_ac_amend(&state, &empty_criterion)
        .await
        .expect_err("blank rewrite text rejected");
    assert!(err.contains("requires non-empty `criterion`"));

    let invalid_index = Some(
        serde_json::json!({
            "id": proposal.short_id,
            "reason": "criterion is no longer relevant",
            "amendments": [{"index": 99, "operation": "drop"}],
        })
        .as_object()
        .expect("args object")
        .clone(),
    );
    let err = call_proposal_ac_amend(&state, &invalid_index)
        .await
        .expect_err("out of range index rejected");
    assert!(err.contains("acceptance-criteria index 99 out of range"));

    let unchanged = proposal_repo
        .get(&proposal.id)
        .await
        .expect("reload proposal after validation failures")
        .expect("proposal exists");
    assert_eq!(unchanged.latest_revision_seq, 1);
    assert_eq!(unchanged.acceptance_criteria, proposal.acceptance_criteria);

    let amend_args = Some(
        serde_json::json!({
            "id": proposal.short_id,
            "reason": "old text was unverifiable; duplicate of prior AC; external-only proof is not agent-verifiable",
            "amendments": [
                {"index": 0, "operation": "rewrite", "criterion": "New verifiable text"},
                {"index": 1, "operation": "drop"},
                {"index": 1, "operation": "waive"}
            ],
        })
        .as_object()
        .expect("args object")
        .clone(),
    );
    let response = call_proposal_ac_amend(&state, &amend_args)
        .await
        .expect("amend succeeds");
    assert_eq!(response["ok"], serde_json::json!(true));
    assert_eq!(response["latest_revision_seq"], serde_json::json!(2));
    assert_eq!(response["acceptance_criteria_count"], serde_json::json!(2));

    let updated = proposal_repo
        .get(&proposal.id)
        .await
        .expect("reload proposal")
        .expect("proposal exists");
    assert_eq!(updated.latest_revision_seq, 2);
    let criteria: Vec<serde_json::Value> = serde_json::from_str(&updated.acceptance_criteria)
        .expect("updated acceptance criteria json");
    assert_eq!(criteria.len(), 2);
    assert_eq!(criteria[0]["criterion"], "New verifiable text");
    assert_eq!(criteria[1]["criterion"], "Waive me");
    assert_eq!(criteria[1]["waived"], true);
}

#[tokio::test]
async fn proposal_ac_set_stays_status_only_without_revision_bump() {
    let db = create_test_db();
    let proposal_repo = djinn_db::ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal = proposal_repo
        .create(djinn_db::ProposalCreateInput {
            title: "status-only proposal",
            body: "body",
            acceptance_criteria: Some(
                r#"[{"criterion":"Keep text","met":false},{"criterion":"Also keep","met":false}]"#,
            ),
            status: Some("building"),
            body_format: None,
        })
        .await
        .expect("create proposal");
    let state = agent_context_from_db(db.clone(), CancellationToken::new());

    let args = Some(
        serde_json::json!({
            "id": proposal.short_id,
            "acceptance_criteria": [{"met": true}, {"met": false}],
        })
        .as_object()
        .expect("args object")
        .clone(),
    );
    let response = call_proposal_ac_set(&state, &args)
        .await
        .expect("set succeeds");
    assert_eq!(response["met"], serde_json::json!(1));
    assert_eq!(response["total"], serde_json::json!(2));

    let updated = proposal_repo
        .get(&proposal.id)
        .await
        .expect("reload proposal")
        .expect("proposal exists");
    assert_eq!(updated.latest_revision_seq, 1);
    let criteria: Vec<serde_json::Value> = serde_json::from_str(&updated.acceptance_criteria)
        .expect("updated acceptance criteria json");
    assert_eq!(criteria[0]["criterion"], "Keep text");
    assert_eq!(criteria[0]["met"], true);
    assert_eq!(criteria[1]["criterion"], "Also keep");
}

#[tokio::test]
async fn proposal_ac_set_records_successful_reconcile_for_graduated_epics() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let proposal_repo = djinn_db::ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal = proposal_repo
        .create(djinn_db::ProposalCreateInput {
            title: "reconcile proposal",
            body: "body",
            acceptance_criteria: Some(r#"[{"criterion":"Ship it","met":false}]"#),
            status: Some("approved"),
            body_format: None,
        })
        .await
        .expect("create proposal");
    proposal_repo
        .link_epic(&proposal.id, &epic.id, &project.id)
        .await
        .expect("link graduated epic");
    proposal_repo
        .set_building(&proposal.id, "builder")
        .await
        .expect("mark building");
    let drifted = proposal_repo
        .update(
            &proposal.id,
            djinn_db::ProposalUpdateInput {
                title: "reconcile proposal v2",
                body: "body v2",
                acceptance_criteria: r#"[{"criterion":"Ship it better","met":false}]"#,
                status: "building",
                superseded_by: None,
                body_format: Some("markdown"),
                event_metadata: None,
            },
        )
        .await
        .expect("amend while building");
    assert_eq!(drifted.latest_revision_seq, 2);
    assert!(drifted.pending_reconcile);

    let state = agent_context_from_db(db.clone(), CancellationToken::new());
    let args = Some(
        serde_json::json!({
            "id": proposal.short_id,
            "acceptance_criteria": [{"met": true}],
        })
        .as_object()
        .expect("args object")
        .clone(),
    );
    let response = call_proposal_ac_set(&state, &args)
        .await
        .expect("set succeeds");
    assert_eq!(response["ok"], serde_json::json!(true));
    assert_eq!(response["met"], serde_json::json!(1));

    let reconciled = proposal_repo
        .get(&proposal.id)
        .await
        .expect("reload proposal")
        .expect("proposal exists");
    assert_eq!(reconciled.last_reconciled_revision_seq, Some(2));
    assert!(!reconciled.pending_reconcile);
    let latest_by_epic = proposal_repo
        .latest_epic_reconciliations(&proposal.id)
        .await
        .expect("latest epic reconciliations");
    assert_eq!(latest_by_epic.get(&epic.id), Some(&2));
}

#[tokio::test]
async fn proposal_reconcile_obsolete_epic_then_ac_set_preserves_unrelated_epics() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let obsolete_epic = create_test_epic(&db, &project.id).await;
    let preserved_epic = create_test_epic(&db, &project.id).await;
    let obsolete_task = create_test_task(&db, &project.id, &obsolete_epic.id).await;
    let preserved_task = create_test_task(&db, &project.id, &preserved_epic.id).await;
    let proposal_repo = djinn_db::ProposalRepository::new(db.clone(), EventBus::noop());
    let task_repo = djinn_db::TaskRepository::new(db.clone(), EventBus::noop());
    let epic_repo = djinn_db::EpicRepository::new(db.clone(), EventBus::noop());
    let proposal = proposal_repo
        .create(djinn_db::ProposalCreateInput {
            title: "obsolete reconcile proposal",
            body: "body",
            acceptance_criteria: Some(r#"[{"criterion":"Ship revised scope","met":false}]"#),
            status: Some("approved"),
            body_format: None,
        })
        .await
        .expect("create proposal");
    proposal_repo
        .link_epic(&proposal.id, &obsolete_epic.id, &project.id)
        .await
        .expect("link obsolete epic");
    proposal_repo
        .link_epic(&proposal.id, &preserved_epic.id, &project.id)
        .await
        .expect("link preserved epic");
    proposal_repo
        .set_building(&proposal.id, "builder")
        .await
        .expect("mark building");
    let drifted = proposal_repo
        .update(
            &proposal.id,
            djinn_db::ProposalUpdateInput {
                title: "obsolete reconcile proposal v2",
                body: "body v2",
                acceptance_criteria: r#"[{"criterion":"Ship revised scope","met":false}]"#,
                status: "building",
                superseded_by: None,
                body_format: Some("markdown"),
                event_metadata: None,
            },
        )
        .await
        .expect("amend while building");
    assert_eq!(drifted.latest_revision_seq, 2);
    assert!(drifted.pending_reconcile);

    let state = agent_context_from_db(db.clone(), CancellationToken::new());
    let teardown_args = Some(
        serde_json::json!({
            "proposal_id": proposal.short_id,
            "epic_id": obsolete_epic.short_id,
            "reason": "amended proposal removed this work",
        })
        .as_object()
        .expect("teardown args object")
        .clone(),
    );
    let teardown = call_proposal_reconcile_obsolete_epic(&state, &teardown_args)
        .await
        .expect("obsolete teardown succeeds");
    assert_eq!(teardown["ok"], serde_json::json!(true));
    assert_eq!(teardown["blocked"], serde_json::json!(false));

    let closed_obsolete_task = task_repo
        .get(&obsolete_task.id)
        .await
        .expect("load obsolete task")
        .expect("obsolete task exists");
    assert_eq!(closed_obsolete_task.status, "closed");
    let untouched_preserved_task = task_repo
        .get(&preserved_task.id)
        .await
        .expect("load preserved task")
        .expect("preserved task exists");
    assert_ne!(untouched_preserved_task.status, "closed");
    let closed_obsolete_epic = epic_repo
        .get(&obsolete_epic.id)
        .await
        .expect("load obsolete epic")
        .expect("obsolete epic exists");
    assert_eq!(closed_obsolete_epic.status, "closed");
    let untouched_preserved_epic = epic_repo
        .get(&preserved_epic.id)
        .await
        .expect("load preserved epic")
        .expect("preserved epic exists");
    assert_ne!(untouched_preserved_epic.status, "closed");
    let linked = proposal_repo
        .graduated_epics(&proposal.id)
        .await
        .expect("list linked epics");
    assert!(
        !linked
            .iter()
            .any(|(epic_id, _)| epic_id == &obsolete_epic.id)
    );
    assert!(
        linked
            .iter()
            .any(|(epic_id, _)| epic_id == &preserved_epic.id)
    );

    let ac_args = Some(
        serde_json::json!({
            "id": proposal.short_id,
            "acceptance_criteria": [{"met": true}],
        })
        .as_object()
        .expect("ac args object")
        .clone(),
    );
    let ac_response = call_proposal_ac_set(&state, &ac_args)
        .await
        .expect("proposal_ac_set succeeds");
    assert_eq!(ac_response["ok"], serde_json::json!(true));

    let reconciled = proposal_repo
        .get(&proposal.id)
        .await
        .expect("reload proposal")
        .expect("proposal exists");
    assert_eq!(reconciled.last_reconciled_revision_seq, Some(2));
    assert!(!reconciled.pending_reconcile);
    let latest_by_epic = proposal_repo
        .latest_epic_reconciliations(&proposal.id)
        .await
        .expect("latest epic reconciliations");
    assert_eq!(latest_by_epic.get(&preserved_epic.id), Some(&2));
    assert!(!latest_by_epic.contains_key(&obsolete_epic.id));
}
