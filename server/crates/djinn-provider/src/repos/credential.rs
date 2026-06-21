use uuid::Uuid;

use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::models::Credential;
use djinn_db::crypto;
use djinn_db::{Database, Result, ensure_db};

#[derive(Clone)]
pub struct CredentialRepository {
    db: Database,
    events: EventBus,
}

impl CredentialRepository {
    pub fn new(db: Database, events: EventBus) -> Self {
        Self { db, events }
    }

    /// Upsert a credential by `key_name`, owned by the *acting user* read from
    /// the `SESSION_USER_ID` task-local (`current_user_id()`). Encrypts
    /// `raw_value` before storage.
    ///
    /// When a request runs under an authenticated user's scope (chat / MCP /
    /// the connect flows), this stamps `owner_user_id` so the credential is
    /// private to that user. When no user context is present (background
    /// agents, local single-user dev), `owner_user_id` is NULL and the
    /// credential becomes the org-shared fallback — preserving the historical
    /// single-credential behaviour.
    ///
    /// Emits `CredentialCreated` on insert, `CredentialUpdated` on update.
    /// The event payload never includes the encrypted value.
    pub async fn set(
        &self,
        provider_id: &str,
        key_name: &str,
        raw_value: &str,
    ) -> Result<Credential> {
        let owner = djinn_core::auth_context::current_user_id();
        self.set_with_owner(provider_id, key_name, raw_value, owner.as_deref())
            .await
    }

    /// Upsert a credential with an explicit `owner_user_id`. Pass `None` to
    /// write/overwrite the org-shared fallback credential (the admin path);
    /// pass `Some(user_id)` to write a user-private credential.
    ///
    /// Upsert is scoped to the (key_name, owner) pair so a user's private
    /// credential never clobbers the org-shared one and vice-versa. The two
    /// partial unique indexes from migration 28 guarantee at most one row per
    /// scope, so the conflict targets below are well-defined.
    pub async fn set_with_owner(
        &self,
        provider_id: &str,
        key_name: &str,
        raw_value: &str,
        owner_user_id: Option<&str>,
    ) -> Result<Credential> {
        let encrypted = crypto::encrypt(raw_value)?;
        ensure_db!(self.db);

        let existing_id: Option<String> = self.scoped_id(key_name, owner_user_id).await?;
        let id = existing_id
            .clone()
            .unwrap_or_else(|| Uuid::now_v7().to_string());
        let is_new = existing_id.is_none();

        // The two partial unique indexes require distinct conflict targets:
        // `(key_name) WHERE owner_user_id IS NULL` for the org-shared row, and
        // `(key_name, owner_user_id) WHERE owner_user_id IS NOT NULL` for the
        // per-user rows. Branch on the owner so the planner picks the right
        // arbiter index.
        match owner_user_id {
            None => {
                sqlx::query!(
                    r#"INSERT INTO credentials (id, provider_id, key_name, owner_user_id,
                                              encrypted_value, created_at, updated_at)
                     VALUES ($1, $2, $3, NULL, $4,
                             to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                             to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
                     ON CONFLICT (key_name) WHERE owner_user_id IS NULL DO UPDATE SET
                         provider_id     = EXCLUDED.provider_id,
                         encrypted_value = EXCLUDED.encrypted_value,
                         updated_at      = EXCLUDED.updated_at"#,
                    id,
                    provider_id,
                    key_name,
                    encrypted,
                )
                .execute(self.db.pool())
                .await?;
            }
            Some(owner) => {
                sqlx::query!(
                    r#"INSERT INTO credentials (id, provider_id, key_name, owner_user_id,
                                              encrypted_value, created_at, updated_at)
                     VALUES ($1, $2, $3, $4, $5,
                             to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                             to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
                     ON CONFLICT (key_name, owner_user_id) WHERE owner_user_id IS NOT NULL DO UPDATE SET
                         provider_id     = EXCLUDED.provider_id,
                         encrypted_value = EXCLUDED.encrypted_value,
                         updated_at      = EXCLUDED.updated_at"#,
                    id,
                    provider_id,
                    key_name,
                    owner,
                    encrypted,
                )
                .execute(self.db.pool())
                .await?;
            }
        }

        // Reconnecting/replacing a credential clears any prior revoked mark so
        // the provider flips back to "connected" and parked tasks resume.
        // Non-macro (the macro upsert above is unchanged) so the offline sqlx
        // cache doesn't need regenerating for the new columns.
        let _ = sqlx::query(
            "UPDATE credentials SET revoked_at = NULL, revoked_reason = NULL WHERE id = $1",
        )
        .bind(&id)
        .execute(self.db.pool())
        .await;

        let cred = sqlx::query_as!(
            Credential,
            r#"SELECT id, provider_id, key_name, owner_user_id, created_at, updated_at
             FROM credentials WHERE id = $1"#,
            id,
        )
        .fetch_one(self.db.pool())
        .await?;

        if is_new {
            self.events
                .send(DjinnEventEnvelope::credential_created(&cred));
        } else {
            self.events
                .send(DjinnEventEnvelope::credential_updated(&cred));
        }

        Ok(cred)
    }

    /// Resolve the row id for a `(key_name, owner)` scope. Exact match: a NULL
    /// owner only matches the org-shared row; a non-NULL owner only matches
    /// that user's private row (no fallback — callers that want fallback use
    /// [`Self::get_decrypted_for_user`]).
    async fn scoped_id(
        &self,
        key_name: &str,
        owner_user_id: Option<&str>,
    ) -> Result<Option<String>> {
        let id = match owner_user_id {
            None => {
                sqlx::query_scalar!(
                    "SELECT id FROM credentials WHERE key_name = $1 AND owner_user_id IS NULL",
                    key_name
                )
                .fetch_optional(self.db.pool())
                .await?
            }
            Some(owner) => {
                sqlx::query_scalar!(
                    "SELECT id FROM credentials WHERE key_name = $1 AND owner_user_id = $2",
                    key_name,
                    owner
                )
                .fetch_optional(self.db.pool())
                .await?
            }
        };
        Ok(id)
    }

    /// List all credentials (every owner). Never returns raw key values.
    pub async fn list(&self) -> Result<Vec<Credential>> {
        ensure_db!(self.db);
        Ok(sqlx::query_as!(
            Credential,
            r#"SELECT id, provider_id, key_name, owner_user_id, created_at, updated_at
                 FROM credentials
                 ORDER BY provider_id, key_name"#,
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// List the credentials *visible to `user_id`*: that user's own rows plus
    /// the org-shared (`owner_user_id IS NULL`) fallback rows — the same
    /// visibility rule [`Self::resolve_id_for_user`] uses for resolution.
    /// `user_id = None` returns only the org-shared rows (local/background).
    ///
    /// This is what connection-status / connected-model listings must use so a
    /// provider connected via another user's private credential (e.g. their
    /// personal Codex sub) is NOT reported as connected for everyone. Never
    /// returns raw key values.
    pub async fn list_for_user(&self, user_id: Option<&str>) -> Result<Vec<Credential>> {
        ensure_db!(self.db);
        Ok(sqlx::query_as!(
            Credential,
            r#"SELECT id, provider_id, key_name, owner_user_id, created_at, updated_at
                 FROM credentials
                WHERE owner_user_id IS NULL OR owner_user_id = $1
                ORDER BY provider_id, key_name"#,
            user_id,
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Delete the credential `key_name` owned by the *acting user* (read from
    /// `current_user_id()`). With no user context this deletes the org-shared
    /// row. Disconnect flows are symmetric with [`Self::set`]: a user
    /// disconnecting a provider only removes their own credential, never
    /// another user's or (when acting as a user) the org-shared fallback.
    pub async fn delete(&self, key_name: &str) -> Result<bool> {
        let owner = djinn_core::auth_context::current_user_id();
        self.delete_for_owner(key_name, owner.as_deref()).await
    }

    /// Delete the credential for an explicit `(key_name, owner)` scope.
    /// Emits `CredentialDeleted` with the ID.
    pub async fn delete_for_owner(
        &self,
        key_name: &str,
        owner_user_id: Option<&str>,
    ) -> Result<bool> {
        ensure_db!(self.db);
        let deleted_id: Option<String> = self.scoped_id(key_name, owner_user_id).await?;

        if let Some(ref id) = deleted_id {
            sqlx::query!("DELETE FROM credentials WHERE id = $1", id)
                .execute(self.db.pool())
                .await?;
        }

        if let Some(id) = deleted_id {
            self.events
                .send(DjinnEventEnvelope::credential_deleted(&id));
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Mark the credential(s) for `(provider_id, owner)` as revoked with a
    /// human-readable `reason` — called when the provider rejected the
    /// credential with a 401 during a run. Persists `revoked_at`/`revoked_reason`
    /// (the F5-safe source of truth the provider catalog reads) and, when a row
    /// actually transitions to revoked, emits a `credential_revoked` event so the
    /// UI can prompt a reconnect live. Idempotent: the `revoked_at IS NULL` guard
    /// means re-revoking an already-revoked credential is a no-op (no event
    /// storm while a 401 loop drains). Non-macro on purpose — the new columns are
    /// not in the offline sqlx cache.
    ///
    /// Returns the number of rows newly marked revoked.
    pub async fn mark_revoked(
        &self,
        provider_id: &str,
        owner_user_id: Option<&str>,
        reason: &str,
    ) -> Result<u64> {
        ensure_db!(self.db);
        const SET: &str = "UPDATE credentials SET \
             revoked_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), \
             revoked_reason = $1 \
             WHERE provider_id = $2 AND revoked_at IS NULL AND ";
        let result = match owner_user_id {
            Some(owner) => {
                sqlx::query(&format!("{SET}owner_user_id = $3"))
                    .bind(reason)
                    .bind(provider_id)
                    .bind(owner)
                    .execute(self.db.pool())
                    .await?
            }
            None => {
                sqlx::query(&format!("{SET}owner_user_id IS NULL"))
                    .bind(reason)
                    .bind(provider_id)
                    .execute(self.db.pool())
                    .await?
            }
        };
        let n = result.rows_affected();
        if n > 0 {
            self.events.send(DjinnEventEnvelope::credential_revoked(
                owner_user_id,
                provider_id,
                reason,
            ));
        }
        Ok(n)
    }

    /// Whether the credential for `(provider_id, owner)` is currently marked
    /// revoked. Exact-owner (no org-shared fallback) — used by the OAuth
    /// reconnect path to decide whether to start a fresh device flow instead of
    /// short-circuiting on a dead token.
    pub async fn is_revoked_for_owner(
        &self,
        provider_id: &str,
        owner_user_id: Option<&str>,
    ) -> Result<bool> {
        ensure_db!(self.db);
        let id: Option<String> = match owner_user_id {
            Some(owner) => {
                sqlx::query_scalar::<_, String>(
                    "SELECT id FROM credentials \
                     WHERE provider_id = $1 AND owner_user_id = $2 AND revoked_at IS NOT NULL",
                )
                .bind(provider_id)
                .bind(owner)
                .fetch_optional(self.db.pool())
                .await?
            }
            None => {
                sqlx::query_scalar::<_, String>(
                    "SELECT id FROM credentials \
                     WHERE provider_id = $1 AND owner_user_id IS NULL AND revoked_at IS NOT NULL",
                )
                .bind(provider_id)
                .fetch_optional(self.db.pool())
                .await?
            }
        };
        Ok(id.is_some())
    }

    /// Revoked credentials visible to `user_id` (own rows + org-shared fallback)
    /// as `(provider_id, revoked_reason)`. The provider catalog uses this to
    /// render a provider as "Disconnected — <reason>" after a 401, persistently
    /// (survives a page reload, unlike the live `credential_revoked` event).
    pub async fn list_revoked_for_user(
        &self,
        user_id: Option<&str>,
    ) -> Result<Vec<(String, String)>> {
        ensure_db!(self.db);
        let rows = match user_id {
            Some(uid) => {
                sqlx::query_as::<_, (String, String)>(
                    "SELECT provider_id, COALESCE(revoked_reason, '') FROM credentials \
                     WHERE revoked_at IS NOT NULL AND (owner_user_id = $1 OR owner_user_id IS NULL)",
                )
                .bind(uid)
                .fetch_all(self.db.pool())
                .await?
            }
            None => {
                sqlx::query_as::<_, (String, String)>(
                    "SELECT provider_id, COALESCE(revoked_reason, '') FROM credentials \
                     WHERE revoked_at IS NOT NULL AND owner_user_id IS NULL",
                )
                .fetch_all(self.db.pool())
                .await?
            }
        };
        Ok(rows)
    }

    /// Delete the *acting user's* credentials for a given `provider_id` (the
    /// "disconnect provider" path). Scoped by `current_user_id()`: an
    /// authenticated user only removes their own rows — they can NEVER wipe
    /// another user's API keys, and (when acting as a user) they don't touch
    /// the org-shared fallback either. With no user context (local single-user
    /// dev / background) this removes the org-shared rows, preserving the
    /// historical "remove all" feel for that deployment shape.
    ///
    /// Returns the number of rows deleted. Emits `CredentialDeleted` for each.
    pub async fn delete_by_provider(&self, provider_id: &str) -> Result<u64> {
        ensure_db!(self.db);
        let owner = djinn_core::auth_context::current_user_id();

        let ids: Vec<String> =
            match owner.as_deref() {
                Some(uid) => {
                    sqlx::query_scalar!(
                        "SELECT id FROM credentials WHERE provider_id = $1 AND owner_user_id = $2",
                        provider_id,
                        uid
                    )
                    .fetch_all(self.db.pool())
                    .await?
                }
                None => sqlx::query_scalar!(
                    "SELECT id FROM credentials WHERE provider_id = $1 AND owner_user_id IS NULL",
                    provider_id
                )
                .fetch_all(self.db.pool())
                .await?,
            };

        if ids.is_empty() {
            return Ok(0);
        }

        // Delete by the exact id set we just resolved (keeps the scoping logic
        // in one place rather than duplicating the owner predicate).
        let mut deleted = 0u64;
        for id in &ids {
            let r = sqlx::query!("DELETE FROM credentials WHERE id = $1", id)
                .execute(self.db.pool())
                .await?;
            deleted += r.rows_affected();
        }

        for id in ids {
            self.events
                .send(DjinnEventEnvelope::credential_deleted(&id));
        }

        Ok(deleted)
    }

    /// Check whether a credential `key_name` is resolvable for the acting user
    /// (their own row, else the org-shared fallback), without decrypting.
    pub async fn exists(&self, key_name: &str) -> Result<bool> {
        let owner = djinn_core::auth_context::current_user_id();
        self.exists_for_user(key_name, owner.as_deref()).await
    }

    /// Check whether `key_name` is resolvable for `user_id` (their own row,
    /// else the org-shared fallback), without decrypting.
    pub async fn exists_for_user(&self, key_name: &str, user_id: Option<&str>) -> Result<bool> {
        ensure_db!(self.db);
        let found: Option<String> = self.resolve_id_for_user(key_name, user_id).await?;
        Ok(found.is_some())
    }

    /// Resolve the credential row id for `key_name` honouring the user-then-org
    /// fallback: prefer the acting user's private row, otherwise the org-shared
    /// (`owner_user_id IS NULL`) row. Returns `None` when neither exists.
    async fn resolve_id_for_user(
        &self,
        key_name: &str,
        user_id: Option<&str>,
    ) -> Result<Option<String>> {
        // Try the user's own credential first.
        if let Some(uid) = user_id
            && let Some(id) = sqlx::query_scalar!(
                "SELECT id FROM credentials WHERE key_name = $1 AND owner_user_id = $2",
                key_name,
                uid
            )
            .fetch_optional(self.db.pool())
            .await?
        {
            return Ok(Some(id));
        }
        // Fall back to the org-shared row.
        let id = sqlx::query_scalar!(
            "SELECT id FROM credentials WHERE key_name = $1 AND owner_user_id IS NULL",
            key_name
        )
        .fetch_optional(self.db.pool())
        .await?;
        Ok(id)
    }

    /// Return the raw encrypted ciphertext blob for `key_name` without
    /// decrypting it.
    ///
    /// Exists as a deliberate test fixture: contract tests assert that the
    /// stored value is actually encrypted (non-empty and distinct from
    /// plaintext). Production code should use [`Self::get_decrypted`].
    pub async fn get_encrypted_raw(&self, key_name: &str) -> Result<Option<Vec<u8>>> {
        let owner = djinn_core::auth_context::current_user_id();
        self.get_encrypted_raw_for_user(key_name, owner.as_deref())
            .await
    }

    /// Owner-aware variant of [`Self::get_encrypted_raw`] (test fixture).
    pub async fn get_encrypted_raw_for_user(
        &self,
        key_name: &str,
        user_id: Option<&str>,
    ) -> Result<Option<Vec<u8>>> {
        ensure_db!(self.db);
        let Some(id) = self.resolve_id_for_user(key_name, user_id).await? else {
            return Ok(None);
        };
        let blob: Option<Vec<u8>> =
            sqlx::query_scalar!("SELECT encrypted_value FROM credentials WHERE id = $1", id)
                .fetch_optional(self.db.pool())
                .await?;
        Ok(blob)
    }

    /// Decrypt and return the raw API key for `key_name`, resolved against the
    /// *acting user* read from the `SESSION_USER_ID` task-local
    /// (`current_user_id()`): the user's own credential is preferred, falling
    /// back to the org-shared (`owner_user_id IS NULL`) credential.
    ///
    /// Callers that must resolve against a specific user without relying on the
    /// task-local (e.g. the worker dispatch path stamping the task creator's
    /// id) should use [`Self::get_decrypted_for_user`] directly.
    ///
    /// Called by `AgentSupervisor` at dispatch time to obtain the key for
    /// provider creation. Never exposed via MCP tools.
    pub async fn get_decrypted(&self, key_name: &str) -> Result<Option<String>> {
        let user_id = djinn_core::auth_context::current_user_id();
        self.get_decrypted_for_user(key_name, user_id.as_deref())
            .await
    }

    /// Decrypt and return the raw API key for `key_name`, resolved against an
    /// explicit `user_id`: prefer that user's own credential, otherwise the
    /// org-shared (`owner_user_id IS NULL`) fallback. `user_id = None` resolves
    /// only the org-shared credential.
    ///
    /// User A can never read user B's credential through this method: it only
    /// ever returns A's own row or the org-shared row.
    pub async fn get_decrypted_for_user(
        &self,
        key_name: &str,
        user_id: Option<&str>,
    ) -> Result<Option<String>> {
        ensure_db!(self.db);
        let Some(id) = self.resolve_id_for_user(key_name, user_id).await? else {
            return Ok(None);
        };
        let blob: Option<Vec<u8>> =
            sqlx::query_scalar!("SELECT encrypted_value FROM credentials WHERE id = $1", id)
                .fetch_optional(self.db.pool())
                .await?;

        match blob {
            Some(b) => Ok(Some(crypto::decrypt(&b)?)),
            None => Ok(None),
        }
    }

    /// Decrypt and return the raw value for `key_name` owned *exactly* by
    /// `user_id` — **no** org-shared (`owner_user_id IS NULL`) fallback. Pass
    /// `None` to read only the org-shared row.
    ///
    /// Unlike [`Self::get_decrypted_for_user`] (which prefers the user's row but
    /// falls back to the org-shared one), this asks the strict question "does
    /// *this* identity already hold its own value?". Use it for "start an OAuth
    /// connect flow" short-circuits: an admin connecting a provider on the
    /// another user's behalf must not latch onto their *own* (or the
    /// org-shared) token and wrongly report the target as already connected.
    pub async fn get_decrypted_exact_owner(
        &self,
        key_name: &str,
        user_id: Option<&str>,
    ) -> Result<Option<String>> {
        ensure_db!(self.db);
        let Some(id) = self.scoped_id(key_name, user_id).await? else {
            return Ok(None);
        };
        let blob: Option<Vec<u8>> =
            sqlx::query_scalar!("SELECT encrypted_value FROM credentials WHERE id = $1", id)
                .fetch_optional(self.db.pool())
                .await?;

        match blob {
            Some(b) => Ok(Some(crypto::decrypt(&b)?)),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::events::EventBus;
    use tokio::sync::broadcast;

    fn event_bus_for(tx: &broadcast::Sender<DjinnEventEnvelope>) -> EventBus {
        let tx = tx.clone();
        EventBus::new(move |event| {
            let _ = tx.send(event);
        })
    }

    fn make_repo() -> CredentialRepository {
        let db = Database::open_in_memory().expect("failed to create test database");
        CredentialRepository::new(db, EventBus::noop())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_and_list() {
        let repo = make_repo();
        let cred = repo
            .set("anthropic", "ANTHROPIC_API_KEY", "sk-test")
            .await
            .unwrap();
        assert_eq!(cred.provider_id, "anthropic");
        assert_eq!(cred.key_name, "ANTHROPIC_API_KEY");

        let list = repo.list().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].key_name, "ANTHROPIC_API_KEY");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_emits_created_then_updated() {
        let db = Database::open_in_memory().expect("failed to create test database");
        let (tx, mut rx) = broadcast::channel(1024);
        let repo = CredentialRepository::new(db, event_bus_for(&tx));

        repo.set("anthropic", "ANTHROPIC_API_KEY", "key-v1")
            .await
            .unwrap();
        let ev1 = rx.recv().await.unwrap();
        assert_eq!(ev1.entity_type, "credential");
        assert_eq!(ev1.action, "created");

        repo.set("anthropic", "ANTHROPIC_API_KEY", "key-v2")
            .await
            .unwrap();
        let ev2 = rx.recv().await.unwrap();
        assert_eq!(ev2.entity_type, "credential");
        assert_eq!(ev2.action, "updated");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_decrypted_round_trip() {
        let repo = make_repo();
        repo.set("openai", "OPENAI_API_KEY", "sk-secret-value")
            .await
            .unwrap();

        let decrypted = repo.get_decrypted("OPENAI_API_KEY").await.unwrap();
        assert_eq!(decrypted.as_deref(), Some("sk-secret-value"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_decrypted_missing_returns_none() {
        let repo = make_repo();
        let result = repo.get_decrypted("NO_SUCH_KEY").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_removes_and_emits_event() {
        let db = Database::open_in_memory().expect("failed to create test database");
        let (tx, mut rx) = broadcast::channel(1024);
        let repo = CredentialRepository::new(db, event_bus_for(&tx));

        let cred = repo
            .set("anthropic", "ANTHROPIC_API_KEY", "val")
            .await
            .unwrap();
        let _created = rx.recv().await.unwrap(); // consume CredentialCreated

        let deleted = repo.delete("ANTHROPIC_API_KEY").await.unwrap();
        assert!(deleted);

        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.entity_type, "credential");
        assert_eq!(ev.action, "deleted");
        assert_eq!(ev.payload["id"].as_str().unwrap(), cred.id);

        let list = repo.list().await.unwrap();
        assert!(list.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_nonexistent_returns_false() {
        let repo = make_repo();
        let deleted = repo.delete("NO_SUCH_KEY").await.unwrap();
        assert!(!deleted);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upsert_keeps_unique_key_name() {
        let repo = make_repo();
        repo.set("anthropic", "ANTHROPIC_API_KEY", "v1")
            .await
            .unwrap();
        repo.set("anthropic", "ANTHROPIC_API_KEY", "v2")
            .await
            .unwrap();

        let list = repo.list().await.unwrap();
        assert_eq!(list.len(), 1, "upsert must not create duplicate rows");

        let v = repo.get_decrypted("ANTHROPIC_API_KEY").await.unwrap();
        assert_eq!(v.as_deref(), Some("v2"));
    }

    // ── Per-user isolation (migration 28) ──────────────────────────────────

    /// Seed a real `users` row so the `credentials.owner_user_id` FK holds.
    async fn seed_user(db: &Database, github_id: i64, login: &str) -> String {
        db.ensure_initialized().await.unwrap();
        djinn_db::UserRepository::new(db.clone())
            .upsert_from_github(github_id, login, None, None)
            .await
            .unwrap()
            .id
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn user_credential_is_preferred_over_org_shared() {
        let db = Database::open_in_memory().unwrap();
        let user_a = seed_user(&db, 1001, "alice").await;
        let repo = CredentialRepository::new(db.clone(), EventBus::noop());

        // Org-shared fallback (no owner).
        repo.set_with_owner("openai", "OPENAI_API_KEY", "sk-org-shared", None)
            .await
            .unwrap();
        // Alice's private credential.
        repo.set_with_owner("openai", "OPENAI_API_KEY", "sk-alice", Some(&user_a))
            .await
            .unwrap();

        // Both rows coexist under the same key_name.
        assert_eq!(repo.list().await.unwrap().len(), 2);

        // Alice resolves her own.
        let alice = repo
            .get_decrypted_for_user("OPENAI_API_KEY", Some(&user_a))
            .await
            .unwrap();
        assert_eq!(alice.as_deref(), Some("sk-alice"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn falls_back_to_org_shared_when_user_has_none() {
        let db = Database::open_in_memory().unwrap();
        let user_b = seed_user(&db, 1002, "bob").await;
        let repo = CredentialRepository::new(db.clone(), EventBus::noop());

        repo.set_with_owner("openai", "OPENAI_API_KEY", "sk-org-shared", None)
            .await
            .unwrap();

        // Bob has no private credential → resolves the org-shared one.
        let resolved = repo
            .get_decrypted_for_user("OPENAI_API_KEY", Some(&user_b))
            .await
            .unwrap();
        assert_eq!(resolved.as_deref(), Some("sk-org-shared"));

        // None acting-user also resolves org-shared (worker/local-dev path).
        let resolved_none = repo
            .get_decrypted_for_user("OPENAI_API_KEY", None)
            .await
            .unwrap();
        assert_eq!(resolved_none.as_deref(), Some("sk-org-shared"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn user_a_cannot_read_user_b_credential() {
        let db = Database::open_in_memory().unwrap();
        let user_a = seed_user(&db, 2001, "alice2").await;
        let user_b = seed_user(&db, 2002, "bob2").await;
        let repo = CredentialRepository::new(db.clone(), EventBus::noop());

        // Only Bob has a credential; no org-shared fallback exists.
        repo.set_with_owner("openai", "OPENAI_API_KEY", "sk-bob-secret", Some(&user_b))
            .await
            .unwrap();

        // Alice resolves nothing — she cannot see Bob's private credential,
        // and there is no org-shared row to fall back to.
        let alice = repo
            .get_decrypted_for_user("OPENAI_API_KEY", Some(&user_a))
            .await
            .unwrap();
        assert!(
            alice.is_none(),
            "user A must not resolve user B's private credential"
        );

        // Bob still resolves his own.
        let bob = repo
            .get_decrypted_for_user("OPENAI_API_KEY", Some(&user_b))
            .await
            .unwrap();
        assert_eq!(bob.as_deref(), Some("sk-bob-secret"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_stamps_owner_from_task_local() {
        use djinn_core::auth_context::SESSION_USER_ID;
        let db = Database::open_in_memory().unwrap();
        let user_a = seed_user(&db, 3001, "carol").await;
        let repo = CredentialRepository::new(db.clone(), EventBus::noop());

        // Under a user scope, `set` (the connect-flow entry point) stamps
        // owner_user_id automatically.
        SESSION_USER_ID
            .scope(Some(user_a.clone()), async {
                repo.set("openai", "OPENAI_API_KEY", "sk-carol")
                    .await
                    .unwrap();
            })
            .await;

        let owner: Option<String> = sqlx::query_scalar!(
            "SELECT owner_user_id FROM credentials WHERE key_name = $1",
            "OPENAI_API_KEY"
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(owner.as_deref(), Some(user_a.as_str()));

        // Carol resolves her own credential via the task-local-driven getter.
        let resolved = SESSION_USER_ID
            .scope(Some(user_a.clone()), async {
                repo.get_decrypted("OPENAI_API_KEY").await.unwrap()
            })
            .await;
        assert_eq!(resolved.as_deref(), Some("sk-carol"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_by_provider_only_removes_acting_users_rows() {
        use djinn_core::auth_context::SESSION_USER_ID;
        let db = Database::open_in_memory().unwrap();
        let alice = seed_user(&db, 5101, "erin").await;
        let bob = seed_user(&db, 5102, "frank").await;
        let repo = CredentialRepository::new(db.clone(), EventBus::noop());

        repo.set_with_owner("openai", "OPENAI_API_KEY", "org", None)
            .await
            .unwrap();
        repo.set_with_owner("openai", "OPENAI_API_KEY", "alice-key", Some(&alice))
            .await
            .unwrap();
        repo.set_with_owner("openai", "OPENAI_API_KEY", "bob-key", Some(&bob))
            .await
            .unwrap();

        // Alice disconnects openai → only her row goes.
        let deleted = SESSION_USER_ID
            .scope(Some(alice.clone()), async {
                repo.delete_by_provider("openai").await.unwrap()
            })
            .await;
        assert_eq!(deleted, 1);

        // Bob's and the org-shared rows survive.
        assert_eq!(
            repo.get_decrypted_for_user("OPENAI_API_KEY", Some(&bob))
                .await
                .unwrap()
                .as_deref(),
            Some("bob-key")
        );
        assert_eq!(
            repo.get_decrypted_for_user("OPENAI_API_KEY", None)
                .await
                .unwrap()
                .as_deref(),
            Some("org")
        );
        // Alice now resolves the org-shared fallback (her private row is gone).
        assert_eq!(
            repo.get_decrypted_for_user("OPENAI_API_KEY", Some(&alice))
                .await
                .unwrap()
                .as_deref(),
            Some("org")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn private_and_org_credentials_independently_upsert() {
        let db = Database::open_in_memory().unwrap();
        let user_a = seed_user(&db, 4001, "dave").await;
        let repo = CredentialRepository::new(db.clone(), EventBus::noop());

        repo.set_with_owner("openai", "OPENAI_API_KEY", "org-v1", None)
            .await
            .unwrap();
        repo.set_with_owner("openai", "OPENAI_API_KEY", "dave-v1", Some(&user_a))
            .await
            .unwrap();
        // Re-upsert each scope independently.
        repo.set_with_owner("openai", "OPENAI_API_KEY", "org-v2", None)
            .await
            .unwrap();
        repo.set_with_owner("openai", "OPENAI_API_KEY", "dave-v2", Some(&user_a))
            .await
            .unwrap();

        assert_eq!(
            repo.list().await.unwrap().len(),
            2,
            "still exactly two rows"
        );
        assert_eq!(
            repo.get_decrypted_for_user("OPENAI_API_KEY", None)
                .await
                .unwrap()
                .as_deref(),
            Some("org-v2")
        );
        assert_eq!(
            repo.get_decrypted_for_user("OPENAI_API_KEY", Some(&user_a))
                .await
                .unwrap()
                .as_deref(),
            Some("dave-v2")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_for_user_returns_own_plus_org_shared_only() {
        use std::collections::HashSet;
        let db = Database::open_in_memory().unwrap();
        let user_a = seed_user(&db, 6001, "gina").await;
        let user_b = seed_user(&db, 6002, "hank").await;
        let repo = CredentialRepository::new(db.clone(), EventBus::noop());

        repo.set_with_owner("openai", "OPENAI_API_KEY", "org", None)
            .await
            .unwrap(); // org-shared fallback
        repo.set_with_owner(
            "chatgpt_codex",
            "__OAUTH_CHATGPT_CODEX",
            "a-codex",
            Some(&user_a),
        )
        .await
        .unwrap(); // A's personal Codex sub
        repo.set_with_owner("anthropic", "ANTHROPIC_API_KEY", "b-key", Some(&user_b))
            .await
            .unwrap(); // B's private key

        // A sees org-shared + own, never B's.
        let a = repo.list_for_user(Some(&user_a)).await.unwrap();
        let a_keys: HashSet<&str> = a.iter().map(|c| c.key_name.as_str()).collect();
        assert!(a_keys.contains("OPENAI_API_KEY"));
        assert!(a_keys.contains("__OAUTH_CHATGPT_CODEX"));
        assert!(
            !a_keys.contains("ANTHROPIC_API_KEY"),
            "user A must not see user B's private credential"
        );

        // B sees org-shared + own, never A's Codex.
        let b = repo.list_for_user(Some(&user_b)).await.unwrap();
        let b_keys: HashSet<&str> = b.iter().map(|c| c.key_name.as_str()).collect();
        assert!(b_keys.contains("OPENAI_API_KEY"));
        assert!(b_keys.contains("ANTHROPIC_API_KEY"));
        assert!(
            !b_keys.contains("__OAUTH_CHATGPT_CODEX"),
            "user B must not see user A's personal Codex credential"
        );

        // None (local/background) sees only the org-shared row.
        let none = repo.list_for_user(None).await.unwrap();
        assert_eq!(none.len(), 1);
        assert_eq!(none[0].key_name, "OPENAI_API_KEY");
    }

    /// End-to-end replica of the coordinator's `resolve_user_model_priority`
    /// body (real DB + repos + catalog), reproducing the exact staging setup:
    /// a user who selected `openai/gpt-5.5` and connected Codex (`chatgpt_codex`
    /// OAuth, which merges into `openai`) + an org-shared fireworks key. The
    /// per-method `#[cfg(test)]` stub means the live suite never exercises this,
    /// so we replicate it inline. The model MUST stay eligible.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn user_model_priority_keeps_openai_via_codex_merge() {
        use crate::catalog::CatalogService;
        use djinn_core::models::ModelLanes;
        use djinn_db::UserSettingsRepository;

        let db = Database::open_in_memory().unwrap();
        let uid = seed_user(&db, 9001, "fern").await;
        let repo = CredentialRepository::new(db.clone(), EventBus::noop());
        repo.set_with_owner("chatgpt_codex", "__OAUTH_CHATGPT_CODEX", "tok", Some(&uid))
            .await
            .unwrap();
        repo.set_with_owner("fireworks-ai", "FIREWORKS_API_KEY", "fk", None)
            .await
            .unwrap();
        UserSettingsRepository::new(db.clone())
            .upsert_lanes(
                &uid,
                &ModelLanes::from_flat(vec!["openai/gpt-5.5".to_string()]),
            )
            .await
            .unwrap();

        // ── replicate resolve_user_model_priority (implement lane = worker) ──
        let models = UserSettingsRepository::new(db.clone())
            .get(&uid)
            .await
            .unwrap()
            .unwrap()
            .lanes
            .unwrap()
            .for_role("worker")
            .to_vec();
        let creds = repo.list_for_user(Some(&uid)).await.unwrap();
        let connected = CatalogService::new().connected_provider_ids(&creds);
        let result: Vec<String> = models
            .into_iter()
            .filter(|m| {
                let p = m.split_once('/').map(|(p, _)| p).unwrap_or(m.as_str());
                connected.contains(p)
            })
            .collect();

        assert!(
            connected.contains("openai"),
            "codex merge should connect openai; connected={connected:?}"
        );
        assert_eq!(
            result,
            vec!["openai/gpt-5.5".to_string()],
            "openai/gpt-5.5 must remain eligible; connected={connected:?}"
        );
    }
}
