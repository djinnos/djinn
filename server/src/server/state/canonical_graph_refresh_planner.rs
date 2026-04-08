use std::path::{Path, PathBuf};

#[async_trait::async_trait]
pub(crate) trait CanonicalGraphRefreshProbe: Send + Sync {
    async fn cache_has_entry_for(&self, index_tree_path: &Path) -> bool;
    async fn pinned_commit_for(&self, index_tree_path: &Path) -> Option<String>;
    async fn commits_since(&self, project_root: &Path, pinned_commit: &str) -> Option<u64>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WarmPlanInputs {
    pub(crate) cache_has_entry: bool,
    pub(crate) warm_slot_claimed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WarmPlan {
    SkipHotCache,
    CoalesceInflight,
    KickDetachedWarm,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RefreshPlan {
    SkipColdCache,
    SkipPinnedCommitUnavailable,
    SkipCurrent {
        pinned_commit: String,
    },
    RefreshStale {
        pinned_commit: String,
        commits_behind: u64,
    },
    SkipCommitCheckFailed {
        pinned_commit: String,
    },
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct CanonicalGraphRefreshPlanner;

impl CanonicalGraphRefreshPlanner {
    pub(crate) fn plan_warm(&self, inputs: WarmPlanInputs) -> WarmPlan {
        if inputs.cache_has_entry {
            return WarmPlan::SkipHotCache;
        }

        if !inputs.warm_slot_claimed {
            return WarmPlan::CoalesceInflight;
        }

        WarmPlan::KickDetachedWarm
    }

    pub(crate) async fn plan_refresh<P>(&self, probe: &P, project_root: &Path) -> RefreshPlan
    where
        P: CanonicalGraphRefreshProbe,
    {
        let index_tree_path = canonical_graph_index_tree_path(project_root);

        if !probe.cache_has_entry_for(&index_tree_path).await {
            return RefreshPlan::SkipColdCache;
        }

        let Some(pinned_commit) = probe.pinned_commit_for(&index_tree_path).await else {
            return RefreshPlan::SkipPinnedCommitUnavailable;
        };

        match probe.commits_since(project_root, &pinned_commit).await {
            Some(0) => RefreshPlan::SkipCurrent { pinned_commit },
            Some(commits_behind) => RefreshPlan::RefreshStale {
                pinned_commit,
                commits_behind,
            },
            None => RefreshPlan::SkipCommitCheckFailed { pinned_commit },
        }
    }
}

fn canonical_graph_index_tree_path(project_root: &Path) -> PathBuf {
    project_root.join(".djinn").join("worktrees").join("_index")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[derive(Debug, Clone, Default)]
    struct ProbeState {
        cache_has_entry: bool,
        pinned_commit: Option<String>,
        commits_since: Option<u64>,
        observed_index_tree_paths: Vec<PathBuf>,
        observed_commit_checks: Vec<(PathBuf, String)>,
    }

    #[derive(Clone, Default)]
    struct FakeProbe {
        state: Arc<Mutex<ProbeState>>,
    }

    impl FakeProbe {
        async fn with_inputs(
            cache_has_entry: bool,
            pinned_commit: Option<&str>,
            commits_since: Option<u64>,
        ) -> Self {
            Self {
                state: Arc::new(Mutex::new(ProbeState {
                    cache_has_entry,
                    pinned_commit: pinned_commit.map(str::to_string),
                    commits_since,
                    observed_index_tree_paths: Vec::new(),
                    observed_commit_checks: Vec::new(),
                })),
            }
        }

        async fn snapshot(&self) -> ProbeState {
            self.state.lock().await.clone()
        }
    }

    #[async_trait::async_trait]
    impl CanonicalGraphRefreshProbe for FakeProbe {
        async fn cache_has_entry_for(&self, index_tree_path: &Path) -> bool {
            let mut state = self.state.lock().await;
            state
                .observed_index_tree_paths
                .push(index_tree_path.to_path_buf());
            state.cache_has_entry
        }

        async fn pinned_commit_for(&self, _index_tree_path: &Path) -> Option<String> {
            self.state.lock().await.pinned_commit.clone()
        }

        async fn commits_since(&self, project_root: &Path, pinned_commit: &str) -> Option<u64> {
            let mut state = self.state.lock().await;
            state
                .observed_commit_checks
                .push((project_root.to_path_buf(), pinned_commit.to_string()));
            state.commits_since
        }
    }

    #[test]
    fn warm_planner_distinguishes_hot_coalesced_and_kick_paths() {
        let planner = CanonicalGraphRefreshPlanner;

        assert_eq!(
            planner.plan_warm(WarmPlanInputs {
                cache_has_entry: true,
                warm_slot_claimed: true,
            }),
            WarmPlan::SkipHotCache
        );
        assert_eq!(
            planner.plan_warm(WarmPlanInputs {
                cache_has_entry: false,
                warm_slot_claimed: false,
            }),
            WarmPlan::CoalesceInflight
        );
        assert_eq!(
            planner.plan_warm(WarmPlanInputs {
                cache_has_entry: false,
                warm_slot_claimed: true,
            }),
            WarmPlan::KickDetachedWarm
        );
    }

    #[tokio::test]
    async fn refresh_planner_skips_cold_cache_without_extra_probes() {
        let planner = CanonicalGraphRefreshPlanner;
        let probe = FakeProbe::with_inputs(false, Some("abc123"), Some(3)).await;
        let project_root = Path::new("/tmp/project");

        assert_eq!(
            planner.plan_refresh(&probe, project_root).await,
            RefreshPlan::SkipColdCache
        );

        let snapshot = probe.snapshot().await;
        assert_eq!(
            snapshot.observed_index_tree_paths,
            vec![PathBuf::from("/tmp/project/.djinn/worktrees/_index")]
        );
        assert!(snapshot.observed_commit_checks.is_empty());
    }

    #[tokio::test]
    async fn refresh_planner_skips_current_cache() {
        let planner = CanonicalGraphRefreshPlanner;
        let probe = FakeProbe::with_inputs(true, Some("abc123"), Some(0)).await;
        let project_root = Path::new("/tmp/project");

        assert_eq!(
            planner.plan_refresh(&probe, project_root).await,
            RefreshPlan::SkipCurrent {
                pinned_commit: "abc123".to_string()
            }
        );

        let snapshot = probe.snapshot().await;
        assert_eq!(
            snapshot.observed_commit_checks,
            vec![(PathBuf::from("/tmp/project"), "abc123".to_string())]
        );
    }

    #[tokio::test]
    async fn refresh_planner_requests_refresh_for_stale_cache() {
        let planner = CanonicalGraphRefreshPlanner;
        let probe = FakeProbe::with_inputs(true, Some("def456"), Some(4)).await;
        let project_root = Path::new("/tmp/project");

        assert_eq!(
            planner.plan_refresh(&probe, project_root).await,
            RefreshPlan::RefreshStale {
                pinned_commit: "def456".to_string(),
                commits_behind: 4,
            }
        );
    }
}
