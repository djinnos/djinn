//! Task-local session user token used to thread the authenticated GitHub
//! user's access token down into MCP tool handlers.
//!
//! The HTTP MCP handler authenticates the incoming request, resolves the
//! caller's `github_access_token`, and scopes the dispatch under
//! [`SESSION_USER_TOKEN`]. Deep in the stack, tools that need to call the
//! GitHub user API (e.g. `github_app_installations`, `github_list_repos`)
//! read the token via [`current_user_token`] without having to plumb it
//! through every function signature.
//!
//! When no session is present (unauthenticated request, or any internal
//! caller that did not set up the scope), [`current_user_token`] returns
//! `None` and the consumer is expected to return a clear "sign in"-style
//! error.

tokio::task_local! {
    pub static SESSION_USER_TOKEN: Option<String>;
}

/// Read the current task-local session user token, if any.
///
/// Returns `None` when:
/// - the request was unauthenticated,
/// - the task was spawned outside any [`SESSION_USER_TOKEN`] scope.
pub fn current_user_token() -> Option<String> {
    SESSION_USER_TOKEN.try_with(|t| t.clone()).ok().flatten()
}
