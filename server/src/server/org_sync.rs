//! Periodic reconciliation of the locked GitHub org's membership.
//!
//! # Why this exists
//!
//! Phase 2 (`server/src/server/auth.rs`) gates OAuth logins on a live
//! `GET /user/memberships/orgs/{org}` check, but that only fires **at login
//! time**. A user who signs in, gets removed from the org an hour later, and
//! keeps holding their 30-day `djinn_session` cookie would still reach
//! protected handlers. Phase 3C closes that hole by periodically diffing the
//! local `users` table against the org's live member list and:
//!
//!   1. Flipping `users.is_member_of_org=false` for anyone who's disappeared
//!      from GitHub's list.
//!   2. Revoking every `user_auth_sessions` row for those users so their
//!      next request misses the cookie lookup and is bounced back through
//!      the OAuth flow — where the Phase 2 membership re-check will reject
//!      them.
//!
//! Users who are still in the org get `is_member_of_org=true` (idempotent
//! restore-after-rejoin) plus a `last_seen_at` bump.
//!
//! # GitHub API dependency
//!
//! Calls `GET /orgs/{org}/members?per_page=100`, authenticated with an
//! installation access token minted from the App's JWT. **This requires the
//! GitHub App to have the `Members: Read` organization permission**. Without
//! it GitHub returns `403` and this sync is a no-op — the sync logs a loud
//! warning pointing operators at their App's permission page.
//!
//! # Lifetime
//!
//! `start_org_member_sync(state)` spawns a detached tokio task that loops
//! every [`SYNC_INTERVAL`]. Errors during a single iteration are logged and
//! swallowed — the loop survives transient failures and retries on the next
//! tick. The task exits when the app's cancellation token fires.

use std::collections::HashSet;
use std::time::Duration;

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode as HttpStatus},
    response::{IntoResponse, Response},
    routing::post,
};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};

use crate::server::{AppState, authenticate};
use djinn_db::repositories::user::User;
use djinn_db::{OrgConfigRepository, SessionAuthRepository, UserRepository};
use djinn_provider::github_app::get_installation_token;

/// How often the background loop fires a reconciliation.
pub(super) const SYNC_INTERVAL: Duration = Duration::from_secs(60 * 60);

const GITHUB_API: &str = "https://api.github.com";
const USER_AGENT: &str = "djinn-server/0.1 (+https://github.com/djinnos/server)";

// ─── Public surface ───────────────────────────────────────────────────────────

/// One run's outcome, suitable for JSON serialisation in the admin endpoint
/// and for structured logging at info level.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// Total members fetched from `/orgs/{org}/members` (paginated sum).
    pub members_fetched: usize,
    /// Users who flipped `is_member_of_org` from false → true (rejoiners).
    pub users_flipped_active: usize,
    /// Users who flipped `is_member_of_org` from true → false (removed).
    pub users_flipped_inactive: usize,
    /// `user_auth_sessions` rows deleted across all newly-revoked users.
    pub sessions_revoked: usize,
    /// Surfaced when the call to GitHub failed entirely (network, auth,
    /// 403 permission). `None` indicates the sync completed; the other
    /// counters are zero when this is `Some`.
    pub error: Option<String>,
    /// Set when the deployment hasn't been bound to an org yet (no
    /// `org_config` row). Not an error — just means there's nothing to
    /// reconcile.
    pub skipped_reason: Option<String>,
}

impl SyncReport {
    fn skipped(reason: &str) -> Self {
        Self {
            skipped_reason: Some(reason.to_string()),
            ..Default::default()
        }
    }

    fn error(msg: String) -> Self {
        Self {
            error: Some(msg),
            ..Default::default()
        }
    }
}

/// Spawn the periodic background membership-sync task. Returns immediately;
/// the task lives until `state.cancel()` fires.
pub fn start_org_member_sync(state: AppState) {
    let cancel = state.cancel().clone();
    tokio::spawn(async move {
        // Skip the first tick — we want the first reconciliation to run
        // *after* one SYNC_INTERVAL, not immediately at boot (boot is busy
        // enough without stacking one more network call onto the critical
        // path, and a fresh deployment has no `org_config` to check anyway).
        let mut ticker = tokio::time::interval(SYNC_INTERVAL);
        ticker.tick().await;

        tracing::info!(
            interval_secs = SYNC_INTERVAL.as_secs(),
            "org_sync: background membership reconciliation started"
        );

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let report = sync_once(&state).await;
                    log_report(&report);
                }
                _ = cancel.cancelled() => {
                    tracing::info!("org_sync: cancellation received, stopping");
                    break;
                }
            }
        }
    });
}

/// Run a single reconciliation pass. Never returns `Err`; wraps every
/// failure mode into [`SyncReport::error`] / [`SyncReport::skipped_reason`]
/// so the caller (admin endpoint *or* background loop) can always surface
/// structured counters.
pub async fn sync_once(state: &AppState) -> SyncReport {
    // 1. Deployment must be bound to an org.
    let org_repo = OrgConfigRepository::new(state.db().clone());
    let cfg = match org_repo.get().await {
        Ok(Some(cfg)) => cfg,
        Ok(None) => {
            return SyncReport::skipped("deployment is not bound to a GitHub org yet");
        }
        Err(e) => {
            tracing::error!(error = %e, "org_sync: org_config read failed");
            return SyncReport::error(format!("org_config read failed: {e}"));
        }
    };

    // 2. Mint an installation token for the App's installation.
    let token = match get_installation_token(cfg.installation_id as u64).await {
        Ok(t) => t.token,
        Err(e) => {
            tracing::warn!(
                installation_id = cfg.installation_id,
                error = %e,
                "org_sync: failed to mint installation token"
            );
            return SyncReport::error(format!("installation token: {e}"));
        }
    };

    // 3. Fetch the org's member list (paginated).
    let members = match fetch_org_members(&token, &cfg.github_org_login).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                org = %cfg.github_org_login,
                error = %e,
                "org_sync: /orgs/{{org}}/members fetch failed — check that the \
                 GitHub App has the 'Members: Read' organization permission"
            );
            return SyncReport::error(format!("members fetch: {e}"));
        }
    };

    // 4 & 5. Diff against the local users table and apply updates.
    let users_repo = UserRepository::new(state.db().clone());
    let sessions_repo = SessionAuthRepository::new(state.db().clone());

    let local_users = match users_repo.list_all().await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "org_sync: users list failed");
            return SyncReport::error(format!("users list: {e}"));
        }
    };

    let plan = diff_membership(&local_users, &members);
    let mut report = SyncReport {
        members_fetched: members.len(),
        users_flipped_active: plan.to_activate.len(),
        users_flipped_inactive: plan.to_deactivate.len(),
        sessions_revoked: 0,
        error: None,
        skipped_reason: None,
    };

    for id in &plan.to_activate {
        if let Err(e) = users_repo.set_member_status(id, true).await {
            tracing::error!(user_id = %id, error = %e, "org_sync: activate failed");
        }
    }

    for id in &plan.present_members_to_touch {
        if let Err(e) = users_repo.touch_last_seen(id).await {
            // Non-fatal — last_seen_at is informational. Log at debug so a
            // DB blip doesn't drown the logs every hour.
            tracing::debug!(user_id = %id, error = %e, "org_sync: touch_last_seen failed");
        }
    }

    for id in &plan.to_deactivate {
        if let Err(e) = users_repo.set_member_status(id, false).await {
            tracing::error!(user_id = %id, error = %e, "org_sync: deactivate failed");
            continue;
        }
        match sessions_repo.delete_by_user_fk(id).await {
            Ok(n) => report.sessions_revoked += n as usize,
            Err(e) => {
                tracing::error!(
                    user_id = %id,
                    error = %e,
                    "org_sync: delete_by_user_fk failed (user marked inactive but sessions linger)"
                );
            }
        }
    }

    report
}

// ─── Internals ────────────────────────────────────────────────────────────────

/// One entry from `GET /orgs/{org}/members`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(super) struct GithubOrgMember {
    pub id: i64,
    pub login: String,
}

/// The pure diff between local state and the GitHub response. Separated out
/// so unit tests can exercise it without any HTTP or DB dependency.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct MembershipDiff {
    /// User `id`s (surrogate UUIDs) whose `is_member_of_org` should flip true.
    pub to_activate: Vec<String>,
    /// User `id`s whose `is_member_of_org` should flip false. Sessions for
    /// these must also be deleted.
    pub to_deactivate: Vec<String>,
    /// User `id`s already flagged as members AND still present on GitHub.
    /// Used only to bump `last_seen_at`.
    pub present_members_to_touch: Vec<String>,
}

/// Compute the set of DB mutations implied by a GitHub member list, without
/// performing any of them. Unit-testable core of [`sync_once`].
pub(super) fn diff_membership(
    local_users: &[User],
    github_members: &[GithubOrgMember],
) -> MembershipDiff {
    let present: HashSet<i64> = github_members.iter().map(|m| m.id).collect();

    let mut diff = MembershipDiff::default();
    for user in local_users {
        let is_present = present.contains(&user.github_id);
        match (user.is_member_of_org, is_present) {
            (false, true) => diff.to_activate.push(user.id.clone()),
            (true, false) => diff.to_deactivate.push(user.id.clone()),
            (true, true) => diff.present_members_to_touch.push(user.id.clone()),
            (false, false) => {
                // Already inactive and still not on the roster — no-op.
            }
        }
    }
    diff
}

/// Fetch every page of `GET /orgs/{org}/members` with pagination via the
/// `Link: <…>; rel="next"` header. Uses the installation bearer token.
async fn fetch_org_members(
    installation_token: &str,
    org_login: &str,
) -> Result<Vec<GithubOrgMember>, String> {
    let client = Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| format!("reqwest client: {e}"))?;

    let mut next = Some(format!(
        "{GITHUB_API}/orgs/{}/members?per_page=100",
        urlencode_path_segment(org_login),
    ));
    let mut out: Vec<GithubOrgMember> = Vec::new();

    // Cap pages so a runaway pagination loop can't hang the background task.
    let mut pages_seen = 0usize;
    const MAX_PAGES: usize = 100; // 100 * 100/page = 10 000 members, well above any plausible org.

    while let Some(url) = next.take() {
        pages_seen += 1;
        if pages_seen > MAX_PAGES {
            return Err(format!(
                "org_sync: aborting after {MAX_PAGES} pages — pagination loop?"
            ));
        }

        let resp = client
            .get(&url)
            .bearer_auth(installation_token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .map_err(|e| format!("http: {e}"))?;

        let status = resp.status();
        if status == StatusCode::FORBIDDEN {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!(
                "403 Forbidden from /orgs/{org_login}/members — the GitHub App \
                 likely lacks the 'Members: Read' organization permission. \
                 Update it at https://github.com/settings/apps/<slug>/permissions \
                 and re-install. Body: {body}"
            ));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("{status}: {body}"));
        }

        // Capture Link header before consuming the body.
        let link_header = resp
            .headers()
            .get(reqwest::header::LINK)
            .and_then(|h| h.to_str().ok())
            .map(str::to_string);

        let page: Vec<GithubOrgMember> =
            resp.json().await.map_err(|e| format!("decode page: {e}"))?;
        out.extend(page);

        next = link_header.as_deref().and_then(parse_next_link);
    }

    Ok(out)
}

/// Parse a GitHub `Link` header and return the URL whose `rel="next"`, if any.
///
/// Format (per RFC 5988):
///   `<https://api.github.com/…?page=2>; rel="next", <…>; rel="last"`
fn parse_next_link(header: &str) -> Option<String> {
    for segment in header.split(',') {
        let segment = segment.trim();
        let Some((target, params)) = segment.split_once(';') else {
            continue;
        };
        let target = target.trim();
        let target = target.strip_prefix('<')?.strip_suffix('>')?;
        for param in params.split(';') {
            let param = param.trim();
            // Accept `rel=next`, `rel="next"`.
            if let Some(rest) = param.strip_prefix("rel=") {
                let value = rest.trim_matches('"');
                if value == "next" {
                    return Some(target.to_string());
                }
            }
        }
    }
    None
}

/// Minimal percent-encoder for a single path segment (mirrors the helper in
/// `auth.rs` — copy-pasted so this module stays self-contained).
fn urlencode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        let c = *b;
        match c {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(c as char);
            }
            _ => out.push_str(&format!("%{c:02X}")),
        }
    }
    out
}

fn log_report(report: &SyncReport) {
    if let Some(reason) = &report.skipped_reason {
        tracing::info!(reason = %reason, "org_sync: skipped");
        return;
    }
    if let Some(err) = &report.error {
        tracing::warn!(error = %err, "org_sync: iteration failed");
        return;
    }
    tracing::info!(
        members_fetched = report.members_fetched,
        flipped_active = report.users_flipped_active,
        flipped_inactive = report.users_flipped_inactive,
        sessions_revoked = report.sessions_revoked,
        "org_sync: reconciliation complete"
    );
}

// ─── Admin trigger: POST /admin/org-sync ──────────────────────────────────────
//
// Operator-facing route for on-demand reconciliation (debugging, CI hooks,
// verifying after an App-permission change). Gated behind any authenticated
// session — v1 deliberately does not introduce a separate "org admin" flag,
// and the deployment is already locked to a single trusted GitHub org, so
// every signed-in user is by construction an org member who can be trusted
// to poke this endpoint. Revisit when per-user role flags arrive.

/// Build the `/admin/*` sub-router. Merged into the main app router in
/// `server/src/server/mod.rs`.
pub(super) fn router() -> Router<AppState> {
    Router::new().route("/admin/org-sync", post(trigger_sync_handler))
}

async fn trigger_sync_handler(State(state): State<AppState>, headers: HeaderMap) -> Response {
    // v1 gating: require *any* valid session cookie. See module-level
    // comment above for the rationale.
    match authenticate(&state, &headers).await {
        Ok(Some(_)) => {}
        Ok(None) => return HttpStatus::UNAUTHORIZED.into_response(),
        Err(e) => {
            tracing::error!(error = %e, "admin/org-sync: auth lookup failed");
            return HttpStatus::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    let report = sync_once(&state).await;
    log_report(&report);
    Json(report).into_response()
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_db::repositories::user::User;

    fn mk_user(id: &str, github_id: i64, login: &str, is_member: bool) -> User {
        User {
            id: id.to_string(),
            github_id,
            github_login: login.to_string(),
            github_name: None,
            github_avatar_url: None,
            is_member_of_org: is_member,
            last_seen_at: None,
            created_at: "2024-01-01T00:00:00.000Z".to_string(),
        }
    }

    fn mk_member(github_id: i64, login: &str) -> GithubOrgMember {
        GithubOrgMember {
            id: github_id,
            login: login.to_string(),
        }
    }

    /// Alice was flagged as a member and is still present on GitHub → touch.
    /// Bob was flagged as a member but disappeared from GitHub → deactivate.
    /// Carol was flagged inactive locally but has rejoined on GitHub → activate.
    /// Dave was flagged inactive and is still absent → no-op.
    #[test]
    fn diff_classifies_every_transition() {
        let local = vec![
            mk_user("u-alice", 1, "alice", true),
            mk_user("u-bob", 2, "bob", true),
            mk_user("u-carol", 3, "carol", false),
            mk_user("u-dave", 4, "dave", false),
        ];
        let gh = vec![mk_member(1, "alice"), mk_member(3, "carol")];

        let diff = diff_membership(&local, &gh);
        assert_eq!(diff.to_activate, vec!["u-carol".to_string()]);
        assert_eq!(diff.to_deactivate, vec!["u-bob".to_string()]);
        assert_eq!(diff.present_members_to_touch, vec!["u-alice".to_string()]);
    }

    /// Empty roster response revokes every currently-active user. Guards
    /// against the "App was uninstalled, GitHub returns []" failure mode
    /// doing nothing destructive by accident — if the loop ever reached
    /// this branch it really *should* kick everyone off.
    #[test]
    fn empty_roster_deactivates_all_actives() {
        let local = vec![
            mk_user("u-alice", 1, "alice", true),
            mk_user("u-bob", 2, "bob", true),
            mk_user("u-carol", 3, "carol", false),
        ];
        let diff = diff_membership(&local, &[]);
        assert_eq!(
            diff.to_deactivate.iter().collect::<HashSet<_>>(),
            ["u-alice".to_string(), "u-bob".to_string()]
                .iter()
                .collect::<HashSet<_>>()
        );
        assert!(diff.to_activate.is_empty());
        assert!(diff.present_members_to_touch.is_empty());
    }

    /// Reverse case: everyone local is a member and still present → every
    /// user lands in `present_members_to_touch`, nothing flips.
    #[test]
    fn all_still_members_only_touches() {
        let local = vec![
            mk_user("u-alice", 1, "alice", true),
            mk_user("u-bob", 2, "bob", true),
        ];
        let gh = vec![mk_member(1, "alice"), mk_member(2, "bob")];
        let diff = diff_membership(&local, &gh);
        assert!(diff.to_activate.is_empty());
        assert!(diff.to_deactivate.is_empty());
        assert_eq!(diff.present_members_to_touch.len(), 2);
    }

    #[test]
    fn parse_next_link_handles_github_format() {
        let header = "<https://api.github.com/orgs/x/members?page=2&per_page=100>; \
                      rel=\"next\", <https://api.github.com/orgs/x/members?page=5>; rel=\"last\"";
        assert_eq!(
            parse_next_link(header).as_deref(),
            Some("https://api.github.com/orgs/x/members?page=2&per_page=100")
        );
    }

    #[test]
    fn parse_next_link_returns_none_when_no_next() {
        let header = "<https://api.github.com/orgs/x/members?page=5>; rel=\"last\"";
        assert!(parse_next_link(header).is_none());
    }

    #[test]
    fn parse_next_link_tolerates_unquoted_rel() {
        let header = "<https://api.github.com/orgs/x/members?page=2>; rel=next";
        assert_eq!(
            parse_next_link(header).as_deref(),
            Some("https://api.github.com/orgs/x/members?page=2")
        );
    }

    #[test]
    fn urlencode_path_segment_escapes_reserved() {
        assert_eq!(urlencode_path_segment("acme-corp"), "acme-corp");
        assert_eq!(urlencode_path_segment("weird org"), "weird%20org");
    }

    /// End-to-end happy path against a real in-memory DB: seed local users,
    /// apply the diff, assert `set_member_status` + `delete_by_user_fk` did
    /// the right thing. Pure DB test — no HTTP mocking required because the
    /// diff itself is already unit-tested above.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_diff_against_real_db_revokes_sessions() {
        use djinn_db::{CreateUserAuthSession, Database, SessionAuthRepository, UserRepository};

        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let users = UserRepository::new(db.clone());
        let sessions = SessionAuthRepository::new(db.clone());

        // Seed two users: Alice (still a member on GitHub) and Bob (removed).
        let alice = users
            .upsert_from_github(1001, "alice", None, None)
            .await
            .unwrap();
        let bob = users
            .upsert_from_github(1002, "bob", None, None)
            .await
            .unwrap();

        // Both have live browser sessions linked via user_fk.
        sessions
            .create_with_user_fk(
                CreateUserAuthSession {
                    token: "tok-alice",
                    user_id: "alice",
                    github_login: "alice",
                    github_name: None,
                    github_avatar_url: None,
                    github_access_token: "gho_a",
                    expires_at: "2099-01-01T00:00:00.000Z",
                },
                &alice.id,
            )
            .await
            .unwrap();
        sessions
            .create_with_user_fk(
                CreateUserAuthSession {
                    token: "tok-bob-1",
                    user_id: "bob",
                    github_login: "bob",
                    github_name: None,
                    github_avatar_url: None,
                    github_access_token: "gho_b",
                    expires_at: "2099-01-01T00:00:00.000Z",
                },
                &bob.id,
            )
            .await
            .unwrap();
        sessions
            .create_with_user_fk(
                CreateUserAuthSession {
                    token: "tok-bob-2",
                    user_id: "bob",
                    github_login: "bob",
                    github_name: None,
                    github_avatar_url: None,
                    github_access_token: "gho_b2",
                    expires_at: "2099-01-01T00:00:00.000Z",
                },
                &bob.id,
            )
            .await
            .unwrap();

        // GitHub says only Alice is still a member.
        let roster = vec![mk_member(1001, "alice")];
        let local = users.list_all().await.unwrap();
        let diff = diff_membership(&local, &roster);

        // Apply the same way sync_once does.
        for id in &diff.to_activate {
            users.set_member_status(id, true).await.unwrap();
        }
        for id in &diff.present_members_to_touch {
            users.touch_last_seen(id).await.unwrap();
        }
        let mut sessions_revoked = 0u64;
        for id in &diff.to_deactivate {
            users.set_member_status(id, false).await.unwrap();
            sessions_revoked += sessions.delete_by_user_fk(id).await.unwrap();
        }

        // Alice unchanged and still logged in.
        let alice_after = users.get_by_id(&alice.id).await.unwrap().unwrap();
        assert!(alice_after.is_member_of_org);
        assert!(
            sessions.get_by_token("tok-alice").await.unwrap().is_some(),
            "member-in-good-standing session must survive the sync"
        );

        // Bob is now inactive and both sessions are gone.
        let bob_after = users.get_by_id(&bob.id).await.unwrap().unwrap();
        assert!(!bob_after.is_member_of_org);
        assert!(sessions.get_by_token("tok-bob-1").await.unwrap().is_none());
        assert!(sessions.get_by_token("tok-bob-2").await.unwrap().is_none());
        assert_eq!(sessions_revoked, 2);
    }
}
