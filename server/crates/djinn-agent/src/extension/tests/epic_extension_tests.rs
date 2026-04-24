use super::*;

#[tokio::test]
async fn epic_extension_handlers_match_shared_epic_ops_behavior() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let _project_path = crate::extension::tests::project_fs_path(&project).to_string_lossy().into_owned();
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
