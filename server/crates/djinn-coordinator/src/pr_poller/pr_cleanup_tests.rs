use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use djinn_provider::github_api::{
    AutoMergeRequest, DequeueEvent, GitHubUser, MergeQueueEntry, MergeQueueEntryState,
    PrMergeQueueState, PrRef, PrState, PullRequest,
};
use time::OffsetDateTime;

use super::pr_cleanup::{
    BranchCleanupOutcome, PrCleanupGitHub, PrCleanupPolicy, PrCleanupPolicyConfig, PrCleanupTarget,
};

#[derive(Clone, Default)]
struct MockCleanupGitHub {
    state: Arc<Mutex<MockState>>,
}

#[derive(Default)]
struct MockState {
    merge_queue_state: PrMergeQueueStateFixture,
    base_prs: Vec<PullRequest>,
    deleted_refs: Vec<String>,
}

#[derive(Clone, Default)]
struct PrMergeQueueStateFixture {
    queued: bool,
}

#[async_trait]
impl PrCleanupGitHub for MockCleanupGitHub {
    async fn get_pr_merge_queue_state(
        &self,
        _owner: &str,
        _repo: &str,
        _pull_number: u64,
    ) -> Result<PrMergeQueueState> {
        let state = self.state.lock().unwrap().merge_queue_state.clone();
        Ok(PrMergeQueueState {
            merge_state_status: None,
            merge_queue_entry: state.queued.then(|| MergeQueueEntry {
                id: "mqe_1".to_string(),
                state: MergeQueueEntryState::Queued,
                position: Some(1),
                estimated_time_to_merge: Some(30),
                solo: None,
            }),
            auto_merge_request: None::<AutoMergeRequest>,
            last_dequeue: None::<DequeueEvent>,
            head_committed_at: None,
        })
    }

    async fn list_pulls_by_base(
        &self,
        _owner: &str,
        _repo: &str,
        _base: &str,
    ) -> Result<Vec<PullRequest>> {
        Ok(self.state.lock().unwrap().base_prs.clone())
    }

    async fn delete_ref(&self, _owner: &str, _repo: &str, ref_name: &str) -> Result<()> {
        self.state
            .lock()
            .unwrap()
            .deleted_refs
            .push(ref_name.to_string());
        Ok(())
    }
}

fn config() -> PrCleanupPolicyConfig {
    let mut bot_logins = HashSet::new();
    bot_logins.insert("djinn-bot[bot]".to_string());
    let mut protected_branches = HashSet::new();
    protected_branches.insert("main".to_string());

    PrCleanupPolicyConfig {
        enabled: true,
        dry_run: false,
        grace_period: Duration::from_secs(600),
        owner: "djinnos".to_string(),
        repo: "djinn".to_string(),
        bot_logins,
        protected_branches,
        allowed_branch_prefixes: vec!["task/".to_string(), "chore/".to_string()],
    }
}

fn policy(
    github: MockCleanupGitHub,
    config: PrCleanupPolicyConfig,
) -> PrCleanupPolicy<MockCleanupGitHub> {
    PrCleanupPolicy::with_now(
        github,
        config,
        OffsetDateTime::parse(
            "2026-06-21T12:00:00Z",
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap(),
    )
}

fn target(closed_at: Option<&str>, updated_at: &str) -> PrCleanupTarget {
    PrCleanupTarget {
        short_id: "b111".to_string(),
        updated_at: updated_at.to_string(),
        closed_at: closed_at.map(str::to_string),
    }
}

fn pr(number: u64, author: &str, head: &str, base: &str) -> PullRequest {
    PullRequest {
        number,
        title: "PR".to_string(),
        state: PrState::Open,
        user: Some(GitHubUser {
            login: author.to_string(),
            id: 1,
        }),
        merged: Some(false),
        merge_commit_sha: None,
        html_url: format!("https://github.com/djinnos/djinn/pull/{number}"),
        head: PrRef {
            ref_name: head.to_string(),
            sha: "head-sha".to_string(),
        },
        base: PrRef {
            ref_name: base.to_string(),
            sha: "base-sha".to_string(),
        },
        auto_merge: None,
        node_id: format!("PR_{number}"),
        mergeable: Some(true),
        mergeable_state: Some("clean".to_string()),
        draft: Some(false),
    }
}

#[tokio::test]
async fn pr_cleanup_skips_when_policy_disabled() {
    let mut cfg = config();
    cfg.enabled = false;
    let cleanup = policy(MockCleanupGitHub::default(), cfg);

    assert!(
        !cleanup
            .should_cleanup_pr_for_target(
                &target(Some("2026-06-21T11:00:00Z"), "2026-06-21T11:00:00Z"),
                &pr(1, "djinn-bot[bot]", "task/b111", "main"),
            )
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn pr_cleanup_skips_within_grace_period_using_closed_at() {
    let cleanup = policy(MockCleanupGitHub::default(), config());

    assert!(
        !cleanup
            .should_cleanup_pr_for_target(
                &target(Some("2026-06-21T11:55:00Z"), "2026-06-21T10:00:00Z"),
                &pr(1, "djinn-bot[bot]", "task/b111", "main"),
            )
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn pr_cleanup_uses_updated_at_when_closed_at_missing() {
    let cleanup = policy(MockCleanupGitHub::default(), config());

    assert!(
        !cleanup
            .should_cleanup_pr_for_target(
                &target(None, "2026-06-21T11:55:00Z"),
                &pr(1, "djinn-bot[bot]", "task/b111", "main"),
            )
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn pr_cleanup_skips_human_authored_prs() {
    let cleanup = policy(MockCleanupGitHub::default(), config());

    assert!(
        !cleanup
            .should_cleanup_pr_for_target(
                &target(Some("2026-06-21T10:00:00Z"), "2026-06-21T10:00:00Z"),
                &pr(1, "alice", "task/b111", "main"),
            )
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn pr_cleanup_skips_merge_queue_prs() {
    let github = MockCleanupGitHub::default();
    github.state.lock().unwrap().merge_queue_state.queued = true;
    let cleanup = policy(github, config());

    assert!(
        !cleanup
            .should_cleanup_pr_for_target(
                &target(Some("2026-06-21T10:00:00Z"), "2026-06-21T10:00:00Z"),
                &pr(1, "djinn-bot[bot]", "task/b111", "main"),
            )
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn pr_cleanup_allows_bot_pr_after_guardrails_pass() {
    let cleanup = policy(MockCleanupGitHub::default(), config());

    assert!(
        cleanup
            .should_cleanup_pr_for_target(
                &target(Some("2026-06-21T10:00:00Z"), "2026-06-21T10:00:00Z"),
                &pr(1, "djinn-bot[bot]", "task/b111", "main"),
            )
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn branch_cleanup_skips_protected_branches() {
    let cleanup = policy(MockCleanupGitHub::default(), config());

    assert!(
        !cleanup
            .delete_branch_if_allowed_for_target(
                &target(Some("2026-06-21T10:00:00Z"), "2026-06-21T10:00:00Z"),
                "main",
            )
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn branch_cleanup_skips_branch_used_as_base_of_open_pr() {
    let github = MockCleanupGitHub::default();
    github
        .state
        .lock()
        .unwrap()
        .base_prs
        .push(pr(2, "djinn-bot[bot]", "task/child", "task/b111"));
    let cleanup = policy(github, config());

    assert!(
        !cleanup
            .delete_branch_if_allowed_for_target(
                &target(Some("2026-06-21T10:00:00Z"), "2026-06-21T10:00:00Z"),
                "task/b111",
            )
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn branch_cleanup_dry_run_does_not_delete_ref() {
    let github = MockCleanupGitHub::default();
    let mut cfg = config();
    cfg.dry_run = true;
    let cleanup = policy(github.clone(), cfg);

    let outcome = cleanup
        .delete_branch_if_allowed_for_target(
            &target(Some("2026-06-21T10:00:00Z"), "2026-06-21T10:00:00Z"),
            "refs/heads/task/b111",
        )
        .await
        .unwrap();

    assert_eq!(outcome, BranchCleanupOutcome::DryRunWouldDelete);
    assert!(github.state.lock().unwrap().deleted_refs.is_empty());
}

#[tokio::test]
async fn branch_cleanup_deletes_ref_when_guardrails_pass() {
    let github = MockCleanupGitHub::default();
    let cleanup = policy(github.clone(), config());

    let outcome = cleanup
        .delete_branch_if_allowed_for_target(
            &target(Some("2026-06-21T10:00:00Z"), "2026-06-21T10:00:00Z"),
            "task/b111",
        )
        .await
        .unwrap();

    assert_eq!(outcome, BranchCleanupOutcome::Deleted);
    assert_eq!(
        github.state.lock().unwrap().deleted_refs,
        vec!["heads/task/b111".to_string()]
    );
}
