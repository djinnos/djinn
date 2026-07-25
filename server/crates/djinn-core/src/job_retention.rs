//! Pure, fail-closed retention policy for task-run Kubernetes Jobs.
//!
//! This module intentionally accepts only persisted/runtime evidence and an
//! injected clock.  Every cleanup path must use this policy rather than making
//! its own deletion decision.
//!
//! # Relationship to exact-pod watchdog termination
//!
//! This policy is a *retention* authority: it decides when a Job that no longer
//! backs live work may be garbage-collected. It is deliberately not the only
//! code that can delete a task-run Job — `terminate_taskrun_pod_exact` in
//! `djinn-k8s` also removes the Job, but as the tail of an identity-fenced
//! termination of one specific recorded Pod UID, driven by an unresolved
//! invocation-journal record. The two are orthogonal: one is triggered by an
//! outcome plus elapsed time, the other by a recorded Pod that must be proven
//! dead.
//!
//! They do share one ordering invariant. The watchdog needs the Job to still
//! exist (or to have been deleted by its own protocol) in order to confirm the
//! Pod deletion; a Job reaped out from under it leaves the journal record
//! unresolved. Its recovery pass fires only after a 300-second grace, so
//! neither retention window below may drop under 300 seconds. Unresolved
//! records never carry clean terminal agreement, so they classify as
//! [`RetentionOutcome::Failure`] and get the full hour rather than the
//! success window.

use std::time::{Duration, SystemTime};

/// Must not fall below the exact-pod watchdog's 300-second recovery grace; see
/// the module docs.
pub const SUCCESS_RETENTION: Duration = Duration::from_secs(300);
pub const FAILURE_RETENTION: Duration = Duration::from_secs(3600);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetentionOutcome {
    /// There is evidence that the Job still backs live work.
    Live,
    Success,
    /// Failed, contradictory, or insufficient terminal evidence.
    Failure,
}

#[derive(Clone, Copy, Debug)]
pub struct SessionEvidence<'a> {
    pub status: &'a str,
    pub ended_at: Option<SystemTime>,
}

#[derive(Clone, Copy, Debug)]
pub struct JobRetentionEvidence<'a> {
    pub created_at: Option<SystemTime>,
    pub completed_at: Option<SystemTime>,
    pub terminal_condition: Option<&'a str>,
    pub task_run_status: Option<&'a str>,
    pub task_run_ended_at: Option<SystemTime>,
    pub sessions: &'a [SessionEvidence<'a>],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetentionDecision {
    pub outcome: RetentionOutcome,
    pub terminal_at: Option<SystemTime>,
    pub delete_after: Option<SystemTime>,
}

impl RetentionDecision {
    pub fn should_delete(self, now: SystemTime) -> bool {
        self.delete_after.is_some_and(|deadline| now >= deadline)
    }
}

/// Classify and age-gate a task-run Job. Unknown evidence is deliberately a
/// failure, not success; absent time evidence is preserved unless creation time
/// proves the static one-hour safety-net has elapsed.
pub fn classify_taskrun_job(
    _now: SystemTime,
    evidence: JobRetentionEvidence<'_>,
) -> RetentionDecision {
    let live_task_run = matches!(evidence.task_run_status, Some("starting" | "running"))
        && evidence.task_run_ended_at.is_none();
    let live_session = evidence.sessions.iter().any(|session| {
        matches!(session.status, "running" | "paused") && session.ended_at.is_none()
    });
    let terminal_task_run = matches!(
        evidence.task_run_status,
        Some("completed" | "failed" | "interrupted")
    );
    let failed_session = evidence
        .sessions
        .iter()
        .any(|session| matches!(session.status, "failed" | "interrupted"));
    let terminal_kubernetes =
        evidence.terminal_condition.is_some() || evidence.completed_at.is_some();

    // Terminal evidence wins over stale live DB evidence. Contradictory
    // evidence fails closed rather than preserving the Job forever as Live.
    let contradictory_live = (terminal_kubernetes && (live_task_run || live_session))
        || (terminal_task_run && live_session)
        || ((live_task_run || live_session) && failed_session);
    if (live_task_run || live_session) && !contradictory_live {
        return RetentionDecision {
            outcome: RetentionOutcome::Live,
            terminal_at: None,
            delete_after: None,
        };
    }

    let terminal_at = evidence
        .completed_at
        .or(evidence.task_run_ended_at)
        .or_else(|| {
            evidence
                .sessions
                .iter()
                .filter_map(|session| {
                    matches!(session.status, "completed" | "failed" | "interrupted")
                        .then_some(session.ended_at)
                        .flatten()
                })
                .max()
        });

    let failed_condition = matches!(evidence.terminal_condition, Some("Failed"));
    let unknown_condition =
        matches!(evidence.terminal_condition, Some(condition) if condition != "Complete");
    let success = evidence.task_run_status == Some("completed")
        && !contradictory_live
        && !failed_condition
        && !unknown_condition
        && !failed_session;
    let outcome = if success {
        RetentionOutcome::Success
    } else {
        RetentionOutcome::Failure
    };
    let retention = if success {
        SUCCESS_RETENTION
    } else {
        FAILURE_RETENTION
    };
    // A missing terminal timestamp may use creation only as the one-hour lower
    // bound; never apply the shorter successful retention to creation time.
    let delete_after = terminal_at
        .and_then(|time| time.checked_add(retention))
        .or_else(|| {
            evidence
                .created_at
                .and_then(|time| time.checked_add(FAILURE_RETENTION))
        });
    RetentionDecision {
        outcome,
        terminal_at,
        delete_after,
    }
}

#[cfg(test)]
mod retention_policy {
    use super::*;

    fn at(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
    }
    fn evidence(
        completed_at: Option<SystemTime>,
        status: Option<&str>,
    ) -> JobRetentionEvidence<'_> {
        JobRetentionEvidence {
            created_at: Some(at(0)),
            completed_at,
            terminal_condition: Some("Complete"),
            task_run_status: status,
            task_run_ended_at: completed_at,
            sessions: &[],
        }
    }

    #[test]
    fn all_boundaries() {
        let success = classify_taskrun_job(at(299), evidence(Some(at(0)), Some("completed")));
        assert_eq!(success.outcome, RetentionOutcome::Success);
        assert!(!success.should_delete(at(299)));
        assert!(success.should_delete(at(300)));

        let unknown = classify_taskrun_job(at(3599), evidence(Some(at(0)), None));
        assert_eq!(unknown.outcome, RetentionOutcome::Failure);
        assert!(!unknown.should_delete(at(3599)));
        assert!(unknown.should_delete(at(3600)));

        let failure = JobRetentionEvidence {
            terminal_condition: Some("Failed"),
            ..evidence(Some(at(0)), Some("failed"))
        };
        let failure = classify_taskrun_job(at(3599), failure);
        assert_eq!(failure.outcome, RetentionOutcome::Failure);
        assert!(!failure.should_delete(at(3599)));
        assert!(failure.should_delete(at(3600)));

        let live_evidence = JobRetentionEvidence {
            terminal_condition: None,
            ..evidence(None, Some("running"))
        };
        let live = classify_taskrun_job(at(9999), live_evidence);
        assert_eq!(live.outcome, RetentionOutcome::Live);
        assert!(!live.should_delete(at(9999)));

        let contradictory = JobRetentionEvidence {
            terminal_condition: Some("Failed"),
            ..evidence(Some(at(0)), Some("completed"))
        };
        assert_eq!(
            classify_taskrun_job(at(3600), contradictory).outcome,
            RetentionOutcome::Failure
        );

        let stale_live_after_failure = JobRetentionEvidence {
            completed_at: Some(at(0)),
            terminal_condition: Some("Failed"),
            task_run_status: Some("running"),
            task_run_ended_at: None,
            ..evidence(Some(at(0)), Some("running"))
        };
        let stale_live_after_failure = classify_taskrun_job(at(3599), stale_live_after_failure);
        assert_eq!(stale_live_after_failure.outcome, RetentionOutcome::Failure);
        assert!(!stale_live_after_failure.should_delete(at(3599)));
        assert!(stale_live_after_failure.should_delete(at(3600)));

        let stale_live_session = [SessionEvidence {
            status: "paused",
            ended_at: None,
        }];
        let terminal_task_with_live_session = JobRetentionEvidence {
            sessions: &stale_live_session,
            ..evidence(Some(at(0)), Some("completed"))
        };
        assert_eq!(
            classify_taskrun_job(at(3600), terminal_task_with_live_session).outcome,
            RetentionOutcome::Failure
        );

        let failed_without_time = [SessionEvidence {
            status: "interrupted",
            ended_at: None,
        }];
        let success_with_failed_session = JobRetentionEvidence {
            sessions: &failed_without_time,
            ..evidence(Some(at(0)), Some("completed"))
        };
        assert_eq!(
            classify_taskrun_job(at(3600), success_with_failed_session).outcome,
            RetentionOutcome::Failure
        );

        let no_time = JobRetentionEvidence {
            created_at: None,
            completed_at: None,
            terminal_condition: None,
            task_run_status: None,
            task_run_ended_at: None,
            sessions: &[],
        };
        assert!(!classify_taskrun_job(at(10_000), no_time).should_delete(at(10_000)));
    }

    /// Boot race: the worker inserts the `task_runs` row (and later the session
    /// row) from *inside* the Pod, so every freshly created Job has a window
    /// where its DB owner rows legitimately do not exist yet. Reaping in that
    /// window kills sessions before they start. This replaces the removed
    /// 10-minute `TASKRUN_JOB_REAP_GRACE` in the coordinator backstop: absent
    /// owner rows are `Failure` with no terminal timestamp, so the Job is held
    /// from `created_at` for the full hour — strictly more generous than the
    /// grace window it supersedes.
    #[test]
    fn boot_race_absent_owner_rows_are_held_from_creation() {
        let absent_owner = JobRetentionEvidence {
            created_at: Some(at(0)),
            completed_at: None,
            terminal_condition: None,
            task_run_status: None,
            task_run_ended_at: None,
            sessions: &[],
        };
        let decision = classify_taskrun_job(at(30), absent_owner);
        assert_eq!(decision.outcome, RetentionOutcome::Failure);
        assert_eq!(decision.terminal_at, None);
        // Well past the retired 10-minute grace, still held.
        assert!(!decision.should_delete(at(600)));
        assert!(!decision.should_delete(at(3599)));
        assert!(decision.should_delete(at(3600)));

        // Same for a running task_run whose session row is not inserted yet:
        // that is genuinely live, so it is never age-gated at all.
        let running_without_session = JobRetentionEvidence {
            created_at: Some(at(0)),
            completed_at: None,
            terminal_condition: None,
            task_run_status: Some("running"),
            task_run_ended_at: None,
            sessions: &[],
        };
        let decision = classify_taskrun_job(at(60), running_without_session);
        assert_eq!(decision.outcome, RetentionOutcome::Live);
        assert_eq!(decision.delete_after, None);
        assert!(!decision.should_delete(at(86_400)));
    }

    /// Neither window may drop below the exact-pod watchdog's 300s recovery
    /// grace, or a retention reap can destroy the Job the watchdog needs in
    /// order to confirm its Pod deletion. See the module docs.
    #[test]
    fn retention_windows_cover_the_watchdog_recovery_grace() {
        const WATCHDOG_GRACE: Duration = Duration::from_secs(300);
        assert!(SUCCESS_RETENTION >= WATCHDOG_GRACE);
        assert!(FAILURE_RETENTION >= WATCHDOG_GRACE);
    }
}
