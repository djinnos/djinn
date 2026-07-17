// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
use crate::github_api::GitHubApiError;
use anyhow::{Result, anyhow};

use crate::github_api::transport::handle_rate_limit;
use crate::github_api::types::{CompareResponse, PrFile, RequiredStatusChecksResponse};
use crate::github_api::{
    AutoMergeRequest, CheckRun, CheckRunsResponse, CreatePrParams, DequeueEvent, GitHubApiClient,
    MergeMethod, MergeQueueEntry, MergeQueueEntryState, PrMergeQueueState, PullRequest,
};

fn github_pr_write_error(
    _method: &'static str,
    path: &str,
    status: Option<reqwest::StatusCode>,
    body_or_detail: &str,
    operation: &'static str,
) -> GitHubApiError {
    GitHubApiError::http(
        operation,
        path.to_string(),
        status.unwrap_or(reqwest::StatusCode::INTERNAL_SERVER_ERROR),
        body_or_detail.to_string(),
    )
}

impl GitHubApiClient {
    /// Create a pull request.
    ///
    /// `owner` and `repo` identify the repository. Returns the created PR.
    pub async fn create_pull_request(
        &self,
        owner: &str,
        repo: &str,
        params: CreatePrParams,
    ) -> std::result::Result<PullRequest, GitHubApiError> {
        let path = format!("/repos/{owner}/{repo}/pulls");
        let url = format!("{}{}", self.base_url, path);
        let body = serde_json::to_value(&params).map_err(|e| {
            GitHubApiError::transport("create_pull_request", path.clone(), e.to_string())
        })?;

        let resp = self
            .send_with_retry(|token| {
                let url = url.clone();
                let body = body.clone();
                let http = self.http.clone();
                async move {
                    let resp = http
                        .post(&url)
                        .bearer_auth(&token)
                        .header("Accept", "application/vnd.github+json")
                        .header("X-GitHub-Api-Version", "2022-11-28")
                        .json(&body)
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            // Idempotent recovery: GitHub returns 422 "A pull request already
            // exists for <owner>:<head>" when a PR for this head branch is
            // already open (a retried dispatch, or a prior run that opened the
            // PR but didn't persist its URL). Adopt the existing PR instead of
            // failing the whole flow — otherwise the caller loops
            // reopen→create→422 forever. Mirrors create_ref's
            // "422 already exists → success" idempotency.
            //
            // The adoption list is retried: only an OPEN PR triggers this 422,
            // yet a single-shot list can still miss it — the list endpoint
            // lags a just-created PR (read-after-write), and during degraded
            // GitHub-API incidents the list 5xxs while the POST's 422 got
            // through (2026-07-16, task mbfw). An escaped 422 fails the whole
            // PR-open run for a PR that exists.
            if status.as_u16() == 422 && body.contains("already exists") {
                const ADOPT_LIST_ATTEMPTS: u32 = 3;
                let head_filter = format!("{owner}:{}", params.head);
                for attempt in 1..=ADOPT_LIST_ATTEMPTS {
                    match self.list_pulls_by_head(owner, repo, &head_filter).await {
                        Ok(prs) => {
                            if let Some(pr) = prs.into_iter().next() {
                                tracing::info!(
                                    owner,
                                    repo,
                                    head = %params.head,
                                    pr = pr.number,
                                    attempt,
                                    "create_pull_request: PR already exists for head — adopting existing"
                                );
                                return Ok(pr);
                            }
                            tracing::warn!(
                                owner,
                                repo,
                                head = %params.head,
                                attempt,
                                "create_pull_request: 422 says a PR exists but the list shows none yet"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                owner,
                                repo,
                                head = %params.head,
                                attempt,
                                error = %e,
                                "create_pull_request: adoption list failed after 422"
                            );
                        }
                    }
                    if attempt < ADOPT_LIST_ATTEMPTS {
                        tokio::time::sleep(std::time::Duration::from_secs(1 << (attempt - 1)))
                            .await;
                    }
                }
            }
            return Err(github_pr_write_error(
                "POST",
                &path,
                Some(status),
                &body,
                "create_pull_request",
            ));
        }
        resp.json().await.map_err(|e| {
            GitHubApiError::transport("create_pull_request", path.clone(), e.to_string())
        })
    }

    /// List open pull requests whose head branch matches `head`.
    pub async fn list_pulls_by_head(
        &self,
        owner: &str,
        repo: &str,
        head: &str,
    ) -> std::result::Result<Vec<PullRequest>, GitHubApiError> {
        let url = format!(
            "{}/repos/{}/{}/pulls?state=open&head={}",
            self.base_url, owner, repo, head
        );

        let resp = self
            .send_with_retry(|token| {
                let url = url.clone();
                let http = self.http.clone();
                async move {
                    let resp = http
                        .get(&url)
                        .bearer_auth(&token)
                        .header("Accept", "application/vnd.github+json")
                        .header("X-GitHub-Api-Version", "2022-11-28")
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(GitHubApiError::http(
                "list_pulls_by_head",
                format!("/repos/{owner}/{repo}/pulls"),
                status,
                body,
            ));
        }
        resp.json().await.map_err(|e| {
            GitHubApiError::transport(
                "list_pulls_by_head",
                format!("/repos/{owner}/{repo}/pulls"),
                e.to_string(),
            )
        })
    }

    /// List open pull requests whose base branch matches `base`.
    pub async fn list_pulls_by_base(
        &self,
        owner: &str,
        repo: &str,
        base: &str,
    ) -> Result<Vec<PullRequest>> {
        let url = format!(
            "{}/repos/{}/{}/pulls?state=open&base={}",
            self.base_url, owner, repo, base
        );

        let resp = self
            .send_with_retry(|token| {
                let url = url.clone();
                let http = self.http.clone();
                async move {
                    let resp = http
                        .get(&url)
                        .bearer_auth(&token)
                        .header("Accept", "application/vnd.github+json")
                        .header("X-GitHub-Api-Version", "2022-11-28")
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("list_pulls_by_base failed ({}): {}", status, body));
        }
        Ok(resp.json().await.map_err(|e| {
            GitHubApiError::transport(
                "list_pulls_by_base",
                format!("/repos/{owner}/{repo}/pulls"),
                e.to_string(),
            )
        })?)
    }

    /// List pull requests whose head branch matches `head`, filtering by state.
    pub async fn list_pulls_by_head_with_state(
        &self,
        owner: &str,
        repo: &str,
        head: &str,
        state: &str,
    ) -> Result<Vec<PullRequest>> {
        let url = format!(
            "{}/repos/{}/{}/pulls?state={}&head={}",
            self.base_url, owner, repo, state, head
        );

        let resp = self
            .send_with_retry(|token| {
                let url = url.clone();
                let http = self.http.clone();
                async move {
                    let resp = http
                        .get(&url)
                        .bearer_auth(&token)
                        .header("Accept", "application/vnd.github+json")
                        .header("X-GitHub-Api-Version", "2022-11-28")
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "list_pulls_by_head_with_state failed ({}): {}",
                status,
                body
            ));
        }
        Ok(resp.json().await.map_err(|e| {
            GitHubApiError::transport(
                "list_pulls_by_head",
                format!("/repos/{owner}/{repo}/pulls"),
                e.to_string(),
            )
        })?)
    }

    /// List all open pull requests in a repository, paginating through all pages.
    ///
    /// Used by the periodic stale-PR sweep to enumerate bot-authored PRs on
    /// `task/*` and `chore/*` branches. The caller is responsible for filtering
    /// by head-branch prefix and author.
    pub async fn list_open_pulls(
        &self,
        owner: &str,
        repo: &str,
    ) -> std::result::Result<Vec<PullRequest>, GitHubApiError> {
        let mut all_prs = Vec::new();
        let mut page: u32 = 1;
        const PER_PAGE: u32 = 100;

        loop {
            let url = format!(
                "{}/repos/{}/{}/pulls?state=open&per_page={}&page={}",
                self.base_url, owner, repo, PER_PAGE, page
            );

            let resp = self
                .send_with_retry(|token| {
                    let url = url.clone();
                    let http = self.http.clone();
                    async move {
                        let resp = http
                            .get(&url)
                            .bearer_auth(&token)
                            .header("Accept", "application/vnd.github+json")
                            .header("X-GitHub-Api-Version", "2022-11-28")
                            .send()
                            .await?;
                        handle_rate_limit(resp).await
                    }
                })
                .await?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(GitHubApiError::http(
                    "list_open_pulls",
                    format!("/repos/{owner}/{repo}/pulls"),
                    status,
                    body,
                ));
            }

            let page_prs: Vec<PullRequest> = resp.json().await.map_err(|e| {
                GitHubApiError::transport(
                    "list_open_pulls",
                    format!("/repos/{owner}/{repo}/pulls"),
                    e.to_string(),
                )
            })?;

            let is_last_page = page_prs.len() < PER_PAGE as usize;
            all_prs.extend(page_prs);
            if is_last_page {
                break;
            }
            page += 1;
        }

        Ok(all_prs)
    }

    /// Close an open pull request by setting its state to `"closed"`.
    ///
    /// Inverse of [`Self::reopen_pull_request`]. Used by the periodic
    /// reconciliation sweep and the inline cleanup hook to close stale PRs
    /// whose backing task has been closed or superseded.
    pub async fn close_pull_request(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
    ) -> Result<PullRequest> {
        let path = format!("/repos/{owner}/{repo}/pulls/{pull_number}");
        let url = format!("{}{}", self.base_url, path);
        let body = serde_json::json!({ "state": "closed" });

        let resp = self
            .send_with_retry(|token| {
                let url = url.clone();
                let body = body.clone();
                let http = self.http.clone();
                async move {
                    let resp = http
                        .patch(&url)
                        .bearer_auth(&token)
                        .header("Accept", "application/vnd.github+json")
                        .header("X-GitHub-Api-Version", "2022-11-28")
                        .json(&body)
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(github_pr_write_error(
                "PATCH",
                &path,
                Some(status),
                &body,
                "close_pull_request",
            )
            .into());
        }
        Ok(resp.json().await.map_err(|e| {
            GitHubApiError::transport("close_pull_request", path.clone(), e.to_string())
        })?)
    }

    /// Create a comment on a pull request via the Issues API.
    ///
    /// GitHub treats PR comments as issue comments; the endpoint is
    /// `POST /repos/{owner}/{repo}/issues/{pull_number}/comments`.
    /// Used to leave an audit-trail comment when a PR is closed by the
    /// reconciliation sweep or inline cleanup hook.
    pub async fn create_pr_comment(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
        body: &str,
    ) -> Result<serde_json::Value> {
        let path = format!("/repos/{owner}/{repo}/issues/{pull_number}/comments");
        let url = format!("{}{}", self.base_url, path);
        let request_body = serde_json::json!({ "body": body });

        let resp = self
            .send_with_retry(|token| {
                let url = url.clone();
                let request_body = request_body.clone();
                let http = self.http.clone();
                async move {
                    let resp = http
                        .post(&url)
                        .bearer_auth(&token)
                        .header("Accept", "application/vnd.github+json")
                        .header("X-GitHub-Api-Version", "2022-11-28")
                        .json(&request_body)
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let response_body = resp.text().await.unwrap_or_default();
            return Err(github_pr_write_error(
                "POST",
                &path,
                Some(status),
                &response_body,
                "create_pr_comment",
            )
            .into());
        }
        Ok(resp.json().await.map_err(|e| {
            GitHubApiError::transport("create_pr_comment", path.clone(), e.to_string())
        })?)
    }

    /// Reopen a closed pull request by setting its state back to `"open"`.
    pub async fn reopen_pull_request(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
    ) -> Result<PullRequest> {
        let path = format!("/repos/{owner}/{repo}/pulls/{pull_number}");
        let url = format!("{}{}", self.base_url, path);
        let body = serde_json::json!({ "state": "open" });

        let resp = self
            .send_with_retry(|token| {
                let url = url.clone();
                let body = body.clone();
                let http = self.http.clone();
                async move {
                    let resp = http
                        .patch(&url)
                        .bearer_auth(&token)
                        .header("Accept", "application/vnd.github+json")
                        .header("X-GitHub-Api-Version", "2022-11-28")
                        .json(&body)
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(github_pr_write_error(
                "PATCH",
                &path,
                Some(status),
                &body,
                "reopen_pull_request",
            )
            .into());
        }
        Ok(resp.json().await.map_err(|e| {
            GitHubApiError::transport("reopen_pull_request", path.clone(), e.to_string())
        })?)
    }

    /// Enable auto-merge on an existing pull request.
    pub async fn enable_auto_merge(
        &self,
        _owner: &str,
        _repo: &str,
        _pull_number: u64,
        method: MergeMethod,
        node_id: &str,
        commit_headline: &str,
    ) -> std::result::Result<serde_json::Value, GitHubApiError> {
        let merge_method = match method {
            MergeMethod::Squash => "SQUASH",
            MergeMethod::Rebase => "REBASE",
            MergeMethod::Merge => "MERGE",
        };

        let query = r#"
            mutation EnableAutoMerge($pullRequestId: ID!, $mergeMethod: PullRequestMergeMethod!, $commitHeadline: String!) {
                enablePullRequestAutoMerge(input: {
                    pullRequestId: $pullRequestId,
                    mergeMethod: $mergeMethod,
                    commitHeadline: $commitHeadline
                }) {
                    pullRequest { number title autoMergeRequest { enabledAt mergeMethod } }
                }
            }
        "#;

        let body = serde_json::json!({
            "query": query,
            "variables": {
                "pullRequestId": node_id,
                "mergeMethod": merge_method,
                "commitHeadline": commit_headline,
            }
        });

        let base_url = self.base_url.clone();
        let resp = self
            .send_with_retry(|token| {
                let body = body.clone();
                let http = self.http.clone();
                let base_url = base_url.clone();
                async move {
                    let graphql_url = format!("{}/graphql", base_url);
                    let resp = http
                        .post(&graphql_url)
                        .bearer_auth(&token)
                        .json(&body)
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(github_pr_write_error(
                "POST",
                "/graphql",
                Some(status),
                &body,
                "enable_auto_merge",
            ));
        }

        let json: serde_json::Value = resp.json().await.map_err(|e| {
            GitHubApiError::transport("graphql", "/graphql".to_string(), e.to_string())
        })?;
        if let Some(errors) = json.get("errors") {
            return Err(GitHubApiError::graphql(
                "enable_auto_merge",
                "/graphql".to_string(),
                errors.to_string(),
            ));
        }
        Ok(json)
    }

    /// Cancel an active auto-merge request on a PR.
    ///
    /// Mirrors [`Self::enable_auto_merge`] in reverse. Used on task
    /// cancellation so a force-closed task doesn't leave a "merge when
    /// ready" timer running on GitHub's side.
    ///
    /// Best-effort: GitHub returns an error if the PR has no active
    /// auto-merge request (already merged, never enabled, etc.) which
    /// callers should treat as success.
    pub async fn disable_auto_merge(
        &self,
        node_id: &str,
    ) -> std::result::Result<(), GitHubApiError> {
        let query = r#"
            mutation DisableAutoMerge($pullRequestId: ID!) {
                disablePullRequestAutoMerge(input: { pullRequestId: $pullRequestId }) {
                    pullRequest { number title }
                }
            }
        "#;

        let body = serde_json::json!({
            "query": query,
            "variables": { "pullRequestId": node_id },
        });

        let base_url = self.base_url.clone();
        let resp = self
            .send_with_retry(|token| {
                let body = body.clone();
                let http = self.http.clone();
                let base_url = base_url.clone();
                async move {
                    let graphql_url = format!("{}/graphql", base_url);
                    let resp = http
                        .post(&graphql_url)
                        .bearer_auth(&token)
                        .json(&body)
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(GitHubApiError::http(
                "disable_auto_merge",
                "/graphql".to_string(),
                status,
                body,
            ));
        }

        let json: serde_json::Value = resp.json().await.map_err(|e| {
            GitHubApiError::transport("graphql", "/graphql".to_string(), e.to_string())
        })?;
        if let Some(errors) = json.get("errors") {
            return Err(GitHubApiError::graphql(
                "disable_auto_merge",
                "/graphql".to_string(),
                errors.to_string(),
            ));
        }
        Ok(())
    }

    /// Enqueue a pull request into the repository merge queue.
    ///
    /// Unlike [`Self::enable_auto_merge`] this mutation does not require
    /// the repo's "Allow auto-merge" setting to be on — it works on any
    /// repo whose protected branch enforces a merge queue. Used as the
    /// fallback when `PUT /pulls/{n}/merge` returns 405 because the
    /// branch protection routes everything through the queue.
    ///
    /// `expected_head_oid` pins the enqueue to the SHA we observed:
    /// if a new commit landed in the meantime, GitHub rejects the
    /// mutation instead of queuing a stale ref.
    pub async fn enqueue_pull_request(
        &self,
        pull_request_node_id: &str,
        expected_head_oid: &str,
    ) -> std::result::Result<(), GitHubApiError> {
        let query = r#"
            mutation EnqueuePullRequest($pullRequestId: ID!, $expectedHeadOid: GitObjectID) {
                enqueuePullRequest(input: {
                    pullRequestId: $pullRequestId,
                    expectedHeadOid: $expectedHeadOid
                }) {
                    mergeQueueEntry { id state }
                }
            }
        "#;

        let body = serde_json::json!({
            "query": query,
            "variables": {
                "pullRequestId": pull_request_node_id,
                "expectedHeadOid": expected_head_oid,
            }
        });

        let base_url = self.base_url.clone();
        let resp = self
            .send_with_retry(|token| {
                let body = body.clone();
                let http = self.http.clone();
                let base_url = base_url.clone();
                async move {
                    let graphql_url = format!("{}/graphql", base_url);
                    let resp = http
                        .post(&graphql_url)
                        .bearer_auth(&token)
                        .json(&body)
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(GitHubApiError::http(
                "enqueue_pull_request",
                "/graphql".to_string(),
                status,
                body,
            ));
        }

        let json: serde_json::Value = resp.json().await.map_err(|e| {
            GitHubApiError::transport("graphql", "/graphql".to_string(), e.to_string())
        })?;
        if let Some(errors) = json.get("errors") {
            return Err(GitHubApiError::graphql(
                "enqueue_pull_request",
                "/graphql".to_string(),
                errors.to_string(),
            ));
        }
        Ok(())
    }

    /// List the GraphQL node ids of a PR's **unresolved** review threads.
    ///
    /// Used by the merge automation when an approved PR is blocked solely by
    /// the repo's "A conversation must be resolved before this pull request
    /// can be merged" rule: an explicit approval makes the reviewer's inline
    /// comments non-blocking, so we resolve the leftover threads ourselves and
    /// let the merge proceed. Returns the ids where `isResolved == false`.
    pub async fn list_unresolved_review_thread_ids(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
    ) -> Result<Vec<String>> {
        // `query (` (with a space — insignificant whitespace in GraphQL) keeps
        // the naive raw-SQL boundary grep from matching the operation keyword.
        let query = r#"query ($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){reviewThreads(first:100){nodes{id isResolved}}}}}"#;

        let body = serde_json::json!({
            "query": query,
            "variables": { "owner": owner, "name": repo, "number": pull_number },
        });

        let base_url = self.base_url.clone();
        let resp = self
            .send_with_retry(|token| {
                let body = body.clone();
                let http = self.http.clone();
                let base_url = base_url.clone();
                async move {
                    let graphql_url = format!("{}/graphql", base_url);
                    let resp = http
                        .post(&graphql_url)
                        .bearer_auth(&token)
                        .json(&body)
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "list_unresolved_review_thread_ids failed ({}): {}",
                status,
                body
            ));
        }

        let json: serde_json::Value = resp.json().await.map_err(|e| {
            GitHubApiError::transport("graphql", "/graphql".to_string(), e.to_string())
        })?;
        if let Some(errors) = json.get("errors") {
            return Err(anyhow!(
                "list_unresolved_review_thread_ids GraphQL error: {}",
                errors
            ));
        }

        let nodes = json["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let ids = nodes
            .iter()
            .filter(|n| n["isResolved"].as_bool() == Some(false))
            .filter_map(|n| n["id"].as_str().map(|s| s.to_string()))
            .collect();
        Ok(ids)
    }

    /// Resolve a single review thread by its GraphQL node id.
    ///
    /// Companion to [`Self::list_unresolved_review_thread_ids`]. Idempotent on
    /// GitHub's side: re-resolving an already-resolved thread is a no-op and the
    /// thread simply won't reappear in the unresolved list on the next poll.
    pub async fn resolve_review_thread(
        &self,
        thread_id: &str,
    ) -> std::result::Result<(), GitHubApiError> {
        let query = r#"mutation($threadId:ID!){resolveReviewThread(input:{threadId:$threadId}){thread{isResolved}}}"#;

        let body = serde_json::json!({
            "query": query,
            "variables": { "threadId": thread_id },
        });

        let base_url = self.base_url.clone();
        let resp = self
            .send_with_retry(|token| {
                let body = body.clone();
                let http = self.http.clone();
                let base_url = base_url.clone();
                async move {
                    let graphql_url = format!("{}/graphql", base_url);
                    let resp = http
                        .post(&graphql_url)
                        .bearer_auth(&token)
                        .json(&body)
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(GitHubApiError::http(
                "resolve_review_thread",
                "/graphql".to_string(),
                status,
                body,
            ));
        }

        let json: serde_json::Value = resp.json().await.map_err(|e| {
            GitHubApiError::transport("graphql", "/graphql".to_string(), e.to_string())
        })?;
        if let Some(errors) = json.get("errors") {
            return Err(GitHubApiError::graphql(
                "resolve_review_thread",
                "/graphql".to_string(),
                errors.to_string(),
            ));
        }
        Ok(())
    }

    /// Remove a pull request from the repository merge queue.
    ///
    /// `merge_queue_entry_id` is [`MergeQueueEntry::id`] from a prior
    /// [`Self::get_pr_merge_queue_state`] call. Used on task cancellation
    /// when the PR is already queued — disabling auto-merge alone won't
    /// remove the existing queue entry.
    pub async fn dequeue_pull_request(
        &self,
        pull_request_node_id: &str,
    ) -> std::result::Result<(), GitHubApiError> {
        let query = r#"
            mutation DequeuePullRequest($id: ID!) {
                dequeuePullRequest(input: { id: $id }) {
                    mergeQueueEntry { id state }
                }
            }
        "#;

        let body = serde_json::json!({
            "query": query,
            "variables": { "id": pull_request_node_id },
        });

        let base_url = self.base_url.clone();
        let resp = self
            .send_with_retry(|token| {
                let body = body.clone();
                let http = self.http.clone();
                let base_url = base_url.clone();
                async move {
                    let graphql_url = format!("{}/graphql", base_url);
                    let resp = http
                        .post(&graphql_url)
                        .bearer_auth(&token)
                        .json(&body)
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(GitHubApiError::http(
                "dequeue_pull_request",
                "/graphql".to_string(),
                status,
                body,
            ));
        }

        let json: serde_json::Value = resp.json().await.map_err(|e| {
            GitHubApiError::transport("graphql", "/graphql".to_string(), e.to_string())
        })?;
        if let Some(errors) = json.get("errors") {
            return Err(GitHubApiError::graphql(
                "dequeue_pull_request",
                "/graphql".to_string(),
                errors.to_string(),
            ));
        }
        Ok(())
    }

    /// Fetch the merge-queue / auto-merge state for a PR via GraphQL.
    ///
    /// Returns the fields the REST `pulls/{n}` endpoint doesn't surface:
    /// `mergeStateStatus`, the live `mergeQueueEntry`, the
    /// `autoMergeRequest`, and the most recent `RemovedFromMergeQueueEvent` (used to
    /// surface failure diagnostics when the queue kicks a PR out).
    pub async fn get_pr_merge_queue_state(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
    ) -> Result<PrMergeQueueState> {
        let query = r#"
            query PrMergeQueueState($owner: String!, $repo: String!, $number: Int!) {
                repository(owner: $owner, name: $repo) {
                    pullRequest(number: $number) {
                        mergeStateStatus
                        autoMergeRequest { enabledAt mergeMethod }
                        commits(last: 1) {
                            nodes { commit { committedDate pushedDate } }
                        }
                        mergeQueueEntry {
                            id
                            state
                            position
                            estimatedTimeToMerge
                            solo
                        }
                        timelineItems(last: 20, itemTypes: [REMOVED_FROM_MERGE_QUEUE_EVENT]) {
                            nodes {
                                __typename
                                ... on RemovedFromMergeQueueEvent {
                                    reason
                                    createdAt
                                    actor { login }
                                    beforeCommit { oid }
                                }
                            }
                        }
                    }
                }
            }
        "#;

        let body = serde_json::json!({
            "query": query,
            "variables": { "owner": owner, "repo": repo, "number": pull_number },
        });

        let base_url = self.base_url.clone();
        let resp = self
            .send_with_retry(|token| {
                let body = body.clone();
                let http = self.http.clone();
                let base_url = base_url.clone();
                async move {
                    let graphql_url = format!("{}/graphql", base_url);
                    let resp = http
                        .post(&graphql_url)
                        .bearer_auth(&token)
                        .json(&body)
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "get_pr_merge_queue_state failed ({}): {}",
                status,
                body
            ));
        }

        let json: serde_json::Value = resp.json().await.map_err(|e| {
            GitHubApiError::transport("graphql", "/graphql".to_string(), e.to_string())
        })?;
        if let Some(errors) = json.get("errors") {
            return Err(anyhow!(
                "get_pr_merge_queue_state GraphQL error: {}",
                errors
            ));
        }

        let pr = &json["data"]["repository"]["pullRequest"];
        if pr.is_null() {
            return Err(anyhow!(
                "get_pr_merge_queue_state: pullRequest not found ({}/{}#{})",
                owner,
                repo,
                pull_number
            ));
        }

        let merge_state_status = pr["mergeStateStatus"].as_str().map(|s| s.to_string());

        let auto_merge_request = if pr["autoMergeRequest"].is_object() {
            Some(AutoMergeRequest {
                enabled_at: pr["autoMergeRequest"]["enabledAt"]
                    .as_str()
                    .map(|s| s.to_string()),
                merge_method: pr["autoMergeRequest"]["mergeMethod"]
                    .as_str()
                    .map(|s| s.to_string()),
            })
        } else {
            None
        };

        let merge_queue_entry = if pr["mergeQueueEntry"].is_object() {
            let state_str = pr["mergeQueueEntry"]["state"].as_str().unwrap_or("");
            let state = serde_json::from_value::<MergeQueueEntryState>(serde_json::Value::String(
                state_str.to_string(),
            ))
            .ok();
            let id = pr["mergeQueueEntry"]["id"]
                .as_str()
                .unwrap_or("")
                .to_string();
            state.map(|state| MergeQueueEntry {
                id,
                state,
                position: pr["mergeQueueEntry"]["position"].as_u64().map(|n| n as u32),
                estimated_time_to_merge: pr["mergeQueueEntry"]["estimatedTimeToMerge"]
                    .as_u64()
                    .map(|n| n as u32),
                solo: pr["mergeQueueEntry"]["solo"].as_bool(),
            })
        } else {
            None
        };

        let last_dequeue = pr["timelineItems"]["nodes"]
            .as_array()
            .and_then(|nodes| {
                nodes
                    .iter()
                    .rev()
                    .find(|n| n["__typename"] == "RemovedFromMergeQueueEvent")
            })
            .map(|node| DequeueEvent {
                reason: node["reason"].as_str().map(|s| s.to_string()),
                merge_group_ref: None,
                created_at: node["createdAt"].as_str().map(|s| s.to_string()),
                before_commit_sha: node["beforeCommit"]["oid"].as_str().map(|s| s.to_string()),
            });

        let head_commit = &pr["commits"]["nodes"][0]["commit"];
        let head_committed_at = head_commit["pushedDate"]
            .as_str()
            .or_else(|| head_commit["committedDate"].as_str())
            .map(|s| s.to_string());

        Ok(PrMergeQueueState {
            merge_state_status,
            merge_queue_entry,
            auto_merge_request,
            last_dequeue,
            head_committed_at,
        })
    }

    /// Get a pull request along with its CI check runs.
    pub async fn get_pull_request(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
    ) -> Result<(PullRequest, CheckRunsResponse)> {
        let pr_url = format!(
            "{}/repos/{}/{}/pulls/{}",
            self.base_url, owner, repo, pull_number
        );

        let pr_resp = self
            .send_with_retry(|token| {
                let url = pr_url.clone();
                let http = self.http.clone();
                async move {
                    let resp = http
                        .get(&url)
                        .bearer_auth(&token)
                        .header("Accept", "application/vnd.github+json")
                        .header("X-GitHub-Api-Version", "2022-11-28")
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        if !pr_resp.status().is_success() {
            let status = pr_resp.status();
            let body = pr_resp.text().await.unwrap_or_default();
            return Err(anyhow!("get_pull_request failed ({}): {}", status, body));
        }
        let pr: PullRequest = pr_resp.json().await?;

        // GitHub paginates `/check-runs` at a default page size of 30, so a PR
        // with >30 check runs would silently drop the rest and the merge gate
        // would misjudge CI. Request the max page size (100) and page through
        // every result until a short (or empty) page signals the end. Bound the
        // loop at MAX_PAGES so a pathological PR can't make us page forever.
        const PER_PAGE: u32 = 100;
        const MAX_PAGES: u32 = 10; // 10 * 100 = 1000 check runs.

        let mut all_runs: Vec<CheckRun> = Vec::new();
        let mut total_count: u32 = 0;
        let mut hit_cap = false;

        for page in 1..=MAX_PAGES {
            let checks_url = format!(
                "{}/repos/{}/{}/commits/{}/check-runs?per_page={}&page={}",
                self.base_url, owner, repo, pr.head.sha, PER_PAGE, page
            );

            let checks_resp = self
                .send_with_retry(|token| {
                    let url = checks_url.clone();
                    let http = self.http.clone();
                    async move {
                        let resp = http
                            .get(&url)
                            .bearer_auth(&token)
                            .header("Accept", "application/vnd.github+json")
                            .header("X-GitHub-Api-Version", "2022-11-28")
                            .send()
                            .await?;
                        handle_rate_limit(resp).await
                    }
                })
                .await?;

            if !checks_resp.status().is_success() {
                tracing::warn!(
                    "GitHubApiClient: check-runs fetch failed ({}) on page {}, returning what was collected",
                    checks_resp.status(),
                    page
                );
                break;
            }

            let page_body: CheckRunsResponse = checks_resp.json().await?;
            // `total_count` reflects the full set on every page; keep the
            // last-seen value (page 1 is sufficient, but later pages agree).
            total_count = page_body.total_count;
            let page_len = page_body.check_runs.len();
            all_runs.extend(page_body.check_runs);

            // A short page (fewer than PER_PAGE) means this was the last page.
            if (page_len as u32) < PER_PAGE {
                break;
            }
            if page == MAX_PAGES {
                hit_cap = true;
            }
        }

        if hit_cap {
            tracing::warn!(
                owner,
                repo,
                head_sha = %pr.head.sha,
                max_pages = MAX_PAGES,
                collected = all_runs.len(),
                total_count,
                "GitHubApiClient: check-runs pagination hit MAX_PAGES cap; some runs may be omitted",
            );
        }

        let checks = CheckRunsResponse {
            total_count,
            check_runs: all_runs,
        };

        Ok((pr, checks))
    }

    /// Check whether the installation token can access a repository.
    pub async fn check_repo_access(&self, owner: &str, repo: &str) -> Result<()> {
        let url = format!("{}/repos/{}/{}", self.base_url, owner, repo);

        let resp = self
            .send_with_retry(|token| {
                let url = url.clone();
                let http = self.http.clone();
                async move {
                    let resp = http
                        .get(&url)
                        .bearer_auth(&token)
                        .header("Accept", "application/vnd.github+json")
                        .header("X-GitHub-Api-Version", "2022-11-28")
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        if resp.status().is_success() {
            return Ok(());
        }

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(anyhow!("check_repo_access failed ({}): {}", status, body))
    }

    /// Re-request review on a pull request from previous reviewers.
    pub async fn re_request_review(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
        reviewer_logins: &[String],
    ) -> Result<()> {
        if reviewer_logins.is_empty() {
            return Ok(());
        }

        let url = format!(
            "{}/repos/{}/{}/pulls/{}/requested_reviewers",
            self.base_url, owner, repo, pull_number
        );
        let body = serde_json::json!({ "reviewers": reviewer_logins });

        let resp = self
            .send_with_retry(|token| {
                let url = url.clone();
                let body = body.clone();
                let http = self.http.clone();
                async move {
                    let resp = http
                        .post(&url)
                        .bearer_auth(&token)
                        .header("Accept", "application/vnd.github+json")
                        .header("X-GitHub-Api-Version", "2022-11-28")
                        .json(&body)
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("re_request_review failed ({}): {}", status, body));
        }
        Ok(())
    }

    /// Post an APPROVE review on a pull request, pinned to a specific commit.
    ///
    /// Used by the auto-approve path: the configured client *must* be built
    /// with [`Self::for_user_token`] (the user's own GitHub token), because
    /// the GitHub App identity that opened the PR cannot approve its own PR.
    ///
    /// `commit_id` is the head SHA the approval applies to. Pinning it means
    /// a subsequent push to the branch automatically invalidates this review
    /// — exactly the behavior we want for the race between "approve" and a
    /// new commit landing before the next poller tick merges.
    ///
    /// GitHub returns 422 if the approver authored a commit on the PR (e.g.
    /// the user pushed manually). Callers should handle 422 distinctly from
    /// other failures so they don't retry indefinitely on the same SHA.
    pub async fn approve_pull_request(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
        commit_id: &str,
    ) -> Result<()> {
        let url = format!(
            "{}/repos/{}/{}/pulls/{}/reviews",
            self.base_url, owner, repo, pull_number
        );
        let body = serde_json::json!({
            "commit_id": commit_id,
            "event": "APPROVE",
        });

        let resp = self
            .send_with_retry(|token| {
                let url = url.clone();
                let body = body.clone();
                let http = self.http.clone();
                async move {
                    let resp = http
                        .post(&url)
                        .bearer_auth(&token)
                        .header("Accept", "application/vnd.github+json")
                        .header("X-GitHub-Api-Version", "2022-11-28")
                        .json(&body)
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "approve_pull_request failed ({}): {}",
                status,
                body
            ));
        }
        Ok(())
    }

    /// Merge the base branch into a PR's head branch (the "Update branch"
    /// button on the GitHub UI).
    ///
    /// Used when GitHub reports `mergeable_state == "behind"` on a repo that
    /// requires up-to-date branches before merging: there are no conflicts,
    /// but the branch-protection rule blocks merging until the head includes
    /// the latest base. This merges base → head, which produces a new head
    /// SHA and triggers a fresh CI run. The PR poller catches the new SHA on
    /// the next tick and re-evaluates from scratch.
    ///
    /// `expected_head_sha` should be the head SHA observed when we decided
    /// to update; GitHub will reject the call with 422 if the head has
    /// already moved (worker pushed a new commit between fetch and update).
    /// On success returns 202 Accepted with `{"message": ..., "url": ...}`.
    pub async fn update_pull_request_branch(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
        expected_head_sha: &str,
    ) -> std::result::Result<serde_json::Value, GitHubApiError> {
        let path = format!("/repos/{owner}/{repo}/pulls/{pull_number}/update-branch");
        let url = format!("{}{}", self.base_url, path);
        let body = serde_json::json!({
            "expected_head_sha": expected_head_sha,
        });

        let resp = self
            .send_with_retry(|token| {
                let url = url.clone();
                let body = body.clone();
                let http = self.http.clone();
                async move {
                    let resp = http
                        .put(&url)
                        .bearer_auth(&token)
                        .header("Accept", "application/vnd.github+json")
                        .header("X-GitHub-Api-Version", "2022-11-28")
                        .json(&body)
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(github_pr_write_error(
                "PUT",
                &path,
                Some(status),
                &body,
                "update_pull_request_branch",
            ));
        }
        resp.json().await.map_err(|e| {
            GitHubApiError::transport("update_pull_request_branch", path.clone(), e.to_string())
        })
    }

    /// Merge a pull request via the REST API.
    pub async fn merge_pull_request(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
        method: MergeMethod,
        commit_title: &str,
    ) -> std::result::Result<serde_json::Value, GitHubApiError> {
        let path = format!("/repos/{owner}/{repo}/pulls/{pull_number}/merge");
        let url = format!("{}{}", self.base_url, path);
        let merge_method_str = match method {
            MergeMethod::Squash => "squash",
            MergeMethod::Rebase => "rebase",
            MergeMethod::Merge => "merge",
        };
        let body = serde_json::json!({
            "merge_method": merge_method_str,
            "commit_title": commit_title,
        });

        let resp = self
            .send_with_retry(|token| {
                let url = url.clone();
                let body = body.clone();
                let http = self.http.clone();
                async move {
                    let resp = http
                        .put(&url)
                        .bearer_auth(&token)
                        .header("Accept", "application/vnd.github+json")
                        .header("X-GitHub-Api-Version", "2022-11-28")
                        .json(&body)
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(github_pr_write_error(
                "PUT",
                &path,
                Some(status),
                &body,
                "merge_pull_request",
            ));
        }
        resp.json().await.map_err(|e| {
            GitHubApiError::transport("merge_pull_request", path.clone(), e.to_string())
        })
    }

    /// Mark a draft PR as ready for review (undraft it).
    pub async fn mark_pr_ready_for_review(&self, node_id: &str) -> Result<serde_json::Value> {
        let query = r#"
            mutation MarkPullRequestReadyForReview($pullRequestId: ID!) {
                markPullRequestReadyForReview(input: { pullRequestId: $pullRequestId }) {
                    pullRequest { number isDraft }
                }
            }
        "#;

        let body = serde_json::json!({
            "query": query,
            "variables": { "pullRequestId": node_id }
        });

        let base_url = self.base_url.clone();
        let resp = self
            .send_with_retry(|token| {
                let body = body.clone();
                let http = self.http.clone();
                let base_url = base_url.clone();
                async move {
                    let graphql_url = format!("{}/graphql", base_url);
                    let resp = http
                        .post(&graphql_url)
                        .bearer_auth(&token)
                        .json(&body)
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "mark_pr_ready_for_review failed ({}): {}",
                status,
                body
            ));
        }

        let json: serde_json::Value = resp.json().await.map_err(|e| {
            GitHubApiError::transport("graphql", "/graphql".to_string(), e.to_string())
        })?;
        if let Some(errors) = json.get("errors") {
            return Err(anyhow!(
                "mark_pr_ready_for_review GraphQL error: {}",
                errors
            ));
        }

        Ok(json)
    }

    /// Delete a git ref on the remote repository.
    ///
    /// `ref_name` is the ref *without* the leading `refs/` prefix — pass
    /// `"heads/<branch>"` for branches, `"tags/<tag>"` for tags. GitHub's
    /// own PR-cleanup path (and our `cleanup_task_branches_post_close`)
    /// uses this to wipe the task branch after merge / force-close so
    /// the mirror and the remote both stop dragging the dead ref around.
    ///
    /// Idempotent: GitHub returns 422 when the ref doesn't exist; we treat
    /// that as success.
    pub async fn delete_ref(&self, owner: &str, repo: &str, ref_name: &str) -> Result<()> {
        let url = format!(
            "{}/repos/{}/{}/git/refs/{}",
            self.base_url, owner, repo, ref_name
        );

        let resp = self
            .send_with_retry(|token| {
                let url = url.clone();
                let http = self.http.clone();
                async move {
                    let resp = http
                        .delete(&url)
                        .bearer_auth(&token)
                        .header("Accept", "application/vnd.github+json")
                        .header("X-GitHub-Api-Version", "2022-11-28")
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        let status = resp.status();
        if status.is_success() || status.as_u16() == 422 {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        Err(anyhow!("delete_ref failed ({}): {}", status, body))
    }

    /// List the **required** status-check contexts configured on a branch's
    /// protection rules.
    ///
    /// Reads `GET /repos/{owner}/{repo}/branches/{branch}/protection/required_status_checks`,
    /// which returns the contexts that branch protection (and, for queue-enabled
    /// repos, the merge queue) treats as merge-gating. These are the only check
    /// runs whose failure should trigger a rework — advisory checks (preview
    /// environments, deploy previews, etc.) are absent from this list.
    ///
    /// Returns:
    /// - `Ok(Some(contexts))` when branch protection with required checks exists.
    /// - `Ok(None)` when the branch has no protection / no required checks
    ///   (HTTP 404), so the caller knows there is no source of truth and can
    ///   fall back to a name-pattern heuristic.
    /// - `Err(..)` on any other failure (e.g. 403 when the installation lacks
    ///   the `administration` permission), so the caller can likewise fall back
    ///   rather than treating every check as non-blocking.
    pub async fn list_required_status_checks(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
    ) -> Result<Option<Vec<String>>> {
        let url = format!(
            "{}/repos/{}/{}/branches/{}/protection/required_status_checks",
            self.base_url, owner, repo, branch
        );

        let resp = self
            .send_with_retry(|token| {
                let url = url.clone();
                let http = self.http.clone();
                async move {
                    let resp = http
                        .get(&url)
                        .bearer_auth(&token)
                        .header("Accept", "application/vnd.github+json")
                        .header("X-GitHub-Api-Version", "2022-11-28")
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        let status = resp.status();
        // 404 = no branch protection / no required-status-checks rule on this
        // branch. Distinct from an access error: there genuinely is no required
        // set, so report `None` and let the caller fall back to heuristics.
        if status.as_u16() == 404 {
            return Ok(None);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "list_required_status_checks failed ({}): {}",
                status,
                body
            ));
        }
        let parsed: RequiredStatusChecksResponse = resp.json().await?;
        Ok(Some(parsed.contexts))
    }

    /// Return the status-check **context names GitHub treats as required for a
    /// specific PR** — exactly what the "Required" badge on the PR page shows.
    ///
    /// Reads the GraphQL `isRequired(pullRequestNumber:)` field on the head
    /// commit's status-check rollup. Unlike the classic
    /// `branches/{branch}/protection/required_status_checks` endpoint (see
    /// [`Self::list_required_status_checks`]), this reflects *every* mechanism
    /// GitHub uses to gate the merge — classic branch protection, repository
    /// **rulesets**, and the merge queue — in one authoritative per-PR answer,
    /// and needs no `administration` permission. Repos configured via rulesets
    /// (increasingly the default) return 404 from the classic endpoint, which
    /// is why that source alone makes the rework loop treat every unknown
    /// advisory check (e.g. `Sentinel`) as blocking.
    ///
    /// Returns:
    /// - `Ok(Some(names))` when the rollup is readable; `names` is exactly the
    ///   contexts with `isRequired == true` (an empty vec authoritatively means
    ///   *nothing* is required, so no check failure is merge-blocking).
    /// - `Ok(None)` when there is no head commit / no rollup to read, so the
    ///   caller can fall back to another source of truth.
    /// - `Err(..)` on transport/GraphQL errors, so the caller can fall back
    ///   rather than treating every check as non-blocking.
    pub async fn required_check_contexts_for_pr(
        &self,
        owner: &str,
        repo: &str,
        pr_number: u64,
    ) -> Result<Option<Vec<String>>> {
        let query = r#"
            query ($owner: String!, $repo: String!, $pr: Int!) {
                repository(owner: $owner, name: $repo) {
                    pullRequest(number: $pr) {
                        commits(last: 1) { nodes { commit {
                            statusCheckRollup {
                                contexts(first: 100) { nodes {
                                    __typename
                                    ... on CheckRun { name isRequired(pullRequestNumber: $pr) }
                                    ... on StatusContext { context isRequired(pullRequestNumber: $pr) }
                                } }
                            }
                        } } }
                    }
                }
            }
        "#;

        let body = serde_json::json!({
            "query": query,
            "variables": { "owner": owner, "repo": repo, "pr": pr_number }
        });

        let base_url = self.base_url.clone();
        let resp = self
            .send_with_retry(|token| {
                let body = body.clone();
                let http = self.http.clone();
                let base_url = base_url.clone();
                async move {
                    let graphql_url = format!("{}/graphql", base_url);
                    let resp = http
                        .post(&graphql_url)
                        .bearer_auth(&token)
                        .json(&body)
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "required_check_contexts_for_pr failed ({}): {}",
                status,
                body
            ));
        }

        let json: serde_json::Value = resp.json().await.map_err(|e| {
            GitHubApiError::transport("graphql", "/graphql".to_string(), e.to_string())
        })?;
        if let Some(errors) = json.get("errors") {
            return Err(anyhow!(
                "required_check_contexts_for_pr GraphQL error: {}",
                errors
            ));
        }

        Ok(parse_required_contexts_from_rollup(&json))
    }

    /// Fetch the list of files changed in a PR via `GET /repos/{owner}/{repo}/pulls/{pull_number}/files`.
    /// Returns the list of file paths (relative to repo root) that the PR modifies,
    /// adds, or deletes. Used by the scope-inversion check to determine what the
    /// PR actually touches.
    pub async fn get_pr_files(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
    ) -> Result<Vec<PrFile>> {
        let url = format!(
            "{}/repos/{}/{}/pulls/{}/files?per_page=100",
            self.base_url, owner, repo, pull_number
        );

        let resp = self
            .send_with_retry(|token| {
                let url = url.clone();
                let http = self.http.clone();
                async move {
                    let resp = http
                        .get(&url)
                        .bearer_auth(&token)
                        .header("Accept", "application/vnd.github+json")
                        .header("X-GitHub-Api-Version", "2022-11-28")
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("get_pr_files failed ({}): {}", status, body));
        }
        let files: Vec<PrFile> = resp.json().await?;
        Ok(files)
    }

    /// Return how many commits `head` is ahead of `base` on GitHub via the
    /// compare API (`GET /repos/{owner}/{repo}/compare/{base}...{head}`).
    ///
    /// Used by the CI-failure rework loop to detect the diff-empty case: when a
    /// reworking worker re-opens a PR whose head is identical to (or contains no
    /// new commits beyond) the base, `ahead_by == 0` and there is nothing a
    /// fresh worker iteration can change — re-dispatching just loops.
    pub async fn compare_commits_ahead_by(
        &self,
        owner: &str,
        repo: &str,
        base: &str,
        head: &str,
    ) -> Result<u64> {
        let url = format!(
            "{}/repos/{}/{}/compare/{}...{}",
            self.base_url, owner, repo, base, head
        );

        let resp = self
            .send_with_retry(|token| {
                let url = url.clone();
                let http = self.http.clone();
                async move {
                    let resp = http
                        .get(&url)
                        .bearer_auth(&token)
                        .header("Accept", "application/vnd.github+json")
                        .header("X-GitHub-Api-Version", "2022-11-28")
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "compare_commits_ahead_by failed ({}): {}",
                status,
                body
            ));
        }
        let parsed: CompareResponse = resp.json().await?;
        Ok(parsed.ahead_by)
    }
}

/// Extract the required check-context names from a `statusCheckRollup` GraphQL
/// response (see [`GitHubApiClient::required_check_contexts_for_pr`]).
///
/// Walks `data.repository.pullRequest.commits.nodes[0].commit.statusCheckRollup`
/// and returns the `name`/`context` of every rollup context whose
/// `isRequired` is `true`. Returns `None` when there is no head commit or no
/// rollup (so the caller can fall back to another source); `Some(vec![])` when
/// a rollup exists but nothing in it is required.
fn parse_required_contexts_from_rollup(json: &serde_json::Value) -> Option<Vec<String>> {
    let rollup =
        json.pointer("/data/repository/pullRequest/commits/nodes/0/commit/statusCheckRollup")?;
    // `statusCheckRollup` is JSON `null` when the commit has no checks at all.
    let nodes = rollup.get("contexts")?.get("nodes")?.as_array()?;
    let required = nodes
        .iter()
        .filter(|n| {
            n.get("isRequired")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        })
        .filter_map(|n| {
            n.get("name")
                .or_else(|| n.get("context"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .collect();
    Some(required)
}

#[cfg(test)]
mod required_contexts_tests {
    use super::parse_required_contexts_from_rollup;

    fn rollup(nodes: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "data": { "repository": { "pullRequest": { "commits": { "nodes": [
                { "commit": { "statusCheckRollup": { "contexts": { "nodes": nodes } } } }
            ] } } } }
        })
    }

    #[test]
    fn keeps_only_required_contexts() {
        let json = rollup(serde_json::json!([
            { "__typename": "CheckRun", "name": "Sentinel", "isRequired": false },
            { "__typename": "CheckRun", "name": "Run Unit Tests", "isRequired": true },
            { "__typename": "CheckRun", "name": "Aikido / aikido", "isRequired": true },
            { "__typename": "StatusContext", "context": "Vercel", "isRequired": false }
        ]));
        let got = parse_required_contexts_from_rollup(&json).expect("rollup present");
        assert_eq!(got, vec!["Run Unit Tests", "Aikido / aikido"]);
    }

    #[test]
    fn empty_when_nothing_required() {
        let json = rollup(serde_json::json!([
            { "__typename": "CheckRun", "name": "Sentinel", "isRequired": false }
        ]));
        // Authoritative empty: a rollup exists but nothing is required → no
        // failing check is merge-blocking (no heuristic fallback).
        assert_eq!(
            parse_required_contexts_from_rollup(&json),
            Some(Vec::<String>::new())
        );
    }

    #[test]
    fn none_when_rollup_absent() {
        // `statusCheckRollup: null` (commit has no checks) → fall back.
        let json = rollup_null();
        assert_eq!(parse_required_contexts_from_rollup(&json), None);
    }

    #[test]
    fn none_when_no_commits() {
        let json = serde_json::json!({
            "data": { "repository": { "pullRequest": { "commits": { "nodes": [] } } } }
        });
        assert_eq!(parse_required_contexts_from_rollup(&json), None);
    }

    fn rollup_null() -> serde_json::Value {
        serde_json::json!({
            "data": { "repository": { "pullRequest": { "commits": { "nodes": [
                { "commit": { "statusCheckRollup": null } }
            ] } } } }
        })
    }
}
