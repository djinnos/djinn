//! Proactive Codex OAuth keep-alive.
//!
//! OpenAI's ChatGPT/Codex OAuth issues a short-lived access token (~1h) plus a
//! **single-use, rotating** refresh token: every refresh returns a *new*
//! refresh token and the clock on the new one effectively resets (a sliding
//! window, community-reported at ~10-30 days before an unused refresh token
//! lapses server-side). Djinn only ever refreshed **lazily** — at dispatch time
//! or reactively after a mid-run 401 — so a credential that is connected but
//! sits *idle* never rotates its refresh token and silently dies once OpenAI
//! expires it. The owner then discovers a dead connection the next time they
//! dispatch work, long after the fact.
//!
//! This sweep keeps idle credentials alive. It runs leader-only on a slow
//! cadence, finds every Codex credential that has been idle past
//! `refresh_after_secs`, and refreshes it — rotating the refresh token so the
//! chain never lapses. When a refresh genuinely fails (the refresh token really
//! is dead), it marks the credential revoked, which surfaces a live
//! "reconnect ChatGPT/Codex" prompt to the owner *proactively* instead of on
//! their next dispatch.
//!
//! Concurrency: refreshes take the process-wide [`codex::CODEX_REFRESH_LOCK`] —
//! the same mutex the dispatch path uses — so the sweep and a concurrent
//! dispatch never double-spend the same single-use refresh token. The one
//! refresher that does *not* share this lock is the connect-flow silent refresh
//! (it can run on a non-leader pod). To stay correct against that, a failed
//! refresh is only treated as "dead" (→ revoke) when the stored refresh token
//! is unchanged; if a peer rotated it underneath us, the credential is still
//! live and we leave it alone.

use std::future::Future;
use std::pin::Pin;

use anyhow::Result;

use crate::oauth::codex::{self, CodexTokens};
use crate::repos::CredentialRepository;

/// Default idle threshold before a keep-alive refresh: ~3 days. Comfortably
/// inside even the shortest community-reported refresh-token lifetime (~10
/// days) while giving actively-used credentials — which refresh themselves at
/// dispatch — no reason to be touched here.
pub const DEFAULT_REFRESH_AFTER_SECS: i64 = 3 * 24 * 60 * 60;
const REFRESH_AFTER_ENV: &str = "DJINN_CODEX_KEEPALIVE_REFRESH_AFTER_SECS";

/// Provider id under which Codex credentials are stored (matches
/// [`CodexTokens::save_to_db`] and the revoked-mark queries).
const CODEX_PROVIDER_ID: &str = "chatgpt_codex";

/// Human-readable reason attached to a credential that failed keep-alive
/// refresh. Rendered by the provider catalog as the disconnect reason and
/// carried on the live `credential_revoked` event.
const REVOKE_REASON: &str = "ChatGPT/Codex session expired — reconnect to continue";

/// Outcome tally for one sweep. Every scanned credential lands in exactly one
/// of `refreshed` / `skipped_fresh` / `revoked` / `races` / `errors`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct KeepaliveStats {
    /// Non-revoked Codex credentials examined.
    pub scanned: usize,
    /// Idle credentials whose refresh token was rotated successfully.
    pub refreshed: usize,
    /// Credentials refreshed recently enough to skip.
    pub skipped_fresh: usize,
    /// Credentials whose refresh token was dead → marked revoked.
    pub revoked: usize,
    /// Refresh failures where a peer had already rotated the token (still live).
    pub races: usize,
    /// Transient errors (persist/DB failures) that left the credential as-is.
    pub errors: usize,
}

/// Parse the idle-threshold override, falling back to the default for missing,
/// unparseable, or non-positive values.
fn parse_refresh_after(raw: Option<&str>) -> i64 {
    raw.and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_REFRESH_AFTER_SECS)
}

/// Resolve the idle threshold from the environment.
pub fn refresh_after_secs_from_env() -> i64 {
    parse_refresh_after(std::env::var(REFRESH_AFTER_ENV).ok().as_deref())
}

/// A Codex credential is due for a keep-alive refresh when its access token has
/// been expired for at least `refresh_after_secs`. `expires_at` is stamped
/// `last_refresh + expires_in` (~1h), so this fires roughly `refresh_after_secs`
/// after the credential last rotated — i.e. only for genuinely idle ones.
/// Actively-used credentials refresh at dispatch and never reach the threshold.
pub(crate) fn is_refresh_due(tokens: &CodexTokens, refresh_after_secs: i64, now: i64) -> bool {
    now >= tokens.expires_at.saturating_add(refresh_after_secs)
}

/// The token-refresh exchange, boxed so it can be stubbed in tests. Returns the
/// new `CodexTokens` (post-rotation) *without* persisting them — the sweep owns
/// persistence so it can stamp the owning user.
type ExchangeFn =
    dyn Fn(CodexTokens) -> Pin<Box<dyn Future<Output = Result<CodexTokens>> + Send>> + Send + Sync;

/// Run one keep-alive sweep over every non-revoked Codex credential.
pub async fn run_codex_keepalive_sweep(repo: &CredentialRepository) -> KeepaliveStats {
    let refresh_after = refresh_after_secs_from_env();
    let exchange: Box<ExchangeFn> = Box::new(|cur: CodexTokens| {
        Box::pin(async move { codex::exchange_refresh(&cur).await })
            as Pin<Box<dyn Future<Output = Result<CodexTokens>> + Send>>
    });
    sweep_inner(repo, refresh_after, codex::now_secs(), exchange.as_ref()).await
}

/// Core sweep, parameterised on `now` and the refresh exchange for testing.
async fn sweep_inner(
    repo: &CredentialRepository,
    refresh_after_secs: i64,
    now: i64,
    exchange: &ExchangeFn,
) -> KeepaliveStats {
    let mut stats = KeepaliveStats::default();

    let creds = match repo.list().await {
        Ok(creds) => creds,
        Err(err) => {
            tracing::warn!(error = %err, "codex_keepalive: credential listing failed; skipping sweep");
            return stats;
        }
    };

    for cred in creds {
        if cred.key_name != codex::CODEX_OAUTH_DB_KEY {
            continue;
        }
        let owner = cred.owner_user_id.clone();

        // Skip already-revoked credentials — a dead refresh token only recovers
        // via a fresh sign-in, so re-refreshing would just burn an HTTP call
        // each tick (and mark_revoked below is idempotent anyway).
        if repo
            .is_revoked_for_owner(CODEX_PROVIDER_ID, owner.as_deref())
            .await
            .unwrap_or(false)
        {
            continue;
        }
        stats.scanned += 1;

        // Pre-lock staleness check so we don't serialize on fresh credentials.
        let Some(tokens) = CodexTokens::load_from_db_for_owner(repo, owner.as_deref()).await else {
            continue; // deleted between listing and read
        };
        if !is_refresh_due(&tokens, refresh_after_secs, now) {
            stats.skipped_fresh += 1;
            continue;
        }

        // Serialize with the dispatch-time refresh path: single-use rotating
        // refresh tokens must never be double-spent.
        let _guard = codex::CODEX_REFRESH_LOCK.lock().await;

        // Re-read under the lock — a dispatch may have refreshed while we waited.
        let current = CodexTokens::load_from_db_for_owner(repo, owner.as_deref())
            .await
            .unwrap_or(tokens);
        if !is_refresh_due(&current, refresh_after_secs, now) {
            stats.skipped_fresh += 1;
            continue;
        }

        match exchange(current.clone()).await {
            Ok(refreshed) => {
                // The leader has no `SESSION_USER_ID` task-local, so stamp the
                // owning user explicitly — otherwise a user-private token would
                // be rewritten as the org-shared row.
                let saved = djinn_core::auth_context::SESSION_USER_ID
                    .scope(owner.clone(), async { refreshed.save_to_db(repo).await })
                    .await;
                match saved {
                    Ok(_) => {
                        stats.refreshed += 1;
                        tracing::info!(owner = ?owner, "codex_keepalive: refreshed idle credential");
                    }
                    Err(err) => {
                        stats.errors += 1;
                        tracing::warn!(error = %err, owner = ?owner, "codex_keepalive: refreshed but persist failed");
                    }
                }
            }
            Err(err) => {
                // Distinguish a dead refresh token from a lost rotation race
                // with a non-lock-holding peer (the connect-flow refresh). Only
                // an *unchanged* stored refresh token means it is genuinely dead.
                let rotated_by_peer =
                    match CodexTokens::load_from_db_for_owner(repo, owner.as_deref()).await {
                        Some(after) => after.refresh_token != current.refresh_token,
                        None => false, // deleted → treat as unchanged (revoke is a no-op)
                    };
                if rotated_by_peer {
                    stats.races += 1;
                    tracing::debug!(owner = ?owner, "codex_keepalive: refresh lost a rotation race; credential still live");
                } else {
                    match repo
                        .mark_revoked(CODEX_PROVIDER_ID, owner.as_deref(), REVOKE_REASON)
                        .await
                    {
                        Ok(n) if n > 0 => {
                            stats.revoked += 1;
                            tracing::warn!(error = %err, owner = ?owner, "codex_keepalive: refresh token dead, marked credential revoked");
                        }
                        // Already revoked (idempotent) — nothing changed.
                        Ok(_) => {}
                        Err(mark_err) => {
                            stats.errors += 1;
                            tracing::warn!(error = %mark_err, owner = ?owner, "codex_keepalive: mark_revoked failed");
                        }
                    }
                }
            }
        }
    }

    if stats.refreshed > 0 || stats.revoked > 0 {
        tracing::info!(?stats, "codex_keepalive: sweep complete");
    } else {
        tracing::debug!(?stats, "codex_keepalive: sweep complete");
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::events::{DjinnEventEnvelope, EventBus};
    use djinn_db::Database;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::broadcast;

    const DAY: i64 = 24 * 60 * 60;

    fn make_repo() -> CredentialRepository {
        let db = Database::open_in_memory().expect("in-memory db");
        CredentialRepository::new(db, EventBus::noop())
    }

    fn tokens_with(refresh: &str, expires_at: i64) -> CodexTokens {
        CodexTokens {
            access_token: "access".into(),
            refresh_token: refresh.into(),
            id_token: None,
            expires_at,
            account_id: Some("acct".into()),
        }
    }

    /// Seed a Codex credential (org-shared unless `owner` is set) carrying the
    /// given token blob.
    async fn seed_codex(repo: &CredentialRepository, owner: Option<&str>, tokens: &CodexTokens) {
        let json = serde_json::to_string(tokens).unwrap();
        repo.set_with_owner(CODEX_PROVIDER_ID, codex::CODEX_OAUTH_DB_KEY, &json, owner)
            .await
            .unwrap();
    }

    async fn stored_tokens(repo: &CredentialRepository, owner: Option<&str>) -> CodexTokens {
        CodexTokens::load_from_db_for_owner(repo, owner)
            .await
            .expect("token present")
    }

    /// Exchange stub that always succeeds, returning a freshly-rotated token,
    /// and counts its invocations.
    fn ok_exchange(calls: Arc<AtomicUsize>) -> Box<ExchangeFn> {
        Box::new(move |_cur: CodexTokens| {
            calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Ok(tokens_with("rotated-new", 10_000_000_000)) })
                as Pin<Box<dyn Future<Output = Result<CodexTokens>> + Send>>
        })
    }

    /// Exchange stub that always fails, as a dead refresh token would.
    fn err_exchange(calls: Arc<AtomicUsize>) -> Box<ExchangeFn> {
        Box::new(move |_cur: CodexTokens| {
            calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Err(anyhow::anyhow!("invalid_grant")) })
                as Pin<Box<dyn Future<Output = Result<CodexTokens>> + Send>>
        })
    }

    #[test]
    fn parse_refresh_after_uses_positive_override_else_default() {
        assert_eq!(parse_refresh_after(Some("3600")), 3600);
        assert_eq!(parse_refresh_after(None), DEFAULT_REFRESH_AFTER_SECS);
        assert_eq!(parse_refresh_after(Some("0")), DEFAULT_REFRESH_AFTER_SECS);
        assert_eq!(parse_refresh_after(Some("-5")), DEFAULT_REFRESH_AFTER_SECS);
        assert_eq!(
            parse_refresh_after(Some("nope")),
            DEFAULT_REFRESH_AFTER_SECS
        );
    }

    #[test]
    fn is_refresh_due_fires_only_past_the_idle_threshold() {
        let now = 1_000_000_000;
        let after = 3 * DAY;
        // Access token expiring in the future → freshly refreshed → not due.
        assert!(!is_refresh_due(&tokens_with("r", now + 3600), after, now));
        // Expired 1 day ago → still inside the 3-day window → not due.
        assert!(!is_refresh_due(&tokens_with("r", now - DAY), after, now));
        // Expired well past the window → due.
        assert!(is_refresh_due(&tokens_with("r", now - 4 * DAY), after, now));
        // Exactly at the boundary → due.
        assert!(is_refresh_due(&tokens_with("r", now - 3 * DAY), after, now));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fresh_credential_is_skipped_without_calling_exchange() {
        let repo = make_repo();
        let now = 2_000_000_000;
        seed_codex(&repo, None, &tokens_with("r0", now + 3600)).await;

        let calls = Arc::new(AtomicUsize::new(0));
        let stats = sweep_inner(&repo, 3 * DAY, now, ok_exchange(calls.clone()).as_ref()).await;

        assert_eq!(calls.load(Ordering::SeqCst), 0, "exchange must not run");
        assert_eq!(stats.scanned, 1);
        assert_eq!(stats.skipped_fresh, 1);
        assert_eq!(stats.refreshed, 0);
        // Token untouched.
        assert_eq!(stored_tokens(&repo, None).await.refresh_token, "r0");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn idle_credential_is_refreshed_and_persisted() {
        let repo = make_repo();
        let now = 2_000_000_000;
        seed_codex(&repo, None, &tokens_with("r0", now - 5 * DAY)).await;

        let calls = Arc::new(AtomicUsize::new(0));
        let stats = sweep_inner(&repo, 3 * DAY, now, ok_exchange(calls.clone()).as_ref()).await;

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(stats.refreshed, 1);
        assert_eq!(stats.revoked, 0);
        // The rotated token is now the stored one.
        assert_eq!(
            stored_tokens(&repo, None).await.refresh_token,
            "rotated-new"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dead_refresh_token_marks_credential_revoked_and_emits_event() {
        let db = Database::open_in_memory().unwrap();
        let (tx, mut rx) = broadcast::channel(1024);
        let repo = CredentialRepository::new(
            db,
            EventBus::new(move |e: DjinnEventEnvelope| {
                let _ = tx.send(e);
            }),
        );
        let now = 2_000_000_000;
        seed_codex(&repo, None, &tokens_with("r0", now - 5 * DAY)).await;
        // Drain the credential_created event from seeding.
        let _ = rx.recv().await.unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let stats = sweep_inner(&repo, 3 * DAY, now, err_exchange(calls.clone()).as_ref()).await;

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(stats.revoked, 1);
        assert_eq!(stats.refreshed, 0);
        assert_eq!(stats.races, 0);
        assert!(
            repo.is_revoked_for_owner(CODEX_PROVIDER_ID, None)
                .await
                .unwrap(),
            "credential must be marked revoked"
        );
        // A live credential_revoked event fired so the UI can prompt reconnect.
        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.entity_type, "credential");
        assert_eq!(ev.action, "revoked");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lost_rotation_race_does_not_revoke() {
        // A peer (e.g. the connect-flow refresh) rotates the token *during* our
        // exchange, then our exchange fails. Because the stored refresh token
        // changed, we must NOT revoke.
        let repo = make_repo();
        let now = 2_000_000_000;
        seed_codex(&repo, None, &tokens_with("r0", now - 5 * DAY)).await;

        let peer_repo = repo.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = calls.clone();
        let racing_exchange: Box<ExchangeFn> = Box::new(move |_cur: CodexTokens| {
            calls2.fetch_add(1, Ordering::SeqCst);
            let peer_repo = peer_repo.clone();
            Box::pin(async move {
                // Peer rotates to a new refresh token, then our exchange fails.
                seed_codex(&peer_repo, None, &tokens_with("r1-peer", 10_000_000_000)).await;
                Err(anyhow::anyhow!("invalid_grant"))
            }) as Pin<Box<dyn Future<Output = Result<CodexTokens>> + Send>>
        });

        let stats = sweep_inner(&repo, 3 * DAY, now, racing_exchange.as_ref()).await;

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(stats.races, 1);
        assert_eq!(stats.revoked, 0);
        assert!(
            !repo
                .is_revoked_for_owner(CODEX_PROVIDER_ID, None)
                .await
                .unwrap(),
            "a raced refresh must not revoke a still-live credential"
        );
        assert_eq!(stored_tokens(&repo, None).await.refresh_token, "r1-peer");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn revoked_credentials_are_excluded_from_the_sweep() {
        let repo = make_repo();
        let now = 2_000_000_000;
        seed_codex(&repo, None, &tokens_with("r0", now - 5 * DAY)).await;
        repo.mark_revoked(CODEX_PROVIDER_ID, None, "prior 401")
            .await
            .unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let stats = sweep_inner(&repo, 3 * DAY, now, ok_exchange(calls.clone()).as_ref()).await;

        assert_eq!(stats.scanned, 0, "revoked rows are not scanned");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn user_private_credential_refresh_stays_on_its_own_row() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let uid = djinn_db::UserRepository::new(db.clone())
            .upsert_from_github(7001, "idle-user", None, None)
            .await
            .unwrap()
            .id;
        let repo = CredentialRepository::new(db.clone(), EventBus::noop());
        seed_codex(
            &repo,
            Some(&uid),
            &tokens_with("r0", 2_000_000_000 - 5 * DAY),
        )
        .await;

        let calls = Arc::new(AtomicUsize::new(0));
        let stats = sweep_inner(
            &repo,
            3 * DAY,
            2_000_000_000,
            ok_exchange(calls.clone()).as_ref(),
        )
        .await;

        assert_eq!(stats.refreshed, 1);
        // The rotated token is readable via an *exact-owner* load for this user,
        // which proves it landed on their private row (not the org-shared one)
        // and is still owned by them.
        assert_eq!(
            stored_tokens(&repo, Some(&uid)).await.refresh_token,
            "rotated-new"
        );
        // Exactly one Codex row exists and it is owned by this user — the
        // refresh neither created an org-shared row nor duplicated the record.
        let codex_rows: Vec<_> = repo
            .list()
            .await
            .unwrap()
            .into_iter()
            .filter(|c| c.key_name == codex::CODEX_OAUTH_DB_KEY)
            .collect();
        assert_eq!(codex_rows.len(), 1);
        assert_eq!(codex_rows[0].owner_user_id.as_deref(), Some(uid.as_str()));
    }
}
