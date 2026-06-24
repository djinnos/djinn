use super::*;

/// Resolve a project's `installation_id` and build a GitHub API client
/// authenticating as that GitHub App installation. Returns `None` when the
/// project row has no installation (legacy pre-Migration-2 rows) or the
/// lookup fails.
pub(crate) async fn resolve_installation_client(
    project_repo: &djinn_db::ProjectRepository,
    project_id: &str,
) -> Option<GitHubApiClient> {
    match project_repo.get_installation_id(project_id).await {
        Ok(Some(id)) => Some(GitHubApiClient::for_installation(id)),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(
                project_id,
                error = %e,
                "PR poller: failed to read installation_id for project"
            );
            None
        }
    }
}
