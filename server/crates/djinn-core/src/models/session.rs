use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Running,
    Completed,
    Interrupted,
    Failed,
    Paused,
}

impl SessionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
            Self::Failed => "failed",
            Self::Paused => "paused",
        }
    }
}

/// How to interpret a session's `cost_usd` value for usage analytics.
///
/// - `Actual` — API-key / pay-as-you-go provider; `cost_usd` is real API spend.
/// - `Projected` — subscription / coding-plan provider; `cost_usd` is a
///   list-rate projection and actual API spend is $0.
/// - `Unpriced` — uncatalogued or missing-price session; excluded from both
///   actual and projected dollar aggregates, but counted visibly.
///
/// Derived at session creation from the resolved catalog pricing and provider
/// credential class, then persisted as a denormalized text label on the session
/// row. No credential foreign key is introduced.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostBasis {
    Actual,
    Projected,
    #[default]
    Unpriced,
}

impl CostBasis {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Actual => "actual",
            Self::Projected => "projected",
            Self::Unpriced => "unpriced",
        }
    }

    /// Parse from the database text representation.
    /// Returns `Unpriced` for any unknown value (defensive).
    pub fn from_db(s: &str) -> Self {
        match s {
            "actual" => Self::Actual,
            "projected" => Self::Projected,
            _ => Self::Unpriced,
        }
    }
}

/// Stable, coarse-grained cause for a failed or interrupted session.
///
/// The seven durable variants are the only values allowed in
/// `sessions.failure_cause`. [`Self::LegacyUnclassified`] is a virtual
/// read/report interpretation for failed or interrupted rows from before the
/// column existed; it must never be written to the column. Reason and
/// diagnostic text intentionally do not belong in this taxonomy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionFailureCause {
    Cancelled,
    Provider,
    Harness,
    Infrastructure,
    Protocol,
    Finalization,
    Unknown,
    LegacyUnclassified,
}

impl SessionFailureCause {
    /// Return the stable report/wire label for this cause.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Provider => "provider",
            Self::Harness => "harness",
            Self::Infrastructure => "infrastructure",
            Self::Protocol => "protocol",
            Self::Finalization => "finalization",
            Self::Unknown => "unknown",
            Self::LegacyUnclassified => "legacy_unclassified",
        }
    }

    /// Parse a durable database label defensively.
    ///
    /// Noncanonical values, including `legacy_unclassified`, are never
    /// retained and instead become [`Self::Unknown`]. The legacy variant is
    /// assigned only by status-aware read logic for a NULL legacy column.
    pub fn from_db(value: &str) -> Self {
        match value {
            "cancelled" => Self::Cancelled,
            "provider" => Self::Provider,
            "harness" => Self::Harness,
            "infrastructure" => Self::Infrastructure,
            "protocol" => Self::Protocol,
            "finalization" => Self::Finalization,
            "unknown" => Self::Unknown,
            _ => Self::Unknown,
        }
    }

    /// Return the label permitted in the durable `sessions.failure_cause`
    /// column, or `None` for the read-only legacy interpretation.
    pub fn durable_label(self) -> Option<&'static str> {
        match self {
            Self::LegacyUnclassified => None,
            cause => Some(cause.as_str()),
        }
    }
}

impl fmt::Display for SessionFailureCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SessionFailureCause {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(match value.as_str() {
            // `legacy_unclassified` is a report-only wire value, never a
            // database label. Database parsing remains strict in `from_db`.
            "legacy_unclassified" => Self::LegacyUnclassified,
            value => Self::from_db(value),
        })
    }
}

#[cfg(feature = "sqlx")]
impl sqlx::Type<sqlx::Postgres> for SessionFailureCause {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        <String as sqlx::Type<sqlx::Postgres>>::type_info()
    }

    fn compatible(ty: &sqlx::postgres::PgTypeInfo) -> bool {
        <String as sqlx::Type<sqlx::Postgres>>::compatible(ty)
    }
}

#[cfg(feature = "sqlx")]
impl<'r> sqlx::Decode<'r, sqlx::Postgres> for SessionFailureCause {
    fn decode(
        value: sqlx::postgres::PgValueRef<'r>,
    ) -> std::result::Result<Self, sqlx::error::BoxDynError> {
        let value = <String as sqlx::Decode<sqlx::Postgres>>::decode(value)?;
        Ok(Self::from_db(&value))
    }
}

/// Persisted lifecycle record for a supervisor-run agent session.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct SessionRecord {
    pub id: String,
    /// `NULL` for `agent_type = 'chat'` (global user-scoped sessions); required
    /// for every other agent type. Enforced at the schema level via the
    /// `sessions_project_scope_by_agent_type` CHECK constraint (migration 14).
    pub project_id: Option<String>,
    pub task_id: Option<String>,
    pub model_id: String,
    pub agent_type: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub status: String,
    pub tokens_in: i64,
    pub tokens_out: i64,
    /// Running total of prompt-cache reads (cache hits) and writes (cache
    /// creation) across the session, persisted so cache hit-rate is queryable
    /// from the DB even when OTel/Langfuse telemetry is not configured.
    /// Added in migration 52.
    #[serde(default)]
    pub cache_read_tokens: i64,
    #[serde(default)]
    pub cache_write_tokens: i64,
    /// FK into `task_runs`; populated by the supervisor. The authoritative
    /// workspace path lives on the task_run row. Before migration 6 this
    /// struct also carried a `worktree_path: Option<String>` field mirroring
    /// the now-dropped `sessions.worktree_path` column.
    pub task_run_id: Option<String>,
    /// Human-readable title.  Populated (and auto-generated) for
    /// `agent_type='chat'` sessions; `NULL` for every other agent type.
    /// Added in migration 16.
    #[serde(default)]
    pub title: Option<String>,
    /// Optional reason a terminal session was deliberately parked instead of
    /// being treated as an ordinary completion/failure. Added in migration 59.
    #[serde(default)]
    pub parked_reason: Option<String>,
    /// Stable, coarse-grained terminal failure cause. This is nullable for
    /// legacy rows and sessions without a failure; it intentionally contains
    /// only taxonomy labels, never parked reasons or diagnostics.
    /// Added in migration 183.
    #[serde(default)]
    pub failure_cause: Option<SessionFailureCause>,
    /// Total cost of the session in USD, derived from the per-million snapshot
    /// rates and the session's token counts. `NULL` until pricing logic is
    /// wired up (unpriced/uncatalogued sessions stay NULL, never $0).
    /// Added in migration 66.
    #[serde(default)]
    pub cost_usd: Option<f64>,
    /// Start-time snapshot of the model's per-million input-token price (USD).
    /// Snapshotted so the cost is stable against later catalog changes.
    /// Added in migration 66.
    #[serde(default)]
    pub input_price_per_million_snapshot: Option<f64>,
    /// Start-time snapshot of the model's per-million output-token price (USD).
    /// Added in migration 66.
    #[serde(default)]
    pub output_price_per_million_snapshot: Option<f64>,
    /// Start-time snapshot of the model's per-million prompt-cache read
    /// (cache hit) token price (USD). Added in migration 66.
    #[serde(default)]
    pub cache_read_price_per_million_snapshot: Option<f64>,
    /// Start-time snapshot of the model's per-million prompt-cache write
    /// (cache creation) token price (USD). Added in migration 66.
    #[serde(default)]
    pub cache_write_price_per_million_snapshot: Option<f64>,
    /// How to interpret `cost_usd` for usage analytics: `actual` (real API
    /// spend), `projected` (subscription-equivalent list-rate projection), or
    /// `unpriced` (excluded from dollar aggregates). Derived at session
    /// creation from catalog pricing + provider credential class.
    /// Added in migration 83.
    #[serde(default = "default_cost_basis")]
    pub cost_basis: String,
    /// Kind of credential that backed the session, for plan-vs-API-key usage
    /// analytics: `plan_oauth` (a personal subscription-plan OAuth credential —
    /// $0 real per-token spend), `api_key` (a metered or coding-plan API key),
    /// or `NULL` (not recorded: legacy rows, interactive `chat`, and
    /// post-session extraction helpers that carry no dispatch-time credential
    /// signal). Written at creation from the resolved credential kind; no
    /// credential foreign key is introduced. Added in migration 88.
    #[serde(default)]
    pub billing_source: Option<String>,
}

impl SessionRecord {
    /// Return the status-aware failure cause suitable for reporting.
    ///
    /// A durable value always wins. Failed and interrupted rows predating the
    /// nullable column are classified as `legacy_unclassified`; NULL remains
    /// cause-free for completed (and non-terminal) rows.
    pub fn interpreted_failure_cause(&self) -> Option<SessionFailureCause> {
        self.failure_cause.or_else(|| match self.status.as_str() {
            "failed" | "interrupted" => Some(SessionFailureCause::LegacyUnclassified),
            _ => None,
        })
    }
}

fn default_cost_basis() -> String {
    "unpriced".to_string()
}

#[cfg(test)]
mod tests {
    use super::{SessionFailureCause, SessionRecord};

    #[test]
    fn session_failure_causes_round_trip_through_durable_labels_and_serde() {
        let cases = [
            (SessionFailureCause::Cancelled, "cancelled"),
            (SessionFailureCause::Provider, "provider"),
            (SessionFailureCause::Harness, "harness"),
            (SessionFailureCause::Infrastructure, "infrastructure"),
            (SessionFailureCause::Protocol, "protocol"),
            (SessionFailureCause::Finalization, "finalization"),
            (SessionFailureCause::Unknown, "unknown"),
        ];

        for (cause, label) in cases {
            assert_eq!(cause.as_str(), label);
            assert_eq!(cause.durable_label(), Some(label));
            assert_eq!(SessionFailureCause::from_db(label), cause);
            assert_eq!(cause.to_string(), label);

            let encoded = serde_json::to_string(&cause).unwrap();
            assert_eq!(encoded, format!("\"{label}\""));
            let decoded: SessionFailureCause = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, cause);
        }
    }

    #[test]
    fn session_failure_cause_discards_unknown_durable_and_wire_labels() {
        let future_label = "provider_timeout: request 123";

        assert_eq!(
            SessionFailureCause::from_db(future_label),
            SessionFailureCause::Unknown
        );
        let decoded: SessionFailureCause =
            serde_json::from_str(&format!("\"{future_label}\"")).unwrap();
        assert_eq!(decoded, SessionFailureCause::Unknown);
        assert_eq!(decoded.as_str(), "unknown");
        assert_eq!(
            SessionFailureCause::from_db("legacy_unclassified"),
            SessionFailureCause::Unknown
        );
    }

    #[test]
    fn session_failure_cause_legacy_interpretation_is_not_persistable() {
        let legacy = SessionFailureCause::LegacyUnclassified;

        assert_eq!(legacy.as_str(), "legacy_unclassified");
        assert_eq!(legacy.durable_label(), None);
        assert_eq!(legacy.to_string(), "legacy_unclassified");

        let encoded = serde_json::to_string(&legacy).unwrap();
        assert_eq!(encoded, "\"legacy_unclassified\"");
        let decoded: SessionFailureCause = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, legacy);
    }

    fn session_record(parked_reason: Option<String>) -> SessionRecord {
        SessionRecord {
            id: "session-1".to_owned(),
            project_id: Some("project-1".to_owned()),
            task_id: Some("task-1".to_owned()),
            model_id: "model".to_owned(),
            agent_type: "worker".to_owned(),
            started_at: "2026-01-02T03:04:05.000Z".to_owned(),
            ended_at: None,
            status: "completed".to_owned(),
            tokens_in: 1,
            tokens_out: 2,
            cache_read_tokens: 3,
            cache_write_tokens: 4,
            task_run_id: None,
            title: None,
            parked_reason,
            failure_cause: None,
            cost_usd: None,
            input_price_per_million_snapshot: None,
            output_price_per_million_snapshot: None,
            cache_read_price_per_million_snapshot: None,
            cache_write_price_per_million_snapshot: None,
            cost_basis: "unpriced".to_owned(),
            billing_source: None,
        }
    }

    #[test]
    fn session_record_serde_round_trips_without_parked_reason() {
        let record = session_record(None);

        let encoded = serde_json::to_string(&record).unwrap();
        let decoded: SessionRecord = serde_json::from_str(&encoded).unwrap();

        assert!(decoded.parked_reason.is_none());
        assert_eq!(decoded.id, record.id);
    }

    #[test]
    fn session_record_serde_round_trips_with_parked_reason() {
        let record = session_record(Some("budget".to_owned()));

        let encoded = serde_json::to_string(&record).unwrap();
        let decoded: SessionRecord = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded.parked_reason.as_deref(), Some("budget"));
        assert_eq!(decoded.id, record.id);
    }

    #[test]
    fn session_record_defaults_missing_nullable_additions() {
        let json = serde_json::json!({
            "id": "session-1",
            "project_id": "project-1",
            "task_id": "task-1",
            "model_id": "model",
            "agent_type": "worker",
            "started_at": "2026-01-02T03:04:05.000Z",
            "ended_at": null,
            "status": "completed",
            "tokens_in": 1,
            "tokens_out": 2,
            "cache_read_tokens": 3,
            "cache_write_tokens": 4,
            "task_run_id": null,
            "title": null
        });

        let decoded: SessionRecord = serde_json::from_value(json).unwrap();

        assert!(decoded.parked_reason.is_none());
        assert!(decoded.failure_cause.is_none());
        assert!(decoded.cost_usd.is_none());
        assert!(decoded.input_price_per_million_snapshot.is_none());
        assert!(decoded.output_price_per_million_snapshot.is_none());
        assert!(decoded.cache_read_price_per_million_snapshot.is_none());
        assert!(decoded.cache_write_price_per_million_snapshot.is_none());
        assert_eq!(decoded.cost_basis, "unpriced");
        assert!(decoded.billing_source.is_none());
    }

    #[test]
    fn session_record_interprets_durable_and_legacy_failure_causes() {
        let mut record = session_record(None);

        assert_eq!(record.interpreted_failure_cause(), None);

        record.status = "failed".to_owned();
        assert_eq!(
            record.interpreted_failure_cause(),
            Some(SessionFailureCause::LegacyUnclassified)
        );

        record.status = "interrupted".to_owned();
        assert_eq!(
            record.interpreted_failure_cause(),
            Some(SessionFailureCause::LegacyUnclassified)
        );

        record.failure_cause = Some(SessionFailureCause::Provider);
        assert_eq!(
            record.interpreted_failure_cause(),
            Some(SessionFailureCause::Provider)
        );
    }
}
