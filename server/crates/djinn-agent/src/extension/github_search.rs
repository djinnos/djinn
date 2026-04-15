//! GitHub code search — thin wrapper around djinn-provider's GitHubApiClient.
//!
//! These helpers are invoked from MCP extension handlers, which run under
//! the `SESSION_USER_TOKEN` task-local scoped by the HTTP MCP handler.

use djinn_provider::github_api::GitHubApiClient;

/// Execute a code search against the GitHub Code Search API.
pub(crate) async fn search(
    query: &str,
    language: Option<&str>,
    repo: Option<&str>,
    path: Option<&str>,
) -> Result<serde_json::Value, String> {
    let client = GitHubApiClient::for_session_user();
    let result = client
        .search_code(query, language, repo, path, None)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_value(result).map_err(|e| e.to_string())
}

/// Fetch the contents of a file from a GitHub repository.
pub(crate) async fn fetch_file(
    repo: &str,
    path: &str,
    git_ref: Option<&str>,
    start_line: Option<u32>,
    end_line: Option<u32>,
) -> Result<serde_json::Value, String> {
    let client = GitHubApiClient::for_session_user();
    let result = client
        .fetch_file(repo, path, git_ref, start_line, end_line)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_value(result).map_err(|e| e.to_string())
}
