//! Org-wide AI policy store (admin-owned singleton).
//!
//! Backs the admin "AI Policy" surface: subscription allow/block, org-default
//! per-role lanes, and the lane lock level. Persisted as a single row in
//! `org_ai_policy` (migration 79). The JSON-array/object columns hold their
//! values as TEXT, mirroring the per-user `user_settings` storage style.

use djinn_core::models::{LockLevel, OrgAiPolicy, OrgDefaultLanes};

use crate::Result;
use crate::database::Database;

/// Parse a JSON string-array TEXT column (e.g. `blocked_subscriptions` or
/// recommended-model override lists). `NULL`/empty/invalid JSON all read as an
/// empty list.
fn parse_string_array(raw: &str) -> Vec<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Vec::new();
    }
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_default()
}

/// Parse the `default_lanes` TEXT column (a JSON object). Invalid/empty reads as
/// the all-empty default (= no org default assignment).
fn parse_default_lanes(raw: &str) -> OrgDefaultLanes {
    let raw = raw.trim();
    if raw.is_empty() {
        return OrgDefaultLanes::default();
    }
    serde_json::from_str::<OrgDefaultLanes>(raw).unwrap_or_default()
}

pub struct OrgAiPolicyRepository {
    db: Database,
}

impl OrgAiPolicyRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Read the singleton policy. The row is seeded by migration 79, so this
    /// always returns a value; a missing row degrades to all-defaults rather
    /// than erroring.
    pub async fn get(&self) -> Result<OrgAiPolicy> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query!(
            r#"SELECT blocked_subscriptions, default_lanes, lock_level,
                      additional_recommended_model_ids,
                      demoted_recommended_model_ids,
                      updated_at
               FROM org_ai_policy WHERE id = 1"#,
        )
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row
            .map(|r| OrgAiPolicy {
                blocked_subscriptions: parse_string_array(&r.blocked_subscriptions),
                default_lanes: parse_default_lanes(&r.default_lanes),
                lock_level: LockLevel::from_db(&r.lock_level),
                additional_recommended_model_ids: parse_string_array(
                    &r.additional_recommended_model_ids,
                ),
                demoted_recommended_model_ids: parse_string_array(&r.demoted_recommended_model_ids),
                updated_at: r.updated_at,
            })
            .unwrap_or_default())
    }

    /// Replace the entire policy (admin write). All fields are authoritative.
    /// Upserts the singleton row so a fresh DB without the seed still persists.
    pub async fn set(&self, policy: &OrgAiPolicy) -> Result<OrgAiPolicy> {
        self.db.ensure_initialized().await?;
        let blocked = serde_json::to_string(&policy.blocked_subscriptions)
            .map_err(|e| crate::Error::Internal(format!("serialize blocked_subscriptions: {e}")))?;
        let default_lanes = serde_json::to_string(&policy.default_lanes)
            .map_err(|e| crate::Error::Internal(format!("serialize default_lanes: {e}")))?;
        let lock_level = policy.lock_level.as_db();
        let additional =
            serde_json::to_string(&policy.additional_recommended_model_ids).map_err(|e| {
                crate::Error::Internal(format!("serialize additional_recommended_model_ids: {e}"))
            })?;
        let demoted =
            serde_json::to_string(&policy.demoted_recommended_model_ids).map_err(|e| {
                crate::Error::Internal(format!("serialize demoted_recommended_model_ids: {e}"))
            })?;
        sqlx::query!(
            r#"INSERT INTO org_ai_policy
                   (id, blocked_subscriptions, default_lanes, lock_level,
                    additional_recommended_model_ids, demoted_recommended_model_ids,
                    updated_at)
               VALUES (1, $1, $2, $3, $4, $5,
                       to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
               ON CONFLICT (id) DO UPDATE SET
                   blocked_subscriptions = EXCLUDED.blocked_subscriptions,
                   default_lanes = EXCLUDED.default_lanes,
                   lock_level = EXCLUDED.lock_level,
                   additional_recommended_model_ids = EXCLUDED.additional_recommended_model_ids,
                   demoted_recommended_model_ids = EXCLUDED.demoted_recommended_model_ids,
                   updated_at = EXCLUDED.updated_at"#,
            blocked,
            default_lanes,
            lock_level,
            additional,
            demoted,
        )
        .execute(self.db.pool())
        .await?;
        self.get().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn get_returns_defaults_on_fresh_db() {
        let db = Database::open_in_memory().expect("in-memory db");
        let repo = OrgAiPolicyRepository::new(db);
        let p = repo.get().await.unwrap();
        assert!(p.blocked_subscriptions.is_empty());
        assert!(p.default_lanes.is_empty());
        assert_eq!(p.lock_level, LockLevel::Flexible);
        assert!(p.additional_recommended_model_ids.is_empty());
        assert!(p.demoted_recommended_model_ids.is_empty());
    }

    #[tokio::test]
    async fn set_then_get_round_trips() {
        let db = Database::open_in_memory().expect("in-memory db");
        let repo = OrgAiPolicyRepository::new(db);

        let policy = OrgAiPolicy {
            blocked_subscriptions: vec![
                "minimax-coding-plan".to_string(),
                "kimi-for-coding".to_string(),
            ],
            default_lanes: OrgDefaultLanes {
                plan: vec!["anthropic/claude-opus-4-7".to_string()],
                implement: vec!["openai/gpt-5.5".to_string()],
                review: vec![],
            },
            lock_level: LockLevel::Locked,
            additional_recommended_model_ids: vec!["anthropic/claude-sonnet-4-7".to_string()],
            demoted_recommended_model_ids: vec!["openai/gpt-5.5".to_string()],
            updated_at: String::new(),
        };
        let saved = repo.set(&policy).await.unwrap();
        assert_eq!(saved.blocked_subscriptions, policy.blocked_subscriptions);
        assert_eq!(saved.default_lanes, policy.default_lanes);
        assert_eq!(saved.lock_level, LockLevel::Locked);
        assert_eq!(
            saved.additional_recommended_model_ids,
            vec!["anthropic/claude-sonnet-4-7".to_string()]
        );
        assert_eq!(
            saved.demoted_recommended_model_ids,
            vec!["openai/gpt-5.5".to_string()]
        );
        assert!(!saved.updated_at.is_empty(), "updated_at stamped on write");

        let read = repo.get().await.unwrap();
        assert_eq!(read.lock_level, LockLevel::Locked);
        assert!(read.is_blocked("MiniMax-Coding-Plan"));
        assert_eq!(
            read.additional_recommended_model_ids,
            vec!["anthropic/claude-sonnet-4-7".to_string()]
        );
        assert_eq!(
            read.demoted_recommended_model_ids,
            vec!["openai/gpt-5.5".to_string()]
        );
    }

    #[tokio::test]
    async fn set_overwrites_previous_policy() {
        let db = Database::open_in_memory().expect("in-memory db");
        let repo = OrgAiPolicyRepository::new(db);

        repo.set(&OrgAiPolicy {
            blocked_subscriptions: vec!["zai-coding-plan".to_string()],
            lock_level: LockLevel::Locked,
            additional_recommended_model_ids: vec!["anthropic/claude-opus-4-8".to_string()],
            ..Default::default()
        })
        .await
        .unwrap();

        // A subsequent write fully replaces the previous policy.
        let cleared = repo.set(&OrgAiPolicy::default()).await.unwrap();
        assert!(cleared.blocked_subscriptions.is_empty());
        assert_eq!(cleared.lock_level, LockLevel::Flexible);
        assert!(cleared.additional_recommended_model_ids.is_empty());
        assert!(cleared.demoted_recommended_model_ids.is_empty());
    }
}
