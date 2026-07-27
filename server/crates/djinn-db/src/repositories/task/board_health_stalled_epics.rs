//! Bounded board-health projection for open epics with no dispatchable work.

use sqlx::Row;

/// Open epics whose task graph has no coordinator-dispatchable work.
pub(super) async fn stalled_epics_section(pool: &sqlx::PgPool) -> serde_json::Value {
    let rows = sqlx::query(
        r#"SELECT e.id, e.short_id, e.title,
                  COALESCE(
                    (SELECT jsonb_agg(jsonb_build_object(
                        'id', t.id, 'short_id', t.short_id,
                        'issue_type', t.issue_type, 'status', t.status,
                        'pr_url', t.pr_url
                    ) ORDER BY t.created_at)
                       FROM tasks t WHERE t.epic_id = e.id),
                    '[]'::jsonb
                  ) AS tasks
             FROM epics e
            WHERE e.status = 'open'
              AND NOT EXISTS (
                SELECT 1 FROM tasks p
                 WHERE p.epic_id = e.id
                   AND p.issue_type IN ('planning', 'decomposition')
                   AND p.status IN ('open', 'in_progress')
              )
              AND EXISTS (
                SELECT 1 FROM tasks w
                 WHERE w.epic_id = e.id
                   AND w.issue_type NOT IN ('planning', 'decomposition')
              )
              AND NOT EXISTS (
                SELECT 1 FROM tasks w
                 WHERE w.epic_id = e.id
                   AND w.issue_type NOT IN ('planning', 'decomposition')
                   AND (
                     w.status NOT IN ('closed', 'pr_review')
                     AND NOT (
                       w.status = 'open' AND EXISTS (
                         SELECT 1
                           FROM blockers b
                           JOIN tasks blocker ON blocker.id = b.blocking_task_id
                          WHERE b.task_id = w.id
                            AND blocker.status <> 'closed'
                       )
                     )
                   )
              )
            ORDER BY e.created_at"#,
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    serde_json::json!({
        "total": rows.len(),
        "findings": rows.into_iter().map(|row| serde_json::json!({
            "id": row.get::<String, _>("id"),
            "short_id": row.get::<String, _>("short_id"),
            "title": row.get::<String, _>("title"),
            "tasks": row.get::<serde_json::Value, _>("tasks"),
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;

    async fn insert_task(
        db: &Database,
        project_id: &str,
        epic_id: &str,
        issue_type: &str,
        status: &str,
    ) -> String {
        let id = uuid::Uuid::now_v7().to_string();
        let compact_id = id.replace('-', "");
        let creator = crate::repositories::test_support::seed_test_user(db).await;
        sqlx::query(
            r#"INSERT INTO tasks (
                 id, project_id, short_id, epic_id, title, description, design,
                 issue_type, status, priority, owner, labels, acceptance_criteria,
                 memory_refs, created_by_user_id
               ) VALUES (
                 $1, $2, $3, $4, 'Task', '', '', $5, $6, 1, '',
                 '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, $7
               )"#,
        )
        .bind(&id)
        .bind(project_id)
        .bind(format!("t{}", &compact_id[compact_id.len() - 8..]))
        .bind(epic_id)
        .bind(issue_type)
        .bind(status)
        .bind(creator)
        .execute(db.pool())
        .await
        .unwrap();
        id
    }

    #[tokio::test]
    async fn reports_closed_planners_terminal_pr_and_blocked_dependent() {
        let db = Database::open_in_memory().unwrap();
        let project_id = uuid::Uuid::now_v7().to_string();
        crate::repositories::test_support::seed_project(&db, &project_id, "stalled").await;
        let epic_id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            r#"INSERT INTO epics (
                 id, project_id, short_id, title, description, emoji, color, owner,
                 status
               ) VALUES ($1, $2, '1woc', 'Retire verification', '', '', '', '', 'open')"#,
        )
        .bind(&epic_id)
        .bind(&project_id)
        .execute(db.pool())
        .await
        .unwrap();

        insert_task(&db, &project_id, &epic_id, "planning", "closed").await;
        insert_task(&db, &project_id, &epic_id, "planning", "closed").await;
        let exhausted = insert_task(&db, &project_id, &epic_id, "task", "pr_review").await;
        sqlx::query("UPDATE tasks SET pr_url = $2 WHERE id = $1")
            .bind(&exhausted)
            .bind("https://github.com/djinnos/djinn/pull/2655")
            .execute(db.pool())
            .await
            .unwrap();
        let dependent = insert_task(&db, &project_id, &epic_id, "task", "open").await;
        sqlx::query("INSERT INTO blockers (task_id, blocking_task_id) VALUES ($1, $2)")
            .bind(&dependent)
            .bind(&exhausted)
            .execute(db.pool())
            .await
            .unwrap();

        let section = stalled_epics_section(db.pool()).await;
        assert_eq!(section["total"], 1);
        assert_eq!(section["findings"][0]["id"], epic_id);

        // Neutralization: closing the blocker makes the dependent dispatchable.
        sqlx::query("UPDATE tasks SET status = 'closed' WHERE id = $1")
            .bind(&exhausted)
            .execute(db.pool())
            .await
            .unwrap();
        assert_eq!(stalled_epics_section(db.pool()).await["total"], 0);
    }
}
