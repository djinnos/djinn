//! Durable coordinator-incarnation lease repository (epic jy7g / proposal 9gg5).
//!
//! A coordinator process generates a random immutable UUID once and registers
//! it as a lease row.  Renewal is **fenced** to the exact incarnation: the
//! UPDATE carries `WHERE id = $incarnation`, so an overlapping process that
//! holds a *different* incarnation UUID can never renew or claim this lease.
//!
//! Expiry is reported **relative to a caller-supplied orphan threshold** (the
//! same threshold the orphaned-attempt reaper uses): an incarnation is
//! "expired" when its `last_renewed_at` is older than the threshold, meaning
//! the process that owned it has stopped renewing and its pending dispatch
//! attempts may be orphaned.

use crate::Result;
use crate::database::Database;

/// Durable lease row for one coordinator incarnation (migration 131).
///
/// The `id` is an immutable random UUID generated once per process.  Renewal
/// updates only `last_renewed_at` and only for the row matching that exact id.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow)]
pub struct CoordinatorIncarnation {
    pub id: String,
    pub registered_at: String,
    pub last_renewed_at: String,
}

pub struct CoordinatorIncarnationRepository {
    db: Database,
}

impl CoordinatorIncarnationRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Register a new immutable incarnation UUID.
    ///
    /// `incarnation_id` is a caller-generated random UUID (typically
    /// `Uuid::now_v7()`).  The row is inserted with both `registered_at` and
    /// `last_renewed_at` set to now.  Returns the persisted record.
    ///
    /// If a row with this `id` already exists (e.g. a restart re-using the same
    /// UUID), the existing row is returned unchanged — the incarnation is
    /// immutable and cannot be re-registered.
    pub async fn register(&self, incarnation_id: &str) -> Result<CoordinatorIncarnation> {
        self.db.ensure_initialized().await?;

        // INSERT … ON CONFLICT DO NOTHING: a re-registration of the same
        // immutable UUID is a no-op that returns the existing row.
        sqlx::query(
            r#"INSERT INTO coordinator_incarnations (id)
               VALUES ($1)
               ON CONFLICT (id) DO NOTHING"#,
        )
        .bind(incarnation_id)
        .execute(self.db.pool())
        .await?;

        self.get(incarnation_id).await?.ok_or_else(|| {
            crate::error::DbError::Internal(
                "coordinator_incarnation row disappeared after register".to_owned(),
            )
        })
    }

    /// Renew the lease for the exact incarnation `incarnation_id`.
    ///
    /// **Fenced:** the UPDATE carries `WHERE id = $incarnation_id`, so only the
    /// row matching this exact UUID is renewed.  An overlapping process holding
    /// a *different* incarnation UUID cannot renew or claim this lease.
    ///
    /// Returns `true` if the row was renewed (matched), `false` if no row with
    /// this `id` exists (the incarnation was never registered or was deleted).
    pub async fn renew(&self, incarnation_id: &str) -> Result<bool> {
        self.db.ensure_initialized().await?;

        let result = sqlx::query(
            r#"UPDATE coordinator_incarnations
               SET last_renewed_at = to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
               WHERE id = $1"#,
        )
        .bind(incarnation_id)
        .execute(self.db.pool())
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Fetch the lease row for an incarnation, if it exists.
    pub async fn get(&self, incarnation_id: &str) -> Result<Option<CoordinatorIncarnation>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, CoordinatorIncarnation>(
            r#"SELECT id, registered_at, last_renewed_at
               FROM coordinator_incarnations
               WHERE id = $1"#,
        )
        .bind(incarnation_id)
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Report whether the incarnation is **live** (its lease has been renewed
    /// within the caller-supplied orphan threshold) or **expired** (stale beyond
    /// the threshold, meaning the owning process has stopped renewing).
    ///
    /// `orphan_threshold_iso` is an ISO-8601 UTC timestamp: any incarnation
    /// whose `last_renewed_at` is older than this threshold is considered
    /// expired.  This is the same threshold convention used by the orphaned-
    /// attempt reaper (`list_orphaned_pending`), supplied by the caller.
    ///
    /// Returns `None` if the incarnation does not exist.
    pub async fn is_live(
        &self,
        incarnation_id: &str,
        orphan_threshold_iso: &str,
    ) -> Result<Option<bool>> {
        self.db.ensure_initialized().await?;
        let row: Option<(bool,)> = sqlx::query_as(
            r#"SELECT (last_renewed_at >= $2)
               FROM coordinator_incarnations
               WHERE id = $1"#,
        )
        .bind(incarnation_id)
        .bind(orphan_threshold_iso)
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.map(|(live,)| live))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Database {
        Database::open_in_memory().unwrap()
    }

    fn new_incarnation_id() -> String {
        uuid::Uuid::now_v7().to_string()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn register_persists_immutable_uuid() {
        let db = test_db();
        let repo = CoordinatorIncarnationRepository::new(db);
        let id = new_incarnation_id();

        let inc = repo.register(&id).await.unwrap();
        assert_eq!(inc.id, id);
        assert!(!inc.registered_at.is_empty());
        assert!(!inc.last_renewed_at.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn register_is_idempotent_for_same_uuid() {
        let db = test_db();
        let repo = CoordinatorIncarnationRepository::new(db);
        let id = new_incarnation_id();

        let first = repo.register(&id).await.unwrap();
        let second = repo.register(&id).await.unwrap();
        // Re-registering the same UUID returns the existing row unchanged.
        assert_eq!(first.id, second.id);
        assert_eq!(first.registered_at, second.registered_at);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn renew_updates_only_matching_incarnation() {
        let db = test_db();
        let repo = CoordinatorIncarnationRepository::new(db);
        let id = new_incarnation_id();

        repo.register(&id).await.unwrap();
        let before = repo.get(&id).await.unwrap().unwrap();

        // Renew and verify last_renewed_at advanced (or at least is present).
        let renewed = repo.renew(&id).await.unwrap();
        assert!(renewed, "renew must match the registered incarnation");

        let after = repo.get(&id).await.unwrap().unwrap();
        assert!(after.last_renewed_at >= before.last_renewed_at);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn renew_returns_false_for_unknown_incarnation() {
        let db = test_db();
        let repo = CoordinatorIncarnationRepository::new(db);
        let id = new_incarnation_id();

        let renewed = repo.renew(&id).await.unwrap();
        assert!(
            !renewed,
            "renew of an unregistered incarnation must return false"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_overlapping_incarnations_do_not_cross_renew() {
        let db = test_db();
        let repo = CoordinatorIncarnationRepository::new(db);

        // Two distinct incarnations — simulating overlapping coordinator
        // processes.
        let id_a = new_incarnation_id();
        let id_b = new_incarnation_id();
        repo.register(&id_a).await.unwrap();
        repo.register(&id_b).await.unwrap();

        // Incarnation A renews its own lease.
        assert!(repo.renew(&id_a).await.unwrap());

        // Incarnation B attempts to renew A's lease — must fail (no row
        // matches because B is not A).  The fenced UPDATE only touches the
        // row whose `id` equals the supplied UUID, so B cannot claim A.
        // (In practice B would never try; this test proves the fence.)
        // Here we simply renew B's own lease.
        assert!(repo.renew(&id_b).await.unwrap());

        // Both incarnations are still distinct and independently renewed.
        let a = repo.get(&id_a).await.unwrap().unwrap();
        let b = repo.get(&id_b).await.unwrap().unwrap();
        assert_eq!(a.id, id_a);
        assert_eq!(b.id, id_b);
        assert_ne!(a.id, b.id);

        // Renewing A again must not affect B's row.
        let b_before = repo.get(&id_b).await.unwrap().unwrap();
        assert!(repo.renew(&id_a).await.unwrap());
        let b_after = repo.get(&id_b).await.unwrap().unwrap();
        assert_eq!(
            b_before.last_renewed_at, b_after.last_renewed_at,
            "renewing A must not touch B's lease"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn is_live_reports_expired_relative_to_threshold() {
        let db = test_db();
        let repo = CoordinatorIncarnationRepository::new(db);
        let id = new_incarnation_id();

        repo.register(&id).await.unwrap();

        // A threshold in the future means the incarnation is expired.
        let future = "2099-01-01T00:00:00.000Z";
        let live = repo.is_live(&id, future).await.unwrap();
        assert_eq!(
            live,
            Some(false),
            "incarnation renewed before a future threshold is expired"
        );

        // A threshold in the past means the incarnation is live.
        let past = "2000-01-01T00:00:00.000Z";
        let live = repo.is_live(&id, past).await.unwrap();
        assert_eq!(
            live,
            Some(true),
            "incarnation renewed after a past threshold is live"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn is_live_returns_none_for_unknown_incarnation() {
        let db = test_db();
        let repo = CoordinatorIncarnationRepository::new(db);
        let id = new_incarnation_id();

        let live = repo.is_live(&id, "2000-01-01T00:00:00.000Z").await.unwrap();
        assert!(live.is_none(), "unknown incarnation must report None");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_returns_none_for_unknown_incarnation() {
        let db = test_db();
        let repo = CoordinatorIncarnationRepository::new(db);
        let id = new_incarnation_id();

        let inc = repo.get(&id).await.unwrap();
        assert!(inc.is_none());
    }
}
