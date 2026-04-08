use anyhow::{Result, anyhow};

use crate::github_api::transport::handle_rate_limit;
use crate::github_api::{
    CheckRunsResponse, CreatePrParams, GitHubApiClient, MergeMethod, PrReview, PrReviewFeedback,
    PullRequest, ReviewComment,
};

impl GitHubApiClient {
    /// Create a pull request.
    ///
    /// `owner` and `repo` identify the repository. Returns the created PR.
    pub async fn create_pull_request(
        &self,
        owner: &str,
        repo: &str,
        params: CreatePrParams,
    ) -> Result<PullRequest> {
        let url = format!("{}/repos/{}/{}/pulls", self.base_url, owner, repo);
        let body = serde_json::to_value(&params)?;

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
            return Err(anyhow!("create_pull_request failed ({}): {}", status, body));
        }
        Ok(resp.json().await?)
    }

    /// List open pull requests whose head branch matches `head`.
    pub async fn list_pulls_by_head(
        &self,
        owner: &str,
        repo: &str,
        head: &str,
    ) -> Result<Vec<PullRequest>> {
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
            return Err(anyhow!("list_pulls_by_head failed ({}): {}", status, body));
        }
        Ok(resp.json().await?)
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
        Ok(resp.json().await?)
    }

    /// Reopen a closed pull request by setting its state back to `"open"`.
    pub async fn reopen_pull_request(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
    ) -> Result<PullRequest> {
        let url = format!(
            "{}/repos/{}/{}/pulls/{}",
            self.base_url, owner, repo, pull_number
        );
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
            return Err(anyhow!("reopen_pull_request failed ({}): {}", status, body));
        }
        Ok(resp.json().await?)
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
    ) -> Result<serde_json::Value> {
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
            return Err(anyhow!("enable_auto_merge failed ({}): {}", status, body));
        }

        let json: serde_json::Value = resp.json().await?;
        if let Some(errors) = json.get("errors") {
            return Err(anyhow!("enable_auto_merge GraphQL error: {}", errors));
        }
        Ok(json)
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

        let checks_url = format!(
            "{}/repos/{}/{}/commits/{}/check-runs",
            self.base_url, owner, repo, pr.head.sha
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

        let checks: CheckRunsResponse = if checks_resp.status().is_success() {
            checks_resp.json().await?
        } else {
            tracing::warn!(
                "GitHubApiClient: check-runs fetch failed ({}), returning empty",
                checks_resp.status()
            );
            CheckRunsResponse {
                total_count: 0,
                check_runs: vec![],
            }
        };

        Ok((pr, checks))
    }

    /// List review comments on a pull request.
    pub async fn list_pull_request_reviews(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
    ) -> Result<Vec<ReviewComment>> {
        let url = format!(
            "{}/repos/{}/{}/pulls/{}/comments",
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

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "list_pull_request_reviews failed ({}): {}",
                status,
                body
            ));
        }
        Ok(resp.json().await?)
    }

    /// List top-level reviews submitted on a pull request.
    pub async fn list_pr_review_states(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
    ) -> Result<Vec<PrReview>> {
        let url = format!(
            "{}/repos/{}/{}/pulls/{}/reviews",
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

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "list_pr_review_states failed ({}): {}",
                status,
                body
            ));
        }
        Ok(resp.json().await?)
    }

    /// Fetch aggregated PR review feedback for a pull request.
    pub async fn fetch_pr_review_feedback(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
        pr_url: &str,
    ) -> Result<PrReviewFeedback> {
        let reviews = self.list_pr_review_states(owner, repo, pull_number).await?;
        let inline_comments = self
            .list_pull_request_reviews(owner, repo, pull_number)
            .await?;

        let change_request_reviews = reviews
            .into_iter()
            .filter(|r| r.state == "CHANGES_REQUESTED")
            .collect();

        Ok(PrReviewFeedback {
            pull_number,
            pr_url: pr_url.to_owned(),
            change_request_reviews,
            inline_comments,
        })
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

    /// Merge a pull request via the REST API.
    pub async fn merge_pull_request(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
        method: MergeMethod,
        commit_title: &str,
    ) -> Result<serde_json::Value> {
        let url = format!(
            "{}/repos/{}/{}/pulls/{}/merge",
            self.base_url, owner, repo, pull_number
        );
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
            return Err(anyhow!("merge_pull_request failed ({}): {}", status, body));
        }
        Ok(resp.json().await?)
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

        let json: serde_json::Value = resp.json().await?;
        if let Some(errors) = json.get("errors") {
            return Err(anyhow!(
                "mark_pr_ready_for_review GraphQL error: {}",
                errors
            ));
        }

        Ok(json)
    }
}
