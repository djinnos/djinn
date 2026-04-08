use anyhow::{Result, anyhow};

use crate::github_api::transport::handle_rate_limit;
use crate::github_api::{GitHubApiClient, PrReview, PrReviewFeedback, ReviewComment};

impl GitHubApiClient {
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
            .filter(|review| review.state == "CHANGES_REQUESTED")
            .collect();

        Ok(PrReviewFeedback {
            pull_number,
            pr_url: pr_url.to_owned(),
            change_request_reviews,
            inline_comments,
        })
    }
}
