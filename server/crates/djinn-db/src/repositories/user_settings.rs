use std::collections::{HashMap, HashSet};

use djinn_core::models::{ModelLanes, UserSettings};

use crate::Result;
use crate::database::Database;

/// Parse the `model_lanes` TEXT column (a JSON object
/// `{ plan: [...], implement: [...], review: [...] }`) into the typed lanes.
/// `NULL`, empty, an all-empty object, or invalid JSON all read as `None`
/// (= "no explicit selection"), so a corrupt value degrades to the global
/// fallback rather than erroring the whole settings read.
fn parse_lanes(raw: Option<&str>) -> Option<ModelLanes> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    match serde_json::from_str::<ModelLanes>(raw) {
        Ok(lanes) if !lanes.is_empty() => Some(lanes),
        _ => None,
    }
}

/// Parse the `max_sessions` TEXT column (a JSON `{model_id: cap}` object).
/// Non-positive caps are dropped (0 is meaningless → treat as unset → default
/// 1 downstream). `NULL`, empty, `{}`, or invalid JSON all read as `None`.
fn parse_max_sessions(raw: Option<&str>) -> Option<HashMap<String, u32>> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    match serde_json::from_str::<HashMap<String, u32>>(raw) {
        Ok(m) => {
            let m: HashMap<String, u32> = m.into_iter().filter(|(_, c)| *c > 0).collect();
            if m.is_empty() { None } else { Some(m) }
        }
        Err(_) => None,
    }
}

pub struct UserSettingsRepository {
    db: Database,
}

impl UserSettingsRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Fetch the row for `user_id`, or `None` if the user has never set anything.
    /// Callers that want a defaults-baseline can use `get_or_default`.
    pub async fn get(&self, user_id: &str) -> Result<Option<UserSettings>> {
        self.db.ensure_initialized().await?;
        // `model_lanes` is a JSON-object TEXT column, so we read the raw string
        // with `query!` and parse it rather than letting `query_as!` try to
        // decode it directly.
        let row = sqlx::query!(
            r#"SELECT user_id, auto_approve_prs AS "auto_approve_prs!: bool",
                      model_lanes, max_sessions, created_at, updated_at
               FROM user_settings WHERE user_id = $1"#,
            user_id,
        )
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.map(|r| UserSettings {
            user_id: r.user_id,
            auto_approve_prs: r.auto_approve_prs,
            lanes: parse_lanes(r.model_lanes.as_deref()),
            max_sessions: parse_max_sessions(r.max_sessions.as_deref()),
            created_at: r.created_at,
            updated_at: r.updated_at,
        }))
    }

    /// Read-side convenience: never returns `None`. Callers downstream of
    /// pr_poller etc. don't want to branch on row existence — a missing row
    /// is semantically "all defaults off".
    pub async fn get_or_default(&self, user_id: &str) -> Result<UserSettings> {
        Ok(self
            .get(user_id)
            .await?
            .unwrap_or_else(|| UserSettings::defaults_for(user_id)))
    }

    /// Return user IDs of every user with `auto_approve_prs = true`, most
    /// recently updated first.
    ///
    /// Used by the PR poller's fallback approver path: when a task has no
    /// `created_by_user_id` (background-agent-spawned) we still want to
    /// auto-approve on behalf of an opted-in human. Phase 0 deployments
    /// typically have a single human user so this list has length 0 or 1.
    pub async fn list_users_with_auto_approve(&self) -> Result<Vec<String>> {
        self.db.ensure_initialized().await?;
        let rows = sqlx::query!(
            "SELECT user_id FROM user_settings \
             WHERE auto_approve_prs = TRUE \
             ORDER BY updated_at DESC",
        )
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows.into_iter().map(|r| r.user_id).collect())
    }

    /// Upsert the `auto_approve_prs` toggle. Returns the resulting row.
    pub async fn upsert_auto_approve_prs(
        &self,
        user_id: &str,
        value: bool,
    ) -> Result<UserSettings> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            r#"INSERT INTO user_settings (user_id, auto_approve_prs, created_at, updated_at)
             VALUES ($1, $2,
                     to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                     to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
             ON CONFLICT (user_id) DO UPDATE SET
                 auto_approve_prs = EXCLUDED.auto_approve_prs,
                 updated_at = EXCLUDED.updated_at"#,
            user_id,
            value,
        )
        .execute(self.db.pool())
        .await?;
        self.get(user_id).await?.ok_or_else(|| {
            crate::Error::Internal(format!(
                "user_settings row missing after upsert for {user_id}"
            ))
        })
    }

    /// Upsert the per-user, per-role model lanes. Each lane preserves order
    /// (= priority high→low). Pass all-empty lanes to clear the selection
    /// (stored as `{"plan":[],...}`, read back as `None` → global fallback).
    /// Returns the resulting row.
    pub async fn upsert_lanes(&self, user_id: &str, lanes: &ModelLanes) -> Result<UserSettings> {
        self.db.ensure_initialized().await?;
        let json = serde_json::to_string(lanes)
            .map_err(|e| crate::Error::Internal(format!("serialize user model lanes: {e}")))?;
        sqlx::query!(
            r#"INSERT INTO user_settings (user_id, auto_approve_prs, model_lanes, created_at, updated_at)
             VALUES ($1, FALSE, $2,
                     to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                     to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
             ON CONFLICT (user_id) DO UPDATE SET
                 model_lanes = EXCLUDED.model_lanes,
                 updated_at = EXCLUDED.updated_at"#,
            user_id,
            json,
        )
        .execute(self.db.pool())
        .await?;
        self.get(user_id).await?.ok_or_else(|| {
            crate::Error::Internal(format!(
                "user_settings row missing after upsert for {user_id}"
            ))
        })
    }

    /// Upsert the per-user, per-model concurrency caps (`{model_id: cap}`),
    /// stored as a JSON-object TEXT value. Pass an empty map to clear caps
    /// (stored as `{}`, read back as `None` → default 1 per model). Returns the
    /// resulting row.
    pub async fn upsert_max_sessions(
        &self,
        user_id: &str,
        max_sessions: &HashMap<String, u32>,
    ) -> Result<UserSettings> {
        self.db.ensure_initialized().await?;
        let json = serde_json::to_string(max_sessions)
            .map_err(|e| crate::Error::Internal(format!("serialize user max_sessions: {e}")))?;
        sqlx::query!(
            r#"INSERT INTO user_settings (user_id, auto_approve_prs, max_sessions, created_at, updated_at)
             VALUES ($1, FALSE, $2,
                     to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                     to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
             ON CONFLICT (user_id) DO UPDATE SET
                 max_sessions = EXCLUDED.max_sessions,
                 updated_at = EXCLUDED.updated_at"#,
            user_id,
            json,
        )
        .execute(self.db.pool())
        .await?;
        self.get(user_id).await?.ok_or_else(|| {
            crate::Error::Internal(format!(
                "user_settings row missing after upsert for {user_id}"
            ))
        })
    }

    /// Distinct union of every user's selected model ids across ALL lanes
    /// (order not meaningful here). The slot pool uses this to size capacity for
    /// the union of all users' per-user model selections, since per-user
    /// dispatch may run any of them. Skips users with no explicit selection.
    pub async fn all_selected_models(&self) -> Result<Vec<String>> {
        self.db.ensure_initialized().await?;
        let rows =
            sqlx::query!("SELECT model_lanes FROM user_settings WHERE model_lanes IS NOT NULL")
                .fetch_all(self.db.pool())
                .await?;
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for r in rows {
            if let Some(lanes) = parse_lanes(r.model_lanes.as_deref()) {
                for m in lanes.all_models() {
                    if seen.insert(m.clone()) {
                        out.push(m);
                    }
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;

    async fn seed_user(db: &Database, suffix: &str) -> String {
        db.ensure_initialized().await.unwrap();
        let id = uuid::Uuid::now_v7().to_string();
        let github_id: i64 = suffix.bytes().map(i64::from).sum::<i64>() + 1_000_000;
        let login = format!("user-{suffix}");
        sqlx::query!(
            "INSERT INTO users (id, github_id, github_login) VALUES ($1, $2, $3)",
            id,
            github_id,
            login,
        )
        .execute(db.pool())
        .await
        .unwrap();
        id
    }

    #[tokio::test]
    async fn get_returns_none_for_unknown_user() {
        let db = Database::open_in_memory().expect("in-memory db");
        let user_id = seed_user(&db, "none").await;
        let repo = UserSettingsRepository::new(db);
        assert!(repo.get(&user_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn get_or_default_returns_defaults_for_unknown_user() {
        let db = Database::open_in_memory().expect("in-memory db");
        let user_id = seed_user(&db, "default").await;
        let repo = UserSettingsRepository::new(db);
        let s = repo.get_or_default(&user_id).await.unwrap();
        assert_eq!(s.user_id, user_id);
        assert!(!s.auto_approve_prs);
    }

    #[tokio::test]
    async fn upsert_creates_then_updates() {
        let db = Database::open_in_memory().expect("in-memory db");
        let user_id = seed_user(&db, "upsert").await;
        let repo = UserSettingsRepository::new(db);

        let created = repo.upsert_auto_approve_prs(&user_id, true).await.unwrap();
        assert!(created.auto_approve_prs);

        let updated = repo.upsert_auto_approve_prs(&user_id, false).await.unwrap();
        assert!(!updated.auto_approve_prs);
        assert_eq!(updated.user_id, user_id);
    }

    #[tokio::test]
    async fn list_users_with_auto_approve_returns_only_opted_in_users() {
        let db = Database::open_in_memory().expect("in-memory db");
        let user_off = seed_user(&db, "off").await;
        let user_on_a = seed_user(&db, "on-a").await;
        let user_on_b = seed_user(&db, "on-b").await;
        let repo = UserSettingsRepository::new(db);

        repo.upsert_auto_approve_prs(&user_off, false)
            .await
            .unwrap();
        repo.upsert_auto_approve_prs(&user_on_a, true)
            .await
            .unwrap();
        repo.upsert_auto_approve_prs(&user_on_b, true)
            .await
            .unwrap();

        let ids = repo.list_users_with_auto_approve().await.unwrap();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&user_on_a));
        assert!(ids.contains(&user_on_b));
        assert!(!ids.contains(&user_off));
    }

    #[tokio::test]
    async fn fk_cascade_drops_settings_when_user_removed() {
        let db = Database::open_in_memory().expect("in-memory db");
        let user_id = seed_user(&db, "cascade").await;
        let repo = UserSettingsRepository::new(db.clone());
        repo.upsert_auto_approve_prs(&user_id, true).await.unwrap();

        sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(&user_id)
            .execute(db.pool())
            .await
            .unwrap();

        assert!(repo.get(&user_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn upsert_lanes_round_trips_and_clears() {
        let db = Database::open_in_memory().expect("in-memory db");
        let user_id = seed_user(&db, "models").await;
        let repo = UserSettingsRepository::new(db);

        assert!(repo.get_or_default(&user_id).await.unwrap().lanes.is_none());

        let lanes = ModelLanes {
            plan: vec![
                "openai/gpt-5.5".to_string(),
                "anthropic/claude-opus-4-7".to_string(),
            ],
            implement: vec!["openai/gpt-5.5".to_string()],
            review: vec!["anthropic/claude-opus-4-7".to_string()],
        };
        let row = repo.upsert_lanes(&user_id, &lanes).await.unwrap();
        assert_eq!(row.lanes.as_ref(), Some(&lanes));
        // Order is preserved per lane on read-back (priority high→low).
        assert_eq!(
            repo.get(&user_id).await.unwrap().unwrap().lanes.unwrap(),
            lanes
        );

        // Clearing stores all-empty lanes, which read back as None (→ global).
        let cleared = repo
            .upsert_lanes(&user_id, &ModelLanes::default())
            .await
            .unwrap();
        assert!(cleared.lanes.is_none());
    }

    #[tokio::test]
    async fn lanes_and_auto_approve_do_not_clobber_each_other() {
        let db = Database::open_in_memory().expect("in-memory db");
        let user_id = seed_user(&db, "coexist").await;
        let repo = UserSettingsRepository::new(db);

        repo.upsert_auto_approve_prs(&user_id, true).await.unwrap();
        repo.upsert_lanes(
            &user_id,
            &ModelLanes::from_flat(vec!["openai/gpt-5.5".to_string()]),
        )
        .await
        .unwrap();

        let s = repo.get(&user_id).await.unwrap().unwrap();
        assert!(
            s.auto_approve_prs,
            "lanes upsert must not clobber auto_approve_prs"
        );
        assert_eq!(s.lanes.unwrap().plan, vec!["openai/gpt-5.5".to_string()]);
    }

    #[tokio::test]
    async fn all_selected_models_unions_across_users_and_lanes() {
        let db = Database::open_in_memory().expect("in-memory db");
        let a = seed_user(&db, "u-a").await;
        let b = seed_user(&db, "u-b").await;
        let repo = UserSettingsRepository::new(db);

        repo.upsert_lanes(
            &a,
            &ModelLanes {
                plan: vec!["openai/gpt-5.5".to_string()],
                implement: vec!["x/y".to_string()],
                review: vec![],
            },
        )
        .await
        .unwrap();
        repo.upsert_lanes(
            &b,
            &ModelLanes {
                plan: vec!["x/y".to_string()],
                implement: vec![],
                review: vec!["anthropic/claude-opus-4-7".to_string()],
            },
        )
        .await
        .unwrap();

        let mut all = repo.all_selected_models().await.unwrap();
        all.sort();
        assert_eq!(
            all,
            vec![
                "anthropic/claude-opus-4-7".to_string(),
                "openai/gpt-5.5".to_string(),
                "x/y".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn upsert_max_sessions_round_trips_clears_and_coexists() {
        let db = Database::open_in_memory().expect("in-memory db");
        let user_id = seed_user(&db, "caps").await;
        let repo = UserSettingsRepository::new(db);

        assert!(
            repo.get_or_default(&user_id)
                .await
                .unwrap()
                .max_sessions
                .is_none()
        );

        let caps = HashMap::from([
            ("openai/gpt-5.5".to_string(), 2u32),
            ("fireworks-ai/kimi".to_string(), 3u32),
        ]);
        let row = repo.upsert_max_sessions(&user_id, &caps).await.unwrap();
        assert_eq!(
            row.max_sessions.as_ref().unwrap().get("openai/gpt-5.5"),
            Some(&2)
        );
        assert_eq!(
            row.max_sessions.as_ref().unwrap().get("fireworks-ai/kimi"),
            Some(&3)
        );

        // Coexists with the model lanes (independent column patch).
        repo.upsert_lanes(
            &user_id,
            &ModelLanes::from_flat(vec!["openai/gpt-5.5".to_string()]),
        )
        .await
        .unwrap();
        let s = repo.get(&user_id).await.unwrap().unwrap();
        assert_eq!(s.lanes.unwrap().plan, vec!["openai/gpt-5.5".to_string()]);
        assert_eq!(s.max_sessions.unwrap().get("openai/gpt-5.5"), Some(&2));

        // Caps of 0 are dropped on read (→ default downstream); empty clears.
        repo.upsert_max_sessions(&user_id, &HashMap::from([("x/y".to_string(), 0u32)]))
            .await
            .unwrap();
        assert!(
            repo.get(&user_id)
                .await
                .unwrap()
                .unwrap()
                .max_sessions
                .is_none()
        );
    }
}
