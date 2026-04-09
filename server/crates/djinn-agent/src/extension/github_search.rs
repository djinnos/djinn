//! GitHub code search — thin wrapper around djinn-provider's GitHubApiClient.

use std::sync::Arc;

use djinn_provider::github_api::GitHubApiClient;
use djinn_provider::repos::CredentialRepository;

/// Execute a code search against the GitHub Code Search API.
pub(crate) async fn search(
    cred_repo: Arc<CredentialRepository>,
    query: &str,
    language: Option<&str>,
    repo: Option<&str>,
    path: Option<&str>,
) -> Result<serde_json::Value, String> {
    let client = GitHubApiClient::new(cred_repo);
    let result = client
        .search_code(query, language, repo, path, None)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_value(result).map_err(|e| e.to_string())
}

/// Fetch the contents of a file from a GitHub repository.
pub(crate) async fn fetch_file(
    cred_repo: Arc<CredentialRepository>,
    repo: &str,
    path: &str,
    git_ref: Option<&str>,
    start_line: Option<u32>,
    end_line: Option<u32>,
) -> Result<serde_json::Value, String> {
    let client = GitHubApiClient::new(cred_repo);
    let result = client
        .fetch_file(repo, path, git_ref, start_line, end_line)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_value(result).map_err(|e| e.to_string())
}
