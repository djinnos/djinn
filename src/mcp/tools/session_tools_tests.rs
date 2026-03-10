#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::db::repositories::session_message::SessionMessageRepository;
    use crate::db::repositories::task::TaskRepository;
    use crate::test_helpers::{
        create_test_app, create_test_db, create_test_epic, create_test_project, create_test_session,
        create_test_task, mcp_call_tool,
    };

    #[tokio::test]
    async fn session_list_returns_empty_for_task_with_no_sessions() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let task = create_test_task(&db, &project.id, &epic.id).await;
        let app = create_test_app();

        let payload = mcp_call_tool(
            &app,
            "session-tools-tests",
            "session_list",
            json!({"task_id": task.id, "project": project.path}),
        )
        .await;

        assert_eq!(payload.get("error").and_then(|v| v.as_str()), None);
        assert_eq!(payload.get("task_id").and_then(|v| v.as_str()), Some(task.id.as_str()));
        assert_eq!(
            payload
                .get("sessions")
                .and_then(|v| v.as_array())
                .map(|v| v.len()),
            Some(0)
        );
    }

    #[tokio::test]
    async fn session_list_returns_expected_fields_for_seeded_session() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let task = create_test_task(&db, &project.id, &epic.id).await;
        let session = create_test_session(&db, &project.id, &task.id).await;
        let app = create_test_app();

        let payload = mcp_call_tool(
            &app,
            "session-tools-tests",
            "session_list",
            json!({"task_id": task.id, "project": project.path}),
        )
        .await;

        assert_eq!(payload.get("error").and_then(|v| v.as_str()), None);
        let sessions = payload
            .get("sessions")
            .and_then(|v| v.as_array())
            .expect("sessions array");
        assert!(!sessions.is_empty());
        let first = &sessions[0];
        assert_eq!(first.get("id").and_then(|v| v.as_str()), Some(session.id.as_str()));
        assert_eq!(first.get("task_id").and_then(|v| v.as_str()), Some(task.id.as_str()));
        assert_eq!(first.get("agent_type").and_then(|v| v.as_str()), Some("worker"));
        assert_eq!(first.get("model_id").and_then(|v| v.as_str()), Some("test-model"));
        assert_eq!(first.get("status").and_then(|v| v.as_str()), Some("running"));
        assert!(first.get("tokens_in").and_then(|v| v.as_i64()).is_some());
        assert!(first.get("tokens_out").and_then(|v| v.as_i64()).is_some());
    }

    #[tokio::test]
    async fn session_active_returns_empty_arrays_when_no_running_sessions() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let app = create_test_app();

        let payload = mcp_call_tool(
            &app,
            "session-tools-tests",
            "session_active",
            json!({"project": project.path}),
        )
        .await;

        assert_eq!(payload.get("error").and_then(|v| v.as_str()), None);
        assert_eq!(
            payload
                .get("sessions")
                .and_then(|v| v.as_array())
                .map(|v| v.len()),
            Some(0)
        );
    }

    #[tokio::test]
    async fn session_show_returns_full_session_for_valid_id() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let task = create_test_task(&db, &project.id, &epic.id).await;
        let session = create_test_session(&db, &project.id, &task.id).await;
        let app = create_test_app();

        let payload = mcp_call_tool(
            &app,
            "session-tools-tests",
            "session_show",
            json!({"id": session.id, "project": project.path}),
        )
        .await;

        assert_eq!(payload.get("error").and_then(|v| v.as_str()), None);
        assert_eq!(payload.get("id").and_then(|v| v.as_str()), Some(session.id.as_str()));
        assert_eq!(payload.get("task_id").and_then(|v| v.as_str()), Some(task.id.as_str()));
        assert_eq!(payload.get("agent_type").and_then(|v| v.as_str()), Some("worker"));
        assert_eq!(payload.get("model_id").and_then(|v| v.as_str()), Some("test-model"));
        assert_eq!(payload.get("status").and_then(|v| v.as_str()), Some("running"));
        assert!(payload.get("started_at").and_then(|v| v.as_str()).is_some());
    }

    #[tokio::test]
    async fn session_show_returns_error_for_unknown_id() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let app = create_test_app();

        let payload = mcp_call_tool(
            &app,
            "session-tools-tests",
            "session_show",
            json!({"id": "00000000-0000-0000-0000-000000000000", "project": project.path}),
        )
        .await;

        assert!(payload
            .get("error")
            .and_then(|v| v.as_str())
            .is_some_and(|e| e.contains("session not found")));
    }

    #[tokio::test]
    async fn session_for_task_returns_empty_when_no_running_session() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let task = create_test_task(&db, &project.id, &epic.id).await;
        let app = create_test_app();

        let payload = mcp_call_tool(
            &app,
            "session-tools-tests",
            "session_for_task",
            json!({"task_id": task.id, "project": project.path}),
        )
        .await;

        assert_eq!(payload.get("ok").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(payload.get("task_id").and_then(|v| v.as_str()), Some(task.id.as_str()));
        assert_eq!(payload.get("session_id"), Some(&serde_json::Value::Null));
    }

    #[tokio::test]
    async fn session_for_task_returns_session_for_running_task() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let task = create_test_task(&db, &project.id, &epic.id).await;
        let session = create_test_session(&db, &project.id, &task.id).await;
        let app = create_test_app();

        let payload = mcp_call_tool(
            &app,
            "session-tools-tests",
            "session_for_task",
            json!({"task_id": task.id, "project": project.path}),
        )
        .await;

        assert_eq!(payload.get("ok").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(payload.get("task_id").and_then(|v| v.as_str()), Some(task.id.as_str()));
        assert_eq!(payload.get("session_id").and_then(|v| v.as_str()), Some(session.id.as_str()));
    }

    #[tokio::test]
    async fn task_timeline_returns_messages_grouped_by_session_in_chronological_order() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let task = create_test_task(&db, &project.id, &epic.id).await;
        let session_a = create_test_session(&db, &project.id, &task.id).await;
        let session_b = create_test_session(&db, &project.id, &task.id).await;

        let msg_repo = SessionMessageRepository::new(db.clone(), tokio::sync::broadcast::channel(8).0);
        msg_repo
            .append_message(
                &session_a.id,
                "assistant",
                r#"[{\"type\":\"text\",\"text\":\"a1\"}]"#,
                Some("2026-01-01T00:00:01Z"),
            )
            .await
            .expect("append a1");
        msg_repo
            .append_message(
                &session_b.id,
                "assistant",
                r#"[{\"type\":\"text\",\"text\":\"b1\"}]"#,
                Some("2026-01-01T00:00:02Z"),
            )
            .await
            .expect("append b1");

        let task_repo = TaskRepository::new(db.clone(), tokio::sync::broadcast::channel(8).0);
        task_repo
            .log_activity(
                Some(&task.id),
                "session-tools-tests",
                "worker",
                "status_changed",
                &json!({"from":"open","to":"in_progress"}).to_string(),
            )
            .await
            .expect("add activity");

        let app = create_test_app();
        let payload = mcp_call_tool(
            &app,
            "session-tools-tests",
            "task_timeline",
            json!({"task_id": task.id, "project": project.path}),
        )
        .await;

        assert_eq!(payload.get("error").and_then(|v| v.as_str()), None);
        let sessions = payload
            .get("sessions")
            .and_then(|v| v.as_array())
            .expect("sessions array");
        assert!(sessions.len() >= 2);

        let messages = payload
            .get("messages")
            .and_then(|v| v.as_array())
            .expect("messages array");
        assert_eq!(messages.len(), 2);
        assert!(messages[0].get("timestamp").and_then(|v| v.as_str()) <= messages[1].get("timestamp").and_then(|v| v.as_str()));

        let by_session: std::collections::HashMap<_, _> = messages
            .iter()
            .filter_map(|m| m.get("session_id").and_then(|v| v.as_str()).map(|sid| (sid.to_string(), m)))
            .collect();
        assert!(by_session.contains_key(&session_a.id));
        assert!(by_session.contains_key(&session_b.id));

        let activity = payload
            .get("activity")
            .and_then(|v| v.as_array())
            .expect("activity array");
        assert!(!activity.is_empty());
    }
}
