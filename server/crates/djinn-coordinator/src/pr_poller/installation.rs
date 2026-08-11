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
        Ok(Some(id)) => Some(installation_client(id)),
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

pub(crate) fn installation_client(installation_id: u64) -> GitHubApiClient {
    #[cfg(test)]
    if let Some(base_url) = INSTALLATION_CLIENT_BASE_URL.lock().unwrap().clone() {
        return GitHubApiClient::for_installation_with_base_url(installation_id, base_url);
    }

    GitHubApiClient::for_installation(installation_id)
}

/// Route installation-authenticated poller requests to a deterministic server.
/// This test-only seam retains the real project lookup, token cache, client
/// authentication, and provider request path.
#[cfg(test)]
static INSTALLATION_CLIENT_BASE_URL: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) fn set_installation_client_base_url_for_test(base_url: Option<String>) {
    *INSTALLATION_CLIENT_BASE_URL.lock().unwrap() = base_url;
}

#[cfg(test)]
mod tests {
    use djinn_core::events::EventBus;
    use djinn_db::{Database, ProjectRepository};
    use djinn_provider::github_app::installations::prime_cache_for_tests;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{resolve_installation_client, set_installation_client_base_url_for_test};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn persisted_installation_reaches_deterministic_provider_request() {
        let db = Database::open_in_memory().unwrap();
        let project =
            djinn_db::test_support::make_project(&db, std::path::Path::new("provider")).await;
        let installation_id = 4_242;
        let installation = djinn_db::test_support::persist_project_github_installation_for_test(
            &db,
            &project.id,
            "acme",
            "widget",
            installation_id,
        )
        .await;
        assert_eq!(installation.installation_id, installation_id);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widget/git/ref/heads/main"))
            .and(header("authorization", "Bearer ghs_installation_fixture"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ref": "refs/heads/main",
                "object": { "sha": "deterministic-head" }
            })))
            .expect(1)
            .mount(&server)
            .await;

        prime_cache_for_tests(installation_id, "ghs_installation_fixture");
        set_installation_client_base_url_for_test(Some(server.uri()));
        let client =
            resolve_installation_client(&ProjectRepository::new(db, EventBus::noop()), &project.id)
                .await
                .expect("persisted installation must create a provider client");
        let sha = client
            .get_ref("acme", "widget", "heads/main")
            .await
            .expect("installation-authenticated provider request");
        set_installation_client_base_url_for_test(None);

        assert_eq!(sha.as_deref(), Some("deterministic-head"));
    }
}
