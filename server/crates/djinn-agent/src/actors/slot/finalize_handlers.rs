// Re-exports canonical finalize types and wraps the two async entry points
// with `AgentContext → SlotContext` adapter glue.

#[cfg(test)]
pub(crate) use djinn_slot::finalize_handlers::apply_ac_verdicts;
#[cfg(test)]
pub(crate) use djinn_slot::finalize_types::AcVerdict;

use crate::context::AgentContext;

/// Agent-compatible wrapper around `djinn_slot::finalize_handlers::process_finalize_payload`.
pub(crate) async fn process_finalize_payload(
    payload: &Option<serde_json::Value>,
    finalize_tool_name: &str,
    task_id: &str,
    app_state: &AgentContext,
) {
    crate::with_slot_context!(app_state, |slot_ctx| {
        djinn_slot::finalize_handlers::process_finalize_payload(
            payload,
            finalize_tool_name,
            task_id,
            slot_ctx,
        )
    });
}

pub(crate) async fn process_completion_intent(
    intent: &djinn_slot::output_parser::CompletionIntent,
    finalize_tool_name: &str,
    task_id: &str,
    app_state: &AgentContext,
) {
    crate::with_slot_context!(app_state, |slot_ctx| {
        djinn_slot::finalize_handlers::process_completion_intent_with_outcome(
            intent,
            finalize_tool_name,
            task_id,
            slot_ctx,
        )
    });
}

/// Agent-compatible wrapper around `djinn_slot::finalize_handlers::handle_budget_park`.
pub(crate) async fn handle_budget_park(
    summary: &str,
    details: &str,
    task_id: &str,
    app_state: &AgentContext,
) {
    crate::with_slot_context!(app_state, |slot_ctx| {
        djinn_slot::finalize_handlers::handle_budget_park(summary, details, task_id, slot_ctx)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers;
    use djinn_db::{Database, TaskRepository};
    struct FinalizeFixtures {
        db: Database,
        ctx: crate::context::AgentContext,
        task: djinn_core::models::Task,
    }
    impl FinalizeFixtures {
        async fn new() -> Self {
            let db = test_helpers::create_test_db();
            let ctx = test_helpers::agent_context_from_db(
                db.clone(),
                tokio_util::sync::CancellationToken::new(),
            );
            let project = test_helpers::create_test_project(&db).await;
            let epic = test_helpers::create_test_epic(&db, &project.id).await;
            let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;
            Self { db, ctx, task }
        }
        fn repo(&self) -> TaskRepository {
            TaskRepository::new(self.db.clone(), self.ctx.event_bus.clone())
        }
    }
    #[test]
    fn apply_ac_verdicts_sets_met_flags_from_payload() {
        let existing =
            r#"[{"criterion":"write tests","met":false},{"criterion":"passing ci","met":false}]"#;
        let verdicts = vec![
            AcVerdict {
                criterion: "write tests".to_string(),
                met: true,
            },
            AcVerdict {
                criterion: "passing ci".to_string(),
                met: true,
            },
        ];
        let result = apply_ac_verdicts(existing, &verdicts);
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed[0]["met"], true);
        assert_eq!(parsed[1]["met"], true);
    }
    #[test]
    fn apply_ac_verdicts_preserves_existing_criterion_text_when_empty() {
        let existing = r#"[{"criterion":"write tests","met":false}]"#;
        let verdicts = vec![AcVerdict {
            criterion: String::new(),
            met: true,
        }];
        let result = apply_ac_verdicts(existing, &verdicts);
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed[0]["criterion"], "write tests");
        assert_eq!(parsed[0]["met"], true);
    }
    #[test]
    fn apply_ac_verdicts_handles_empty_existing_gracefully() {
        let existing = "not-valid-json";
        let verdicts = vec![AcVerdict {
            criterion: "x".to_string(),
            met: false,
        }];
        let result = apply_ac_verdicts(existing, &verdicts);
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed[0]["criterion"], "x");
        assert_eq!(parsed[0]["met"], false);
    }
    #[tokio::test]
    async fn budget_park_logs_extractor_compatible_work_submitted() {
        let f = FinalizeFixtures::new().await;
        handle_budget_park(
            "completed A; B remains",
            "budget-triggered wind-down summary captured",
            &f.task.id,
            &f.ctx,
        )
        .await;
        let entries = f.repo().list_activity(&f.task.id).await.unwrap();
        let work_entries: Vec<_> = entries
            .iter()
            .filter(|e| e.event_type == "work_submitted")
            .collect();
        assert_eq!(work_entries.len(), 1);
        let body: serde_json::Value = serde_json::from_str(&work_entries[0].payload).unwrap();
        assert_eq!(body["summary"], "completed A; B remains");
        assert_eq!(
            body["remaining_concerns"],
            "budget-parked: budget-triggered wind-down summary captured"
        );
    }
    #[tokio::test]
    async fn budget_park_empty_summary_skips_activity() {
        let f = FinalizeFixtures::new().await;
        handle_budget_park("   ", "ignored", &f.task.id, &f.ctx).await;
        let entries = f.repo().list_activity(&f.task.id).await.unwrap();
        assert!(entries.iter().all(|e| e.event_type != "work_submitted"));
    }
    #[tokio::test]
    async fn submit_work_logs_activity_with_summary_and_files() {
        let f = FinalizeFixtures::new().await;
        let payload = Some(serde_json::json!({
            "task_id": f.task.short_id,
            "commit_title": "feat: implement the feature",
            "summary": "implemented the feature",
            "files_changed": ["src/main.rs", "src/lib.rs"],
            "remaining_concerns": ["needs perf testing"]
        }));
        process_finalize_payload(&payload, "submit_work", &f.task.id, &f.ctx).await;
        let entries = f.repo().list_activity(&f.task.id).await.unwrap();
        let work_entry = entries.iter().find(|e| e.event_type == "work_submitted");
        assert!(
            work_entry.is_some(),
            "expected work_submitted activity entry"
        );
        let body: serde_json::Value = serde_json::from_str(&work_entry.unwrap().payload).unwrap();
        assert_eq!(body["summary"], "implemented the feature");
        assert_eq!(body["files_changed"][0], "src/main.rs");
        assert_eq!(body["remaining_concerns"][0], "needs perf testing");
    }
    #[tokio::test]
    async fn budget_park_submit_work_activity_surfaces_unchanged() {
        let f = FinalizeFixtures::new().await;
        let payload = Some(serde_json::json!({
            "task_id": f.task.short_id,
            "commit_title": "park budget summary",
            "summary": "finished the safe subset before parking",
            "files_changed": ["src/lib.rs"],
            "remaining_concerns": ["budget-parked: finish the follow-up UI snapshot"]
        }));
        process_finalize_payload(&payload, "submit_work", &f.task.id, &f.ctx).await;
        let entries = f.repo().list_activity(&f.task.id).await.unwrap();
        let work_entry = entries
            .iter()
            .find(|entry| entry.event_type == "work_submitted")
            .expect("expected budget-park work_submitted activity entry");
        let body: serde_json::Value = serde_json::from_str(&work_entry.payload).unwrap();
        assert_eq!(body["summary"], "finished the safe subset before parking");
        assert_eq!(
            body["remaining_concerns"][0],
            "budget-parked: finish the follow-up UI snapshot"
        );
    }
    #[tokio::test]
    async fn submit_work_malformed_payload_does_not_crash() {
        let f = FinalizeFixtures::new().await;
        let payload = Some(serde_json::json!({"task_id": f.task.id}));
        process_finalize_payload(&payload, "submit_work", &f.task.id, &f.ctx).await;
    }
    #[tokio::test]
    async fn submit_review_atomically_sets_ac_from_criteria_array() {
        let f = FinalizeFixtures::new().await;
        TaskRepository::new(f.db.clone(), f.ctx.event_bus.clone())
            .set_acceptance_criteria(
                &f.task.id,
                r#"[{"criterion":"write tests","met":false},{"criterion":"passes ci","met":false}]"#,
            )
            .await
            .unwrap();
        let payload = Some(serde_json::json!({
            "task_id": f.task.id,
            "verdict": "approved",
            "acceptance_criteria": [
                {"criterion": "write tests", "met": true},
                {"criterion": "passes ci", "met": true}
            ],
            "feedback": null
        }));
        process_finalize_payload(&payload, "submit_review", &f.task.id, &f.ctx).await;
        let updated = f.repo().get(&f.task.id).await.unwrap().unwrap();
        let ac: Vec<serde_json::Value> =
            serde_json::from_str(&updated.acceptance_criteria).unwrap();
        assert_eq!(ac[0]["met"], true);
        assert_eq!(ac[1]["met"], true);
    }
    #[tokio::test]
    async fn submit_review_logs_verdict_activity() {
        let f = FinalizeFixtures::new().await;
        let payload = Some(serde_json::json!({
            "task_id": f.task.id,
            "verdict": "rejected",
            "acceptance_criteria": [],
            "feedback": "missing edge case handling"
        }));
        process_finalize_payload(&payload, "submit_review", &f.task.id, &f.ctx).await;
        let entries = f.repo().list_activity(&f.task.id).await.unwrap();
        let entry = entries.iter().find(|e| e.event_type == "review_submitted");
        assert!(entry.is_some(), "expected review_submitted activity entry");
        let body: serde_json::Value = serde_json::from_str(&entry.unwrap().payload).unwrap();
        assert_eq!(body["verdict"], "rejected");
        assert_eq!(body["feedback"], "missing edge case handling");
    }
    #[tokio::test]
    async fn submit_review_malformed_payload_does_not_crash() {
        let f = FinalizeFixtures::new().await;
        let payload = Some(serde_json::json!({"task_id": f.task.id}));
        process_finalize_payload(&payload, "submit_review", &f.task.id, &f.ctx).await;
    }
    #[tokio::test]
    async fn submit_decision_logs_decision_activity() {
        let f = FinalizeFixtures::new().await;
        let payload = Some(serde_json::json!({
            "task_id": f.task.id,
            "decision": "reopen",
            "rationale": "scope was too broad",
            "created_tasks": []
        }));
        process_finalize_payload(&payload, "submit_decision", &f.task.id, &f.ctx).await;
        let entries = f.repo().list_activity(&f.task.id).await.unwrap();
        let entry = entries
            .iter()
            .find(|e| e.event_type == "decision_submitted");
        assert!(
            entry.is_some(),
            "expected decision_submitted activity entry"
        );
        let body: serde_json::Value = serde_json::from_str(&entry.unwrap().payload).unwrap();
        assert_eq!(body["decision"], "reopen");
        assert_eq!(body["rationale"], "scope was too broad");
    }
    #[tokio::test]
    async fn submit_decision_malformed_payload_does_not_crash() {
        let f = FinalizeFixtures::new().await;
        let payload = Some(serde_json::json!({"task_id": f.task.id}));
        process_finalize_payload(&payload, "submit_decision", &f.task.id, &f.ctx).await;
    }
    #[tokio::test]
    async fn submit_grooming_logs_per_task_activity_entries() {
        let f = FinalizeFixtures::new().await;
        let project = test_helpers::create_test_project(&f.db).await;
        let epic = test_helpers::create_test_epic(&f.db, &project.id).await;
        let task1 = test_helpers::create_test_task(&f.db, &project.id, &epic.id).await;
        let task2 = test_helpers::create_test_task(&f.db, &project.id, &epic.id).await;
        let payload = Some(serde_json::json!({
            "tasks_reviewed": [
                {"task_id": task1.id, "action": "promoted", "changes": "bumped priority to 1"},
                {"task_id": task2.id, "action": "skipped", "changes": null}
            ],
            "summary": "groomed 2 tasks"
        }));
        let synthetic_id = format!("project:{}:planner", project.id);
        process_finalize_payload(&payload, "submit_grooming", &synthetic_id, &f.ctx).await;
        let repo = f.repo();
        let entries1 = repo.list_activity(&task1.id).await.unwrap();
        let e1 = entries1.iter().find(|e| e.event_type == "planning_entry");
        assert!(e1.is_some(), "expected planning_entry for task1");
        let b1: serde_json::Value = serde_json::from_str(&e1.unwrap().payload).unwrap();
        assert_eq!(b1["action"], "promoted");
        assert_eq!(b1["changes"], "bumped priority to 1");
        let entries2 = repo.list_activity(&task2.id).await.unwrap();
        let e2 = entries2.iter().find(|e| e.event_type == "planning_entry");
        assert!(e2.is_some(), "expected planning_entry for task2");
        let b2: serde_json::Value = serde_json::from_str(&e2.unwrap().payload).unwrap();
        assert_eq!(b2["action"], "skipped");
    }
    #[tokio::test]
    async fn submit_grooming_malformed_payload_does_not_crash() {
        let f = FinalizeFixtures::new().await;
        let payload = Some(serde_json::json!("not-an-object"));
        process_finalize_payload(&payload, "submit_grooming", "project:x:planner", &f.ctx).await;
    }
    #[tokio::test]
    async fn none_payload_is_a_noop() {
        let f = FinalizeFixtures::new().await;
        process_finalize_payload(&None, "submit_work", "any-task-id", &f.ctx).await;
    }
    #[tokio::test]
    async fn unknown_finalize_tool_is_a_noop() {
        let f = FinalizeFixtures::new().await;
        let payload = Some(serde_json::json!({"anything": "here"}));
        process_finalize_payload(&payload, "submit_unknown", "any-task-id", &f.ctx).await;
    }
}
