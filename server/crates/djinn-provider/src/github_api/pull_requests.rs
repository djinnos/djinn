use anyhow::{Result, anyhow};

use crate::github_api::transport::handle_rate_limit;
use crate::github_api::{
    CheckRunsResponse, CreatePrParams, GitHubApiClient, MergeMethod, PullRequest,
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
    ) -> Result<serde_json::Value> {
        let url = format!(
            "{}/repos/{}/{}/pulls/{}/update-branch",
            self.base_url, owner, repo, pull_number
        );
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
            return Err(anyhow!(
                "update_pull_request_branch failed ({}): {}",
                status,
                body
            ));
        }
        Ok(resp.json().await.unwrap_or(serde_json::Value::Null))
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
}
