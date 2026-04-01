use crate::context::AgentContext;
use crate::roles::finalize::{AcVerdict, SubmitDecision, SubmitGrooming, SubmitReview, SubmitWork};
use djinn_db::TaskRepository;

/// Process the structured finalize tool payload captured by the reply loop (ADR-036).
///
/// Called from the task lifecycle after the reply loop exits cleanly. Logs structured
/// activity entries and performs side effects specific to each finalize tool:
/// - `submit_work`: logs work summary and files changed
/// - `submit_review`: atomically sets AC met/unmet state, logs verdict
/// - `submit_decision`: logs lead decision and rationale
/// - `submit_grooming`: logs per-task grooming entries
///
/// Silently no-ops if `payload` is `None` (session ended without a finalize tool call).
/// Malformed payloads are logged as warnings and do not crash the lifecycle.
pub(crate) async fn process_finalize_payload(
    payload: &Option<serde_json::Value>,
    finalize_tool_name: &str,
    task_id: &str,
    app_state: &AgentContext,
) {
    let Some(payload) = payload else { return };

    match finalize_tool_name {
        "submit_work" => handle_submit_work(payload, task_id, app_state).await,
        "submit_review" => handle_submit_review(payload, task_id, app_state).await,
        "submit_decision" => handle_submit_decision(payload, task_id, app_state).await,
        "submit_grooming" => handle_submit_grooming(payload, app_state).await,
        other => {
            tracing::debug!(
                finalize_tool = %other,
                "finalize_handlers: unrecognized finalize tool; skipping"
            );
        }
    }
}

/// Log structured work-submission activity for a worker session.
async fn handle_submit_work(payload: &serde_json::Value, task_id: &str, app_state: &AgentContext) {
    let work = match serde_json::from_value::<SubmitWork>(payload.clone()) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(
                task_id = %task_id,
                error = %e,
                "finalize_handlers: malformed submit_work payload"
            );
            return;
        }
    };

    let activity_payload = serde_json::json!({
        "commit_title": work.commit_title,
        "summary": work.summary,
        "files_changed": work.files_changed,
        "remaining_concerns": work.remaining_concerns,
    })
    .to_string();

    let repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    if let Err(e) = repo
        .log_activity(
            Some(task_id),
            "agent-supervisor",
            "worker",
            "work_submitted",
            &activity_payload,
        )
        .await
    {
        tracing::warn!(
            task_id = %task_id,
            error = %e,
            "finalize_handlers: failed to log submit_work activity"
        );
    }
}

/// Atomically set AC met/unmet on the task from the criteria array, then log the verdict.
async fn handle_submit_review(
    payload: &serde_json::Value,
    task_id: &str,
    app_state: &AgentContext,
) {
    let review = match serde_json::from_value::<SubmitReview>(payload.clone()) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                task_id = %task_id,
                error = %e,
                "finalize_handlers: malformed submit_review payload"
            );
            return;
        }
    };

    let repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());

    // Atomically set AC met/unmet state from the criteria array.
    if !review.acceptance_criteria.is_empty() {
        match repo.get(task_id).await {
            Ok(Some(task)) => {
                let ac_json =
                    apply_ac_verdicts(&task.acceptance_criteria, &review.acceptance_criteria);
                if let Err(e) = repo
                    .update(
                        task_id,
                        &task.title,
                        &task.description,
                        &task.design,
                        task.priority,
                        &task.owner,
                        &task.labels,
                        &ac_json,
                    )
                    .await
                {
                    tracing::warn!(
                        task_id = %task_id,
                        error = %e,
                        "finalize_handlers: failed to update AC from submit_review"
                    );
                }
            }
            Ok(None) => {
                tracing::warn!(
                    task_id = %task_id,
                    "finalize_handlers: task not found for AC update"
                );
            }
            Err(e) => {
                tracing::warn!(
                    task_id = %task_id,
                    error = %e,
                    "finalize_handlers: failed to load task for AC update"
                );
            }
        }
    }

    // Log verdict and feedback as structured activity.
    let activity_payload = serde_json::json!({
        "verdict": review.verdict,
        "feedback": review.feedback,
    })
    .to_string();

    if let Err(e) = repo
        .log_activity(
            Some(task_id),
            "agent-supervisor",
            "reviewer",
            "review_submitted",
            &activity_payload,
        )
        .await
    {
        tracing::warn!(
            task_id = %task_id,
            error = %e,
            "finalize_handlers: failed to log submit_review activity"
        );
    }
}

/// Merge incoming per-criterion verdicts into the task's existing AC JSON.
///
/// Uses index-based matching. If an incoming verdict is missing `criterion` text,
/// the existing criterion text at that index is preserved.
fn apply_ac_verdicts(existing_json: &str, verdicts: &[AcVerdict]) -> String {
    let existing: Vec<serde_json::Value> = serde_json::from_str(existing_json).unwrap_or_default();

    let merged: Vec<serde_json::Value> = verdicts
        .iter()
        .enumerate()
        .map(|(i, verdict)| {
            let criterion_text = if !verdict.criterion.is_empty() {
                verdict.criterion.clone()
            } else {
                existing
                    .get(i)
                    .and_then(|e| e.get("criterion"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            };
            serde_json::json!({
                "criterion": criterion_text,
                "met": verdict.met,
            })
        })
        .collect();

    serde_json::to_string(&merged).unwrap_or_else(|_| "[]".to_string())
}

/// Log lead decision as a structured activity entry.
async fn handle_submit_decision(
    payload: &serde_json::Value,
    task_id: &str,
    app_state: &AgentContext,
) {
    let decision = match serde_json::from_value::<SubmitDecision>(payload.clone()) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(
                task_id = %task_id,
                error = %e,
                "finalize_handlers: malformed submit_decision payload"
            );
            return;
        }
    };

    let activity_payload = serde_json::json!({
        "decision": decision.decision,
        "rationale": decision.rationale,
        "created_tasks": decision.created_tasks,
    })
    .to_string();

    let repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    if let Err(e) = repo
        .log_activity(
            Some(task_id),
            "agent-supervisor",
            "lead",
            "decision_submitted",
            &activity_payload,
        )
        .await
    {
        tracing::warn!(
            task_id = %task_id,
            error = %e,
            "finalize_handlers: failed to log submit_decision activity"
        );
    }
}

/// Log per-task planning activity entries.
///
/// The planner is project-scoped, so `task_id` is a synthetic project identifier.
/// Each `tasks_reviewed` entry references a real task by its own `task_id` field.
async fn handle_submit_grooming(payload: &serde_json::Value, app_state: &AgentContext) {
    let grooming = match serde_json::from_value::<SubmitGrooming>(payload.clone()) {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "finalize_handlers: malformed submit_grooming payload"
            );
            return;
        }
    };

    let repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    for entry in &grooming.tasks_reviewed {
        let activity_payload = serde_json::json!({
            "action": entry.action,
            "changes": entry.changes,
        })
        .to_string();

        if let Err(e) = repo
            .log_activity(
                Some(&entry.task_id),
                "agent-supervisor",
                "planner",
                "planning_entry",
                &activity_payload,
            )
            .await
        {
            tracing::warn!(
                task_id = %entry.task_id,
                error = %e,
                "finalize_handlers: failed to log planning_entry activity"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers;

    // ── apply_ac_verdicts ────────────────────────────────────────────────────

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

    // ── process_finalize_payload: submit_work ────────────────────────────────

    #[tokio::test]
    async fn submit_work_logs_activity_with_summary_and_files() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

        let payload = Some(serde_json::json!({
            "task_id": task.short_id,
            "summary": "implemented the feature",
            "files_changed": ["src/main.rs", "src/lib.rs"],
            "remaining_concerns": ["needs perf testing"]
        }));

        process_finalize_payload(&payload, "submit_work", &task.id, &ctx).await;

        let repo = TaskRepository::new(db.clone(), ctx.event_bus.clone());
        let entries = repo.list_activity(&task.id).await.unwrap();
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
    async fn submit_work_malformed_payload_does_not_crash() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

        // Missing required "summary" field.
        let payload = Some(serde_json::json!({"task_id": task.id}));
        // Should not panic.
        process_finalize_payload(&payload, "submit_work", &task.id, &ctx).await;
    }

    // ── process_finalize_payload: submit_review ──────────────────────────────

    #[tokio::test]
    async fn submit_review_atomically_sets_ac_from_criteria_array() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

        // Seed AC with met=false.
        sqlx::query("UPDATE tasks SET acceptance_criteria = ?1 WHERE id = ?2")
            .bind(r#"[{"criterion":"write tests","met":false},{"criterion":"passes ci","met":false}]"#)
            .bind(&task.id)
            .execute(db.pool())
            .await
            .unwrap();

        let payload = Some(serde_json::json!({
            "task_id": task.id,
            "verdict": "approved",
            "acceptance_criteria": [
                {"criterion": "write tests", "met": true},
                {"criterion": "passes ci", "met": true}
            ],
            "feedback": null
        }));

        process_finalize_payload(&payload, "submit_review", &task.id, &ctx).await;

        // AC should be updated in the DB.
        let repo = TaskRepository::new(db.clone(), ctx.event_bus.clone());
        let updated = repo.get(&task.id).await.unwrap().unwrap();
        let ac: Vec<serde_json::Value> =
            serde_json::from_str(&updated.acceptance_criteria).unwrap();
        assert_eq!(ac[0]["met"], true);
        assert_eq!(ac[1]["met"], true);
    }

    #[tokio::test]
    async fn submit_review_logs_verdict_activity() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

        let payload = Some(serde_json::json!({
            "task_id": task.id,
            "verdict": "rejected",
            "acceptance_criteria": [],
            "feedback": "missing edge case handling"
        }));

        process_finalize_payload(&payload, "submit_review", &task.id, &ctx).await;

        let repo = TaskRepository::new(db.clone(), ctx.event_bus.clone());
        let entries = repo.list_activity(&task.id).await.unwrap();
        let entry = entries.iter().find(|e| e.event_type == "review_submitted");
        assert!(entry.is_some(), "expected review_submitted activity entry");

        let body: serde_json::Value = serde_json::from_str(&entry.unwrap().payload).unwrap();
        assert_eq!(body["verdict"], "rejected");
        assert_eq!(body["feedback"], "missing edge case handling");
    }

    #[tokio::test]
    async fn submit_review_malformed_payload_does_not_crash() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

        // "verdict" is required but missing.
        let payload = Some(serde_json::json!({"task_id": task.id}));
        process_finalize_payload(&payload, "submit_review", &task.id, &ctx).await;
    }

    // ── process_finalize_payload: submit_decision ────────────────────────────

    #[tokio::test]
    async fn submit_decision_logs_decision_activity() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

        let payload = Some(serde_json::json!({
            "task_id": task.id,
            "decision": "reopen",
            "rationale": "scope was too broad",
            "created_tasks": []
        }));

        process_finalize_payload(&payload, "submit_decision", &task.id, &ctx).await;

        let repo = TaskRepository::new(db.clone(), ctx.event_bus.clone());
        let entries = repo.list_activity(&task.id).await.unwrap();
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
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

        // "decision" is required but missing.
        let payload = Some(serde_json::json!({"task_id": task.id}));
        process_finalize_payload(&payload, "submit_decision", &task.id, &ctx).await;
    }

    // ── process_finalize_payload: submit_grooming ────────────────────────────

    #[tokio::test]
    async fn submit_grooming_logs_per_task_activity_entries() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task1 = test_helpers::create_test_task(&db, &project.id, &epic.id).await;
        let task2 = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

        let payload = Some(serde_json::json!({
            "tasks_reviewed": [
                {"task_id": task1.id, "action": "promoted", "changes": "bumped priority to 1"},
                {"task_id": task2.id, "action": "skipped", "changes": null}
            ],
            "summary": "groomed 2 tasks"
        }));

        // Planner is project-scoped; pass synthetic task_id.
        let synthetic_id = format!("project:{}:planner", project.id);
        process_finalize_payload(&payload, "submit_grooming", &synthetic_id, &ctx).await;

        let repo = TaskRepository::new(db.clone(), ctx.event_bus.clone());

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
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );

        // tasks_reviewed items missing required "action" field — SubmitGrooming itself
        // has tasks_reviewed as #[serde(default)] Vec, so malformed items are the issue.
        // Since tasks_reviewed has #[serde(default)], this will succeed with empty vec.
        // Test a completely invalid payload type instead.
        let payload = Some(serde_json::json!("not-an-object"));
        process_finalize_payload(&payload, "submit_grooming", "project:x:planner", &ctx).await;
    }

    // ── no-op cases ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn none_payload_is_a_noop() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        // Should not panic or error.
        process_finalize_payload(&None, "submit_work", "any-task-id", &ctx).await;
    }

    #[tokio::test]
    async fn unknown_finalize_tool_is_a_noop() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let payload = Some(serde_json::json!({"anything": "here"}));
        process_finalize_payload(&payload, "submit_unknown", "any-task-id", &ctx).await;
    }
}
