//! Task-local session context used to thread the authenticated user's
//! identity (both their GitHub access token and the stable `users.id`
//! surrogate) down into MCP tool handlers and repository inserts.
//!
//! The HTTP MCP handler authenticates the incoming request, resolves the
//! caller's `github_access_token` and `users.id`, and scopes the dispatch
//! under both [`SESSION_USER_TOKEN`] and [`SESSION_USER_ID`]. Deep in the
//! stack, tools that need to call the GitHub user API (e.g.
//! `github_app_installations`, `github_list_repos`) read the token via
//! [`current_user_token`]; repositories that need to stamp
//! `created_by_user_id` on new rows read the id via [`current_user_id`].
//! Neither require plumbing through every function signature.
//!
//! When no session is present (unauthenticated request, or any internal
//! caller that did not set up the scope), the accessors return `None` and
//! the consumer either returns a clear "sign in"-style error (for token
//! consumers) or simply leaves the `created_by_user_id` column as NULL
//! (for repository consumers — agent-spawned rows without a user context
//! are allowed to be unattributed).

tokio::task_local! {
    pub static SESSION_USER_TOKEN: Option<String>;
    /// Stable `users.id` surrogate (VARCHAR(36)) for the authenticated
    /// session, threaded through MCP dispatch so repository inserts can
    /// stamp `created_by_user_id` without plumbing the id through every
    /// call site.
    pub static SESSION_USER_ID: Option<String>;
}

/// Read the current task-local session user token, if any.
///
/// Returns `None` when:
/// - the request was unauthenticated,
/// - the task was spawned outside any [`SESSION_USER_TOKEN`] scope.
pub fn current_user_token() -> Option<String> {
    SESSION_USER_TOKEN.try_with(|t| t.clone()).ok().flatten()
}

/// Read the current task-local session `users.id`, if any.
///
/// Returns `None` when:
/// - the request was unauthenticated (no session cookie),
/// - the task was spawned outside any [`SESSION_USER_ID`] scope (e.g. the
///   agent coordinator's internal loops, background workers).
///
/// Repository inserts consult this to populate the `created_by_user_id`
/// attribution column on `tasks`, `epics`, and `sessions`.
pub fn current_user_id() -> Option<String> {
    SESSION_USER_ID.try_with(|t| t.clone()).ok().flatten()
}
