use serde::{Deserialize, Serialize};

/// Durable coordinator dispatch-decision state for one task.
///
/// Wall-clock timestamps are stored as database values and exposed as strings so
/// the coordinator can translate them into process-local `Instant` values at
/// startup. Runtime `Instant`s are intentionally never serialized.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct DispatchStateRecord {
    pub task_id: String,
    pub failure_streak: i64,
    pub cooldown_until: Option<String>,
    pub escalation_count: i64,
    pub last_dispatched_at: Option<String>,
    pub last_dispatched_role: Option<String>,
    pub inflight_creator_user_id: Option<String>,
    pub inflight_model_id: Option<String>,
    pub updated_at: String,
}
