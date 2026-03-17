use djinn_db::TaskRepository;
use crate::context::AgentContext;

pub(crate) const STALE_ESCALATION_THRESHOLD: i64 = 3;

/// Check if all acceptance criteria are met.
pub(crate) fn all_acceptance_criteria_met(ac_json: &str) -> bool {
    #[derive(serde::Deserialize)]
    struct Criterion {
        #[serde(default)]
        met: bool,
    }

    match serde_json::from_str::<Vec<Criterion>>(ac_json) {
        Ok(criteria) => !criteria.is_empty() && criteria.iter().all(|c| c.met),
        Err(_) => false,
    }
}

/// Returns true if the AC met-state is identical to the snapshot from when
/// the current review cycle started (i.e. the worker made no AC progress).
pub(crate) async fn is_stale_review_cycle(task_id: &str, current_ac_json: &str, app_state: &AgentContext) -> bool {
    let repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let snapshot_json = match repo.last_review_start_ac_snapshot(task_id).await {
        Ok(Some(s)) => s,
        _ => return false, // no snapshot → assume not stale
    };

    // Compare only the `met` booleans, not the full AC (description may differ).
    fn extract_met_pattern(json: &str) -> Vec<bool> {
        #[derive(serde::Deserialize)]
        struct Criterion {
            #[serde(default)]
            met: bool,
        }
        serde_json::from_str::<Vec<Criterion>>(json)
            .unwrap_or_default()
            .into_iter()
            .map(|c| c.met)
            .collect()
    }

    extract_met_pattern(current_ac_json) == extract_met_pattern(&snapshot_json)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ac(items: &[bool]) -> String {
        serde_json::to_string(
            &items
                .iter()
                .map(|met| serde_json::json!({"description": "x", "met": met}))
                .collect::<Vec<_>>(),
        )
        .expect("serialize AC json")
    }

    #[test]
    fn all_acceptance_criteria_met_cases() {
        assert!(!all_acceptance_criteria_met("[]"));
        assert!(all_acceptance_criteria_met(&ac(&[true, true])));
        assert!(!all_acceptance_criteria_met(&ac(&[true, false])));
        assert!(!all_acceptance_criteria_met("{not json}"));
        assert!(all_acceptance_criteria_met(&ac(&[true])));
        assert!(!all_acceptance_criteria_met(&ac(&[false, true, false])));
    }
}

#[cfg(test)]
mod transition_tests {
    use super::*;
    use crate::test_helpers;

    #[allow(dead_code)]
    async fn set_task_status(db: &djinn_db::Database, task_id: &str, status: &str) {
        sqlx::query("UPDATE tasks SET status = ?1 WHERE id = ?2")
            .bind(status)
            .bind(task_id)
            .execute(db.pool())
            .await
            .expect("update task status");
    }

    #[allow(dead_code)]
    async fn set_task_ac(db: &djinn_db::Database, task_id: &str, ac_json: &str) {
        sqlx::query("UPDATE tasks SET acceptance_criteria = ?1 WHERE id = ?2")
            .bind(ac_json)
            .bind(task_id)
            .execute(db.pool())
            .await
            .expect("update AC");
    }

    #[allow(dead_code)]
    async fn set_continuation_count(db: &djinn_db::Database, task_id: &str, count: i64) {
        sqlx::query("UPDATE tasks SET continuation_count = ?1 WHERE id = ?2")
            .bind(count)
            .bind(task_id)
            .execute(db.pool())
            .await
            .expect("update continuation_count");
    }

    async fn insert_review_snapshot(db: &djinn_db::Database, task_id: &str, ac_json: &str) {
        let payload = serde_json::json!({"to_status":"in_task_review","ac_snapshot":serde_json::from_str::<serde_json::Value>(ac_json).expect("valid ac json")}).to_string();
        sqlx::query("INSERT INTO activity_log (id, task_id, actor_id, actor_role, event_type, payload) VALUES (?1, ?2, 'test', 'system', 'status_changed', ?3)")
            .bind(uuid::Uuid::now_v7().to_string())
            .bind(task_id)
            .bind(payload)
            .execute(db.pool())
            .await
            .expect("insert snapshot");
    }

    fn ac(items: &[bool]) -> String {
        serde_json::to_string(
            &items
                .iter()
                .map(|met| serde_json::json!({"description": "x", "met": met}))
                .collect::<Vec<_>>(),
        )
        .expect("serialize AC json")
    }

    #[tokio::test]
    async fn is_stale_review_cycle_cases() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

        let same = ac(&[true, false]);
        insert_review_snapshot(&ctx.db, &task.id, &same).await;
        assert!(is_stale_review_cycle(&task.id, &same, &ctx).await);

        let progressed = ac(&[true, true]);
        assert!(!is_stale_review_cycle(&task.id, &progressed, &ctx).await);

        let task2 = test_helpers::create_test_task(&db, &project.id, &epic.id).await;
        assert!(!is_stale_review_cycle(&task2.id, &same, &ctx).await);

        let empty = "[]".to_string();
        insert_review_snapshot(&ctx.db, &task2.id, &empty).await;
        assert!(is_stale_review_cycle(&task2.id, &empty, &ctx).await);

        let task3 = test_helpers::create_test_task(&db, &project.id, &epic.id).await;
        let three = ac(&[true, false, true]);
        let five = ac(&[true, false, true, false, true]);
        insert_review_snapshot(&ctx.db, &task3.id, &three).await;
        assert!(!is_stale_review_cycle(&task3.id, &five, &ctx).await);
    }

}
