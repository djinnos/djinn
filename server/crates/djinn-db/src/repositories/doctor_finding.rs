//! Doctor findings persistence (Doctor framework epic 08f0, migration 61).
//!
//! The Doctor framework (`djinn-core::doctor`) runs `DoctorCheck`s and emits
//! structured `Finding`s. This repository is the durable store: a row per
//! emitted finding, queryable by id / check name / creation timestamp, with
//! the structured `evidence` and `resolver_snapshot` payloads preserved as
//! JSONB so the fix path (and reports) can re-deserialize the same shape
//! the check produced.
//!
//! ## Resolver snapshot invariant
//!
//! `resolver_snapshot` is the bridge that enforces the Gas Town
//! shared-resolver fix invariant: a fix path MUST re-run the same resolver
//! against the snapshot's inputs, never a hard-coded expected value. We
//! store the snapshot opaquely here (it can be `null` for checks that have
//! no associated resolver); the actual fix enforcement lives in
//! `djinn-core::doctor` and the control-plane MCP tool wiring.
//!
//! ## Severity
//!
//! Mirrored as a `String` (not a Rust enum) so the DB stays the source of
//! truth and the framework can grow new severities without a migration.
//! The schema enforces the value-set via a CHECK constraint.
//!
//! ## JSON payloads
//!
//! `evidence`, `entity_ids`, and `resolver_snapshot` are persisted as JSONB
//! through `serde_json::Value` binds (no manual `to_string` / `from_str`
//! round-trips) so callers can construct any structured shape they need
//! without the repository imposing a typed envelope.

use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::Result;
use crate::database::Database;

/// Canonical severity values enforced by the `doctor_findings_severity_check`
/// constraint. Stored as `&'static str` to keep bind sites allocation-free.
pub mod severity {
    pub const INFO: &str = "info";
    pub const WARN: &str = "warn";
    pub const CRITICAL: &str = "critical";
    pub const ERROR: &str = "error";

    /// True when `value` is one of the canonical severity labels.
    pub fn is_known(value: &str) -> bool {
        matches!(value, INFO | WARN | CRITICAL | ERROR)
    }
}

#[derive(Clone, Debug)]
pub struct KeyedDoctorFinding {
    pub active_key: String,
    pub finding: NewDoctorFinding,
}

/// Lifecycle changes made by one retrieval-health reconciliation.
///
/// Retrieval findings have stable active keys, so callers need the complete
/// rows for both upserts and healthy-absence resolutions to emit an accurate
/// audit/activity trail.
#[derive(Clone, Debug, Default)]
pub struct RetrievalFindingReconciliation {
    pub created: Vec<DoctorFinding>,
    pub updated: Vec<DoctorFinding>,
    pub resolved: Vec<DoctorFinding>,
}

/// A persisted doctor finding — one row of `doctor_findings`.
///
/// This is the durable shape, not the in-memory `Finding` from
/// `djinn-core::doctor`. The two are deliberately decoupled so the
/// repository can compile independently of the core framework and so the
/// schema remains the source of truth. Conversion happens at the
/// control-plane boundary (`From<Finding> for DoctorFinding`) where both
/// types are in scope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorFinding {
    pub id: String,
    /// Identifier of the run that produced this finding (an MCP call id,
    /// a leader-tick id, …). `None` for ad-hoc / single-finding inserts.
    pub run_id: Option<String>,
    /// Wall-clock UTC ISO-8601 timestamp the finding was recorded.
    pub created_at: String,
    pub status: String,
    pub observed_at: String,
    pub check_name: String,
    /// One of `severity::INFO`, `severity::WARN`, `severity::CRITICAL`.
    pub severity: String,
    /// Opaque entity ids this finding relates to. Always a JSON array;
    /// `[]` when the finding has no specific entity.
    pub entity_ids: serde_json::Value,
    /// Structured check-specific evidence (query results, computed values,
    /// …). Free-form JSON.
    pub evidence: serde_json::Value,
    /// Resolver inputs and outputs captured at check time. `None` for
    /// checks with no associated resolver.
    pub resolver_snapshot: Option<serde_json::Value>,
    /// Free-form human-readable detail surfaced in reports.
    pub detail: Option<String>,
}

/// Result of an immutable, deduplication-keyed finding insert.
///
/// Unlike retrieval reconciliation, this operation never updates an existing
/// row: a repeat key preserves the original evidence exactly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeduplicatedDoctorFindingInsert {
    Inserted(Box<DoctorFinding>),
    AlreadyPresent,
}

/// Input for inserting a single finding. `id` and `created_at` are stamped
/// by the repository so callers do not need to generate them. `run_id` is
/// optional and may be `None` for ad-hoc inserts.
#[derive(Clone, Debug)]
pub struct NewDoctorFinding {
    pub run_id: Option<String>,
    pub check_name: String,
    pub severity: String,
    pub entity_ids: serde_json::Value,
    pub evidence: serde_json::Value,
    pub resolver_snapshot: Option<serde_json::Value>,
    pub detail: Option<String>,
}

/// Filter for `list_recent` — bounded, newest-first scans of the table.
///
/// Every filter is optional; an unset field means "no narrowing on that
/// dimension". All filtering happens in SQL — callers never receive the
/// unfiltered row set. The repository does not impose a default
/// `since` cutoff (board/audit callers explicitly opt in to the time
/// filter so a year-old run is still findable through the MCP
/// `doctor_list_findings` surface).
#[derive(Clone, Debug, Default)]
pub struct RecentDoctorFindings {
    /// Optional run id filter (matches `run_id` exactly when set).
    pub run_id: Option<String>,
    /// Optional check name filter (matches `check_name` exactly when set).
    pub check_name: Option<String>,
    /// Optional lower-bound timestamp filter. Rows with
    /// `created_at < since` are excluded. The value is compared as a
    /// string (the column is a UTC ISO-8601 `VARCHAR` — see
    /// `61_doctor_findings.sql`), which is lexicographically equivalent
    /// to a real time comparison for any value matching the schema's
    /// `to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')`
    /// template.
    pub since: Option<String>,
    /// Cap on the number of rows returned. `None` means "no explicit cap"
    /// (the repository still applies a defensive ceiling — see
    /// `MAX_RECENT_FINDINGS`).
    pub limit: Option<usize>,
}

/// Defensive ceiling for `list_recent` when the caller passes `None`.
/// Prevents accidentally materializing a million-row history scan.
pub const MAX_RECENT_FINDINGS: usize = 500;

pub struct DoctorFindingRepository {
    db: Database,
}

impl DoctorFindingRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Insert a single finding. Returns the persisted row (with stamped
    /// `id` and `created_at`).
    pub async fn insert(&self, new: NewDoctorFinding) -> Result<DoctorFinding> {
        self.db.ensure_initialized().await?;
        let id = Uuid::now_v7().to_string();
        sqlx::query(
            r#"INSERT INTO doctor_findings
                 (id, run_id, check_name, severity, entity_ids, evidence,
                  resolver_snapshot, detail)
               VALUES ($1, $2, $3, $4, $5::jsonb, $6::jsonb, $7::jsonb, $8)"#,
        )
        .bind(&id)
        .bind(new.run_id.as_deref())
        .bind(&new.check_name)
        .bind(&new.severity)
        .bind(&new.entity_ids)
        .bind(&new.evidence)
        .bind(new.resolver_snapshot.as_ref())
        .bind(new.detail.as_deref())
        .execute(self.db.pool())
        .await?;

        // The DEFAULT on `created_at` uses `now()`, so we can fetch the row
        // we just wrote by id. This keeps the response self-consistent
        // without requiring the caller to pass a timestamp.
        self.get(&id).await?.ok_or_else(|| {
            crate::Error::Internal("doctor finding vanished after insert".to_owned())
        })
    }

    /// Insert a finding once for a non-null immutable deduplication key.
    ///
    /// Migration 137 supplies the nullable unique index used here. `DO NOTHING`
    /// is intentional: proposal-integrity findings are historical evidence, so
    /// a rerun must not replace the first observation's evidence or timestamp.
    pub async fn insert_ignore_duplicate(
        &self,
        new: NewDoctorFinding,
        deduplication_key: &str,
    ) -> Result<DeduplicatedDoctorFindingInsert> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query(&format!(
            r#"INSERT INTO doctor_findings
                 (id, run_id, check_name, severity, entity_ids, evidence,
                  resolver_snapshot, detail, deduplication_key)
               VALUES ($1, $2, $3, $4, $5::jsonb, $6::jsonb, $7::jsonb, $8, $9)
               ON CONFLICT (deduplication_key) WHERE deduplication_key IS NOT NULL
               DO NOTHING
               RETURNING {SELECT_COLS}"#
        ))
        .bind(Uuid::now_v7().to_string())
        .bind(new.run_id.as_deref())
        .bind(&new.check_name)
        .bind(&new.severity)
        .bind(&new.entity_ids)
        .bind(&new.evidence)
        .bind(new.resolver_snapshot.as_ref())
        .bind(new.detail.as_deref())
        .bind(deduplication_key)
        .fetch_optional(self.db.pool())
        .await?;

        Ok(match row {
            Some(row) => DeduplicatedDoctorFindingInsert::Inserted(Box::new(row_to_finding(&row))),
            None => DeduplicatedDoctorFindingInsert::AlreadyPresent,
        })
    }

    /// Insert a batch of findings from a single run. Each finding gets
    /// its own id and `created_at`; the operation is NOT atomic across
    /// findings (a per-row failure aborts the batch with whatever rows
    /// had already been written). Returns the persisted rows in the same
    /// order as the input.
    pub async fn insert_many(&self, batch: Vec<NewDoctorFinding>) -> Result<Vec<DoctorFinding>> {
        let mut out = Vec::with_capacity(batch.len());
        for finding in batch {
            out.push(self.insert(finding).await?);
        }
        Ok(out)
    }

    /// Reconcile retrieval-health findings; preserved keys are not mutated.
    /// Returns every lifecycle transition so callers can record an activity
    /// entry for creates, updates, and healthy-absence resolutions.
    pub async fn reconcile_retrieval_findings(
        &self,
        findings: Vec<KeyedDoctorFinding>,
        preserve_keys: &[String],
    ) -> Result<RetrievalFindingReconciliation> {
        self.db.ensure_initialized().await?;
        let mut emitted = Vec::new();
        let mut reconciliation = RetrievalFindingReconciliation::default();
        for keyed in findings {
            emitted.push(keyed.active_key.clone());
            let n = keyed.finding;
            let row = sqlx::query(&format!(r#"INSERT INTO doctor_findings (id,run_id,check_name,severity,entity_ids,evidence,resolver_snapshot,detail,active_key,status) VALUES ($1,$2,$3,$4,$5::jsonb,$6::jsonb,$7::jsonb,$8,$9,'active') ON CONFLICT (check_name,active_key) WHERE active_key IS NOT NULL DO UPDATE SET run_id=EXCLUDED.run_id,severity=EXCLUDED.severity,entity_ids=EXCLUDED.entity_ids,evidence=EXCLUDED.evidence,resolver_snapshot=EXCLUDED.resolver_snapshot,detail=EXCLUDED.detail,status='active',observed_at=to_char(now() AT TIME ZONE 'utc','YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') RETURNING {SELECT_COLS}, (xmax = 0) AS created"#))
                .bind(Uuid::now_v7().to_string()).bind(n.run_id.as_deref()).bind(&n.check_name).bind(&n.severity).bind(&n.entity_ids).bind(&n.evidence).bind(n.resolver_snapshot.as_ref()).bind(n.detail.as_deref()).bind(&keyed.active_key).fetch_one(self.db.pool()).await?;
            let finding = row_to_finding(&row);
            if row.get::<bool, _>("created") {
                reconciliation.created.push(finding);
            } else {
                reconciliation.updated.push(finding);
            }
        }
        let resolved = sqlx::query(&format!(r#"UPDATE doctor_findings SET status='resolved',observed_at=to_char(now() AT TIME ZONE 'utc','YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') WHERE status='active' AND check_name IN ('memory.retrieval_zero_result','memory.injection_starvation','memory.retrieval_health_refresh') AND NOT (active_key=ANY($1)) AND NOT (active_key=ANY($2)) RETURNING {SELECT_COLS}"#))
            .bind(&emitted)
            .bind(preserve_keys)
            .fetch_all(self.db.pool())
            .await?;
        reconciliation.resolved = resolved.iter().map(row_to_finding).collect();
        Ok(reconciliation)
    }

    pub async fn active_retrieval_alarm_keys(&self) -> Result<Vec<String>> {
        self.db.ensure_initialized().await?;
        let rows = sqlx::query("SELECT active_key FROM doctor_findings WHERE status = 'active' AND check_name IN ('memory.retrieval_zero_result', 'memory.injection_starvation') AND active_key IS NOT NULL")
            .fetch_all(self.db.pool()).await?;
        Ok(rows.iter().map(|row| row.get("active_key")).collect())
    }

    /// Fetch a finding by its primary key. `None` if not found.
    pub async fn get(&self, id: &str) -> Result<Option<DoctorFinding>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query(&format!(
            "SELECT {SELECT_COLS} FROM doctor_findings WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.as_ref().map(row_to_finding))
    }

    /// Return the immutable deduplication key for one persisted finding.
    /// It is separate from [`DoctorFinding`] because it is an idempotency
    /// contract rather than board-facing finding data.
    pub async fn deduplication_key(&self, id: &str) -> Result<Option<String>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query("SELECT deduplication_key FROM doctor_findings WHERE id = $1")
            .bind(id)
            .fetch_optional(self.db.pool())
            .await?;
        Ok(row.map(|row| row.get("deduplication_key")))
    }

    /// Fetch the most recent finding for `check_name`, or `None`.
    /// Used by the fix path to load the snapshot for the latest run.
    pub async fn latest_for_check(&self, check_name: &str) -> Result<Option<DoctorFinding>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query(&format!(
            "SELECT {SELECT_COLS} FROM doctor_findings
             WHERE check_name = $1
             ORDER BY created_at DESC
             LIMIT 1"
        ))
        .bind(check_name)
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.as_ref().map(row_to_finding))
    }

    /// Newest-first scan, optionally filtered by `run_id`, `check_name`,
    /// and/or a `since` lower-bound timestamp. The `limit` is clamped to
    /// [`MAX_RECENT_FINDINGS`].
    ///
    /// All filters are applied in SQL; the repository never fetches
    /// unfiltered rows and post-filters in memory. The `since` predicate
    /// uses `created_at >= $n`, which is index-eligible via
    /// `doctor_findings_created_at_idx` (and via
    /// `doctor_findings_check_name_created_at_idx` when the call also
    /// narrows on `check_name`).
    pub async fn list_recent(&self, query: RecentDoctorFindings) -> Result<Vec<DoctorFinding>> {
        self.db.ensure_initialized().await?;

        let limit = query
            .limit
            .unwrap_or(MAX_RECENT_FINDINGS)
            .min(MAX_RECENT_FINDINGS);

        // Build the dynamic WHERE clause. The placeholders must be numbered
        // to match the bind order below (run_id first, then check_name,
        // then since, then the limit), so we compute the index for each
        // optional filter explicitly.
        let mut where_clauses: Vec<String> = Vec::new();
        let mut next_placeholder: usize = 1;
        if query.run_id.is_some() {
            where_clauses.push(format!("run_id = ${next_placeholder}"));
            next_placeholder += 1;
        }
        if query.check_name.is_some() {
            where_clauses.push(format!("check_name = ${next_placeholder}"));
            next_placeholder += 1;
        }
        if query.since.is_some() {
            where_clauses.push(format!("created_at >= ${next_placeholder}"));
            next_placeholder += 1;
        }
        let where_sql = if where_clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", where_clauses.join(" AND "))
        };
        let limit_placeholder = next_placeholder;

        let sql = format!(
            "SELECT {SELECT_COLS} FROM doctor_findings {where_sql} \
             ORDER BY created_at DESC LIMIT ${limit_placeholder}"
        );

        let mut q = sqlx::query(&sql);
        if let Some(run_id) = query.run_id.as_deref() {
            q = q.bind(run_id);
        }
        if let Some(check_name) = query.check_name.as_deref() {
            q = q.bind(check_name);
        }
        if let Some(since) = query.since.as_deref() {
            q = q.bind(since);
        }
        q = q.bind(limit as i64);

        let rows = q.fetch_all(self.db.pool()).await?;
        Ok(rows.iter().map(row_to_finding).collect())
    }

    /// Count findings for a given check, newest-first scanning. Used by
    /// report rollups; counts are unbounded (no LIMIT).
    pub async fn count_for_check(&self, check_name: &str) -> Result<i64> {
        self.db.ensure_initialized().await?;
        let row =
            sqlx::query("SELECT COUNT(*)::bigint AS n FROM doctor_findings WHERE check_name = $1")
                .bind(check_name)
                .fetch_one(self.db.pool())
                .await?;
        let n: i64 = row.try_get("n")?;
        Ok(n)
    }
}

/// Column list shared by every read path. Cast JSONB columns to `text` so
/// the runtime query path can deserialize them through `serde_json::from_str`
/// uniformly — this avoids the macro vs runtime split on JSONB types.
const SELECT_COLS: &str = "id, run_id, created_at, status, observed_at, check_name, severity, \
     entity_ids::text AS entity_ids_text, \
     evidence::text AS evidence_text, \
     resolver_snapshot::text AS resolver_snapshot_text, \
     detail";

fn row_to_finding(row: &sqlx::postgres::PgRow) -> DoctorFinding {
    let entity_ids_text: String = row
        .try_get("entity_ids_text")
        .unwrap_or_else(|_| "[]".to_owned());
    let evidence_text: String = row
        .try_get("evidence_text")
        .unwrap_or_else(|_| "{}".to_owned());
    let resolver_snapshot_text: Option<String> =
        row.try_get("resolver_snapshot_text").ok().flatten();

    DoctorFinding {
        id: row.get("id"),
        run_id: row.get("run_id"),
        created_at: row.get("created_at"),
        status: row
            .try_get("status")
            .unwrap_or_else(|_| "active".to_owned()),
        observed_at: row
            .try_get("observed_at")
            .unwrap_or_else(|_| row.get("created_at")),
        check_name: row.get("check_name"),
        severity: row.get("severity"),
        entity_ids: serde_json::from_str(&entity_ids_text)
            .unwrap_or(serde_json::Value::Array(Vec::new())),
        evidence: serde_json::from_str(&evidence_text)
            .unwrap_or(serde_json::Value::Object(serde_json::Map::new())),
        resolver_snapshot: resolver_snapshot_text
            .as_deref()
            .and_then(|raw| serde_json::from_str(raw).ok()),
        detail: row.get("detail"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;

    fn fresh_db() -> Database {
        Database::open_in_memory().expect("in-memory db")
    }

    fn new_finding(check_name: &str, severity: &str) -> NewDoctorFinding {
        NewDoctorFinding {
            run_id: Some("run-1".to_owned()),
            check_name: check_name.to_owned(),
            severity: severity.to_owned(),
            entity_ids: serde_json::json!(["task-1", "task-2"]),
            evidence: serde_json::json!({"query_time_ms": 12, "rows": 3}),
            resolver_snapshot: Some(serde_json::json!({
                "resolver": "config_snapshot",
                "inputs": {"path": "/etc/djinn.toml"},
                "outputs": {"stack": "rust"}
            })),
            detail: Some("check observed drift".to_owned()),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn insert_and_get_roundtrip_preserves_structured_json() {
        let db = fresh_db();
        let repo = DoctorFindingRepository::new(db);

        let inserted = repo
            .insert(new_finding("config_drift", severity::WARN))
            .await
            .expect("insert");

        assert_eq!(inserted.check_name, "config_drift");
        assert_eq!(inserted.severity, "warn");
        assert_eq!(inserted.run_id.as_deref(), Some("run-1"));
        assert!(!inserted.id.is_empty());
        assert!(
            inserted.created_at.contains('T') && inserted.created_at.ends_with('Z'),
            "created_at should be an ISO-8601 string, got {}",
            inserted.created_at
        );

        // The structured fields survive the round-trip without manual
        // stringification (the whole point of the JSONB columns).
        assert_eq!(inserted.entity_ids, serde_json::json!(["task-1", "task-2"]));
        assert_eq!(
            inserted.evidence,
            serde_json::json!({"query_time_ms": 12, "rows": 3})
        );
        assert_eq!(
            inserted.resolver_snapshot.as_ref().unwrap(),
            &serde_json::json!({
                "resolver": "config_snapshot",
                "inputs": {"path": "/etc/djinn.toml"},
                "outputs": {"stack": "rust"}
            })
        );

        let fetched = repo
            .get(&inserted.id)
            .await
            .expect("get")
            .expect("row present");
        assert_eq!(fetched, inserted);

        // Missing id returns None.
        assert!(repo.get("does-not-exist").await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn insert_many_returns_persisted_rows_in_order() {
        let db = fresh_db();
        let repo = DoctorFindingRepository::new(db);
        let batch = vec![
            new_finding("config_drift", severity::INFO),
            new_finding("config_drift", severity::WARN),
            new_finding("zombie_reaper", severity::CRITICAL),
        ];

        let persisted = repo.insert_many(batch).await.expect("batch");
        assert_eq!(persisted.len(), 3);
        // IDs are unique.
        let ids: std::collections::HashSet<&str> =
            persisted.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids.len(), 3);
        // The 1st and 2nd are for `config_drift`, the 3rd for `zombie_reaper`.
        assert_eq!(persisted[0].check_name, "config_drift");
        assert_eq!(persisted[1].check_name, "config_drift");
        assert_eq!(persisted[2].check_name, "zombie_reaper");
        // Timestamps are monotonically non-decreasing — UUIDv7 id sort and
        // `created_at` sort should agree.
        assert!(persisted[0].created_at <= persisted[1].created_at);
        assert!(persisted[1].created_at <= persisted[2].created_at);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn latest_for_check_returns_newest_matching_row() {
        let db = fresh_db();
        let repo = DoctorFindingRepository::new(db);
        let first = repo
            .insert(new_finding("config_drift", severity::INFO))
            .await
            .unwrap();
        // UUIDv7 timestamps are ms-resolution; a tiny sleep guarantees a
        // strictly later `created_at` without depending on monotonic clock
        // behavior across CI hosts.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let second = repo
            .insert(new_finding("config_drift", severity::CRITICAL))
            .await
            .unwrap();
        // Unrelated check that must not match.
        repo.insert(new_finding("zombie_reaper", severity::WARN))
            .await
            .unwrap();

        let latest = repo
            .latest_for_check("config_drift")
            .await
            .unwrap()
            .expect("latest");
        assert_eq!(latest.id, second.id);
        assert_eq!(latest.severity, "critical");
        assert!(latest.created_at >= first.created_at);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_recent_filters_by_run_id_and_check_name() {
        let db = fresh_db();
        let repo = DoctorFindingRepository::new(db);
        let mut a = new_finding("config_drift", severity::INFO);
        a.run_id = Some("run-A".to_owned());
        let mut b = new_finding("config_drift", severity::WARN);
        b.run_id = Some("run-B".to_owned());
        let mut c = new_finding("zombie_reaper", severity::CRITICAL);
        c.run_id = Some("run-B".to_owned());
        repo.insert(a).await.unwrap();
        repo.insert(b).await.unwrap();
        repo.insert(c).await.unwrap();

        // No filters → everything, newest-first.
        let all = repo
            .list_recent(RecentDoctorFindings::default())
            .await
            .unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].check_name, "zombie_reaper");
        assert_eq!(all[1].check_name, "config_drift");
        assert_eq!(all[2].check_name, "config_drift");

        // Filter by run_id only.
        let run_b = repo
            .list_recent(RecentDoctorFindings {
                run_id: Some("run-B".to_owned()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(run_b.len(), 2);
        assert!(run_b.iter().all(|f| f.run_id.as_deref() == Some("run-B")));

        // Filter by check_name only.
        let drift_only = repo
            .list_recent(RecentDoctorFindings {
                check_name: Some("config_drift".to_owned()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(drift_only.len(), 2);
        assert!(drift_only.iter().all(|f| f.check_name == "config_drift"));

        // Both filters combined.
        let combined = repo
            .list_recent(RecentDoctorFindings {
                run_id: Some("run-B".to_owned()),
                check_name: Some("config_drift".to_owned()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(combined.len(), 1);
        assert_eq!(combined[0].check_name, "config_drift");
        assert_eq!(combined[0].run_id.as_deref(), Some("run-B"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_recent_respects_limit_and_default_ceiling() {
        let db = fresh_db();
        let repo = DoctorFindingRepository::new(db);
        for i in 0..(MAX_RECENT_FINDINGS + 25) {
            let mut f = new_finding("config_drift", severity::INFO);
            f.detail = Some(format!("finding #{i}"));
            repo.insert(f).await.unwrap();
        }
        // Default ceiling caps even with no explicit limit.
        let default_capped = repo
            .list_recent(RecentDoctorFindings::default())
            .await
            .unwrap();
        assert_eq!(default_capped.len(), MAX_RECENT_FINDINGS);

        // Explicit small limit honoured.
        let small = repo
            .list_recent(RecentDoctorFindings {
                limit: Some(7),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(small.len(), 7);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_recent_filters_by_since_lower_bound() {
        let db = fresh_db();
        let repo = DoctorFindingRepository::new(db);

        // Two findings for the same check. The ms-resolution `created_at`
        // (UUIDv7 timestamp + a tiny sleep) lets us derive a `since` that
        // sits between them deterministically.
        let first = repo
            .insert(new_finding("config_drift", severity::INFO))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(8)).await;
        let second = repo
            .insert(new_finding("config_drift", severity::WARN))
            .await
            .unwrap();
        // A third finding for an unrelated check that should not affect the
        // since-filtered scan.
        tokio::time::sleep(std::time::Duration::from_millis(8)).await;
        let third = repo
            .insert(new_finding("zombie_reaper", severity::CRITICAL))
            .await
            .unwrap();

        // `since` strictly after the first finding's created_at → the
        // first one is excluded, but both subsequent rows survive.
        let since = second.created_at.clone();
        let after_first = repo
            .list_recent(RecentDoctorFindings {
                since: Some(since.clone()),
                ..Default::default()
            })
            .await
            .unwrap();
        let after_first_ids: Vec<&str> = after_first.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(after_first_ids, vec![third.id.as_str(), second.id.as_str()]);
        assert!(!after_first_ids.contains(&first.id.as_str()));

        // `since` strictly after any persisted row → empty.
        // Use a far-future timestamp guaranteed to sort lexicographically
        // after any real wall-clock value (the column is a UTC ISO-8601
        // VARCHAR compared as a string).
        let none = repo
            .list_recent(RecentDoctorFindings {
                since: Some("2999-12-31T23:59:59.999Z".to_owned()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(none.is_empty());

        // `since` combined with `check_name` still applies both predicates
        // (the second finding matches; the unrelated third does not).
        let combined = repo
            .list_recent(RecentDoctorFindings {
                check_name: Some("config_drift".to_owned()),
                since: Some(since),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(combined.len(), 1);
        assert_eq!(combined[0].id, second.id);
        assert_eq!(combined[0].check_name, "config_drift");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn count_for_check_returns_match_count() {
        let db = fresh_db();
        let repo = DoctorFindingRepository::new(db);
        assert_eq!(repo.count_for_check("config_drift").await.unwrap(), 0);
        for _ in 0..3 {
            repo.insert(new_finding("config_drift", severity::WARN))
                .await
                .unwrap();
        }
        repo.insert(new_finding("zombie_reaper", severity::WARN))
            .await
            .unwrap();
        assert_eq!(repo.count_for_check("config_drift").await.unwrap(), 3);
        assert_eq!(repo.count_for_check("zombie_reaper").await.unwrap(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn severity_constraint_rejects_unknown_value() {
        let db = fresh_db();
        let repo = DoctorFindingRepository::new(db);
        let bad = NewDoctorFinding {
            run_id: None,
            check_name: "x".to_owned(),
            severity: "fatal".to_owned(),
            entity_ids: serde_json::json!([]),
            evidence: serde_json::json!({}),
            resolver_snapshot: None,
            detail: None,
        };
        // The DB CHECK constraint is what guards the value set; the
        // repository just forwards the error. The exact sqlx error string
        // is unstable across Postgres versions, so we just assert that
        // the insert failed (i.e. the DB rejected the unknown value).
        assert!(repo.insert(bad).await.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_optional_fields_default_safely() {
        let db = fresh_db();
        let repo = DoctorFindingRepository::new(db);
        let minimal = NewDoctorFinding {
            run_id: None,
            check_name: "config_drift".to_owned(),
            severity: severity::INFO.to_owned(),
            entity_ids: serde_json::json!([]),
            evidence: serde_json::json!({}),
            resolver_snapshot: None,
            detail: None,
        };
        let inserted = repo.insert(minimal).await.expect("insert");
        assert!(inserted.run_id.is_none());
        assert!(inserted.resolver_snapshot.is_none());
        assert!(inserted.detail.is_none());
        assert_eq!(inserted.entity_ids, serde_json::json!([]));
        assert_eq!(inserted.evidence, serde_json::json!({}));
    }

    fn keyed_retrieval_finding(check_name: &str, active_key: &str) -> KeyedDoctorFinding {
        let mut finding = new_finding(check_name, severity::ERROR);
        finding.entity_ids = serde_json::json!({"finding_key": active_key});
        finding.evidence = serde_json::json!({
            "refresh_timestamp": "2026-01-01T01:00:00Z",
            "payload": ["must", "remain", "unchanged"],
        });
        finding.resolver_snapshot = Some(serde_json::json!({
            "resolver": "retrieval_alarm",
            "inputs": {"refresh_timestamp": "2026-01-01T01:00:00Z"},
            "outputs": {"alarming": true},
        }));
        KeyedDoctorFinding {
            active_key: active_key.to_owned(),
            finding,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retrieval_reconciliation_preserves_alarm_rows_on_refresh_failure() {
        let repo = DoctorFindingRepository::new(fresh_db());
        let initial = repo
            .reconcile_retrieval_findings(
                vec![
                    keyed_retrieval_finding("memory.retrieval_zero_result", "project-a:dispatch"),
                    keyed_retrieval_finding(
                        "memory.injection_starvation",
                        "project-a:load_knowledge_context",
                    ),
                ],
                &[],
            )
            .await
            .expect("seed active retrieval alarms");
        assert_eq!(initial.created.len(), 2);
        assert!(initial.updated.is_empty());
        assert!(initial.resolved.is_empty());
        let zero_before = initial.created[0].clone();
        let starvation_before = initial.created[1].clone();

        // A whole refresh failure only reconciles the refresh-error key and
        // explicitly preserves every active retrieval alarm key.
        let refresh = repo
            .reconcile_retrieval_findings(
                vec![KeyedDoctorFinding {
                    active_key: "refresh".to_owned(),
                    finding: NewDoctorFinding {
                        run_id: Some("failed-refresh".to_owned()),
                        check_name: "memory.retrieval_health_refresh".to_owned(),
                        severity: severity::ERROR.to_owned(),
                        entity_ids: serde_json::json!([]),
                        evidence: serde_json::json!({
                            "error_class": "retrieval_health_refresh_failed",
                            "attempted_at": "2026-01-01T01:02:00Z",
                            "last_success_at": "2026-01-01T01:00:00Z",
                            "last_success_age_seconds": 120,
                            "detail": "injected repository refresh failure",
                        }),
                        resolver_snapshot: Some(serde_json::json!({
                            "resolver": "retrieval_health_refresh",
                            "outputs": {"healthy": false},
                        })),
                        detail: Some("injected repository refresh failure".to_owned()),
                    },
                }],
                &repo
                    .active_retrieval_alarm_keys()
                    .await
                    .expect("active preserve keys"),
            )
            .await
            .expect("persist refresh failure");
        assert_eq!(refresh.created.len(), 1);
        assert!(refresh.updated.is_empty());
        assert!(refresh.resolved.is_empty());
        assert_eq!(refresh.created[0].severity, severity::ERROR);
        assert_eq!(
            refresh.created[0].evidence["error_class"],
            "retrieval_health_refresh_failed"
        );
        assert_eq!(repo.get(&zero_before.id).await.unwrap(), Some(zero_before));
        assert_eq!(
            repo.get(&starvation_before.id).await.unwrap(),
            Some(starvation_before)
        );

        // A later healthy refresh emits no retrieval findings, resolving both
        // prior alarms and the refresh error through the same keyed path.
        let resolved = repo
            .reconcile_retrieval_findings(Vec::new(), &[])
            .await
            .expect("resolve healthy absences");
        assert_eq!(resolved.resolved.len(), 3);
        for row in initial.created.iter().chain(refresh.created.iter()) {
            assert_eq!(
                repo.get(&row.id)
                    .await
                    .expect("reloaded row")
                    .expect("row retained for history")
                    .status,
                "resolved"
            );
        }
    }
}

#[cfg(test)]
mod deduplication_tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn immutable_deduplication_key_keeps_original_evidence() {
        let repo = DoctorFindingRepository::new(Database::open_in_memory().unwrap());
        let first = repo
            .insert_ignore_duplicate(
                NewDoctorFinding {
                    run_id: Some("first".into()),
                    check_name: "proposal_spec_integrity_v1".into(),
                    severity: severity::ERROR.into(),
                    entity_ids: serde_json::json!(["proposal-1"]),
                    evidence: serde_json::json!({"body_sha256": "first"}),
                    resolver_snapshot: None,
                    detail: Some("first evidence".into()),
                },
                "proposal_spec_integrity_v1:proposal-1:1:v1",
            )
            .await
            .unwrap();
        let DeduplicatedDoctorFindingInsert::Inserted(first) = first else {
            panic!("first insert must create a row");
        };
        let repeated = repo
            .insert_ignore_duplicate(
                NewDoctorFinding {
                    run_id: Some("second".into()),
                    check_name: "proposal_spec_integrity_v1".into(),
                    severity: severity::WARN.into(),
                    entity_ids: serde_json::json!(["proposal-2"]),
                    evidence: serde_json::json!({"body_sha256": "second"}),
                    resolver_snapshot: None,
                    detail: Some("replacement evidence".into()),
                },
                "proposal_spec_integrity_v1:proposal-1:1:v1",
            )
            .await
            .unwrap();
        assert!(matches!(
            repeated,
            DeduplicatedDoctorFindingInsert::AlreadyPresent
        ));
        assert_eq!(
            repo.count_for_check("proposal_spec_integrity_v1")
                .await
                .unwrap(),
            1
        );
        let original = repo.get(&first.id).await.unwrap().unwrap();
        assert_eq!(
            original.evidence,
            serde_json::json!({"body_sha256": "first"})
        );
        assert_eq!(original.detail.as_deref(), Some("first evidence"));
    }
}
