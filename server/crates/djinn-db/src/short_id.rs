use crate::database::Database;
use crate::error::{DbError, DbResult};
use sqlx::Row;

/// A resolved short_id mention pointing to a proposal, epic, or task.
#[derive(Clone, Debug)]
pub struct ResolvedEntity {
    pub short_id: String,
    pub entity_type: String,
    pub title: String,
    pub permalink: String,
}

/// Resolve a batch of short_ids against proposals, epics, and tasks.
///
/// Runs a single UNION ALL query across the three entity tables and returns
/// matches. Unresolvable short_ids are silently omitted.
pub async fn resolve_short_ids(db: &Database, ids: &[String]) -> DbResult<Vec<ResolvedEntity>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }

    db.ensure_initialized().await?;

    let rows = sqlx::query(
        r#"SELECT short_id, 'proposal' as entity_type, title, id as permalink FROM proposals WHERE short_id = ANY($1)
         UNION ALL
         SELECT short_id, 'epic' as entity_type, title, id as permalink FROM epics WHERE short_id = ANY($1)
         UNION ALL
         SELECT short_id, 'task' as entity_type, title, id as permalink FROM tasks WHERE short_id = ANY($1)"#,
    )
    .bind(ids)
    .fetch_all(db.pool())
    .await
    .map_err(DbError::from)?;

    Ok(rows
        .into_iter()
        .map(|row| ResolvedEntity {
            short_id: row.try_get("short_id").unwrap_or_default(),
            entity_type: row.try_get("entity_type").unwrap_or_default(),
            title: row.try_get("title").unwrap_or_default(),
            permalink: row.try_get("permalink").unwrap_or_default(),
        })
        .collect())
}
