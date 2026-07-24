//! Journal-backed invocation recovery and watchdog-notification tests.
//! Split out of `process_lease_tests.rs` to stay within the file-size guard;
//! shares the lease-runner harness (`ScriptedServices`, `clock`, `status`, …)
//! via `use super::*`.

use super::*;

#[tokio::test]
async fn journal_restart_grace_persists_exact_uid_before_single_watchdog() {
    let directory = tempfile::tempdir().unwrap();
    let identity = TaskInvocationLeaseIdentity {
        task_id: "task".into(),
        task_run_id: "run".into(),
        invocation_id: "durable-invocation".into(),
    };
    let journal =
        InvocationJournal::new(directory.path().to_path_buf(), "immutable-pod-uid".into()).unwrap();
    journal
        .record_at(
            &identity,
            Some(LeaseFencingToken(77)),
            false,
            SystemTime::UNIX_EPOCH,
        )
        .unwrap();
    let reconstructed =
        InvocationJournal::new(directory.path().to_path_buf(), "current-pod-uid".into()).unwrap();
    let services = ScriptedServices::new(vec![], vec![], vec![]);
    let clock = TestClock::new(SystemTime::UNIX_EPOCH, Instant::now());
    let calls = Arc::new(Mutex::new(Vec::new()));
    InvocationRecovery {
        journal: &reconstructed,
        services: &services,
        clock: &clock,
        watchdog_grace: Duration::from_secs(10),
    }
    .run(|request| {
        calls.lock().unwrap().push(request.pod_uid.clone());
        std::future::ready(())
    })
    .await
    .unwrap();
    assert!(calls.lock().unwrap().is_empty());
    assert_eq!(reconstructed.unresolved().unwrap().len(), 1);

    clock.advance_wall(Duration::from_secs(10));
    InvocationRecovery {
        journal: &reconstructed,
        services: &services,
        clock: &clock,
        watchdog_grace: Duration::from_secs(10),
    }
    .run(|request| {
        assert!(reconstructed.unresolved().unwrap()[0].watchdog_notified);
        calls.lock().unwrap().push(request.pod_uid.clone());
        std::future::ready(())
    })
    .await
    .unwrap();
    assert_eq!(*calls.lock().unwrap(), vec!["immutable-pod-uid"]);
    assert_eq!(reconstructed.unresolved().unwrap().len(), 1);

    InvocationRecovery {
        journal: &reconstructed,
        services: &services,
        clock: &clock,
        watchdog_grace: Duration::from_secs(10),
    }
    .run(|_request| async move { panic!("durable watchdog bit must prevent duplicate callback") })
    .await
    .unwrap();
    assert_eq!(reconstructed.unresolved().unwrap().len(), 1);
}

#[tokio::test]
async fn watchdog_notification_survives_matching_lifecycle_advancement() {
    let directory = tempfile::tempdir().unwrap();
    let identity = TaskInvocationLeaseIdentity {
        task_id: "task".into(),
        task_run_id: "run".into(),
        invocation_id: "notified-then-advanced".into(),
    };
    let journal =
        InvocationJournal::new(directory.path().to_path_buf(), "immutable-pod-uid".into()).unwrap();
    journal
        .record_at(
            &identity,
            Some(LeaseFencingToken(31)),
            false,
            SystemTime::UNIX_EPOCH,
        )
        .unwrap();
    let services = ScriptedServices::new(
        vec![],
        vec![],
        vec![LeaseResult::LeaseUnavailable, LeaseResult::LeaseUnavailable],
    );
    let clock = TestClock::new(SystemTime::UNIX_EPOCH, Instant::now());
    let calls = Arc::new(Mutex::new(Vec::new()));

    InvocationRecovery {
        journal: &journal,
        services: &services,
        clock: &clock,
        watchdog_grace: Duration::ZERO,
    }
    .run(|request| {
        calls.lock().unwrap().push(request.pod_uid.clone());
        std::future::ready(())
    })
    .await
    .unwrap();
    assert_eq!(*calls.lock().unwrap(), vec!["immutable-pod-uid"]);

    journal
        .record_at(
            &identity,
            Some(LeaseFencingToken(31)),
            true,
            SystemTime::UNIX_EPOCH + Duration::from_secs(7),
        )
        .unwrap();
    let records = journal.unresolved().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].fence, Some(LeaseFencingToken(31)));
    assert!(records[0].terminal_intent);
    assert!(records[0].watchdog_notified);
    assert_eq!(records[0].recorded_at_ms, 7_000);

    InvocationRecovery {
        journal: &journal,
        services: &services,
        clock: &clock,
        watchdog_grace: Duration::ZERO,
    }
    .run(|request| {
        calls.lock().unwrap().push(request.pod_uid.clone());
        std::future::ready(())
    })
    .await
    .unwrap();
    assert_eq!(*calls.lock().unwrap(), vec!["immutable-pod-uid"]);
    assert!(journal.unresolved().unwrap()[0].watchdog_notified);
}

#[tokio::test]
async fn paused_recovery_notification_preserves_lifecycle_advancement() {
    let directory = tempfile::tempdir().unwrap();
    let identity = TaskInvocationLeaseIdentity {
        task_id: "task".into(),
        task_run_id: "run".into(),
        invocation_id: "advancing-invocation".into(),
    };
    let journal = Arc::new(
        InvocationJournal::new(directory.path().to_path_buf(), "current-pod".into()).unwrap(),
    );
    journal
        .record_at(&identity, None, false, SystemTime::UNIX_EPOCH)
        .unwrap();
    let services = Arc::new(ScriptedServices::new(
        vec![],
        vec![],
        vec![LeaseResult::LeaseUnavailable],
    ));
    services.pause_status.store(true, Ordering::SeqCst);
    let clock = Arc::new(TestClock::new(SystemTime::UNIX_EPOCH, Instant::now()));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let recovery = {
        let journal = journal.clone();
        let services = services.clone();
        let clock = clock.clone();
        let calls = calls.clone();
        tokio::spawn(async move {
            InvocationRecovery {
                journal: journal.as_ref(),
                services: services.as_ref(),
                clock: clock.as_ref(),
                watchdog_grace: Duration::ZERO,
            }
            .run(|request| {
                calls.lock().unwrap().push(request.pod_uid.clone());
                std::future::ready(())
            })
            .await
        })
    };
    services.status_entered.notified().await;

    journal
        .record_at(
            &identity,
            Some(LeaseFencingToken(42)),
            true,
            SystemTime::UNIX_EPOCH + Duration::from_secs(5),
        )
        .unwrap();
    services.pause_status.store(false, Ordering::SeqCst);
    services.status_resume.notify_one();
    recovery.await.unwrap().unwrap();

    let records = journal.unresolved().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].fence, Some(LeaseFencingToken(42)));
    assert!(records[0].terminal_intent);
    assert!(records[0].watchdog_notified);
    assert_eq!(records[0].recorded_at_ms, 5_000);
    assert_eq!(*calls.lock().unwrap(), vec!["current-pod"]);
}

#[tokio::test]
async fn paused_recovery_does_not_resurrect_confirmed_lifecycle_cleanup() {
    let directory = tempfile::tempdir().unwrap();
    let identity = TaskInvocationLeaseIdentity {
        task_id: "task".into(),
        task_run_id: "run".into(),
        invocation_id: "cleared-invocation".into(),
    };
    let journal = Arc::new(
        InvocationJournal::new(directory.path().to_path_buf(), "cleared-pod".into()).unwrap(),
    );
    journal
        .record_at(
            &identity,
            Some(LeaseFencingToken(7)),
            true,
            SystemTime::UNIX_EPOCH,
        )
        .unwrap();
    let services = Arc::new(ScriptedServices::new(
        vec![],
        vec![],
        vec![LeaseResult::LeaseUnavailable],
    ));
    services.pause_status.store(true, Ordering::SeqCst);
    let clock = Arc::new(TestClock::new(SystemTime::UNIX_EPOCH, Instant::now()));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let recovery = {
        let journal = journal.clone();
        let services = services.clone();
        let clock = clock.clone();
        let calls = calls.clone();
        tokio::spawn(async move {
            InvocationRecovery {
                journal: journal.as_ref(),
                services: services.as_ref(),
                clock: clock.as_ref(),
                watchdog_grace: Duration::ZERO,
            }
            .run(|request| {
                calls.lock().unwrap().push(request.pod_uid.clone());
                std::future::ready(())
            })
            .await
        })
    };
    services.status_entered.notified().await;

    // The live lifecycle received matching durable terminal confirmation.
    journal.clear(&identity).unwrap();
    services.pause_status.store(false, Ordering::SeqCst);
    services.status_resume.notify_one();
    recovery.await.unwrap().unwrap();

    assert!(journal.unresolved().unwrap().is_empty());
    assert!(calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn recovery_retains_nonterminal_record_and_counted_lease() {
    let directory = tempfile::tempdir().unwrap();
    let identity = TaskInvocationLeaseIdentity {
        task_id: "task".into(),
        task_run_id: "run".into(),
        invocation_id: "still-live-invocation".into(),
    };
    let journal =
        InvocationJournal::new(directory.path().to_path_buf(), "recorded-pod".into()).unwrap();
    // This is the crash boundary after launch/lift but before terminal intent:
    // recovery may inspect it, but cannot release its counted durable lease.
    journal
        .record_at(
            &identity,
            Some(LeaseFencingToken(9)),
            false,
            SystemTime::UNIX_EPOCH,
        )
        .unwrap();
    let services = ScriptedServices::new(vec![], vec![], vec![status(LeaseState::Active, Some(9))]);
    services
        .release
        .lock()
        .unwrap()
        .push_back(LeaseResult::Released {
            candidate_cleanup: false,
        });
    let clock = TestClock::new(SystemTime::UNIX_EPOCH, Instant::now());
    InvocationRecovery {
        journal: &journal,
        services: &services,
        clock: &clock,
        watchdog_grace: Duration::from_secs(1),
    }
    .run(|request| {
        assert_eq!(request.pod_uid, "recorded-pod");
        std::future::ready(())
    })
    .await
    .unwrap();

    assert_eq!(services.status_calls.load(Ordering::SeqCst), 1);
    assert_eq!(services.release_calls.load(Ordering::SeqCst), 0);
    assert_eq!(journal.unresolved().unwrap().len(), 1);
}

#[tokio::test]
async fn recovery_terminal_intent_retains_every_ambiguous_or_nonterminal_status() {
    let identity = TaskInvocationLeaseIdentity {
        task_id: "task".into(),
        task_run_id: "run".into(),
        invocation_id: "conservative-recovery".into(),
    };
    let cases = vec![
        ("unavailable", LeaseResult::LeaseUnavailable),
        (
            "identity conflict",
            LeaseResult::LeaseIdentityConflict {
                identity: LeaseIdentity::TaskInvocation(identity.clone()),
            },
        ),
        (
            "ambiguous timeout",
            LeaseResult::LeaseWaitTimeout {
                timeout_credit: None,
            },
        ),
        ("active", status(LeaseState::Active, Some(5))),
        ("mismatched fence", status(LeaseState::Bound, Some(6))),
        (
            "mismatched terminal fence",
            status(LeaseState::Released, Some(6)),
        ),
    ];
    for (name, outcome) in cases {
        let directory = tempfile::tempdir().unwrap();
        let journal = InvocationJournal::new(directory.path().to_path_buf(), "pod".into()).unwrap();
        journal
            .record_at(
                &identity,
                Some(LeaseFencingToken(5)),
                true,
                SystemTime::UNIX_EPOCH,
            )
            .unwrap();
        let services = ScriptedServices::new(vec![], vec![], vec![outcome]);
        services
            .release
            .lock()
            .unwrap()
            .push_back(LeaseResult::Released {
                candidate_cleanup: false,
            });
        services
            .abandon
            .lock()
            .unwrap()
            .push_back(LeaseResult::Abandoned {
                candidate_cleanup: false,
            });
        let clock = TestClock::new(SystemTime::UNIX_EPOCH, Instant::now());
        InvocationRecovery {
            journal: &journal,
            services: &services,
            clock: &clock,
            watchdog_grace: Duration::from_secs(1),
        }
        .run(|_request| async move { panic!("{name} must retain the unresolved pod") })
        .await
        .unwrap();
        assert_eq!(services.status_calls.load(Ordering::SeqCst), 1, "{name}");
        assert_eq!(services.release_calls.load(Ordering::SeqCst), 0, "{name}");
        assert_eq!(services.abandon_calls.load(Ordering::SeqCst), 0, "{name}");
        assert_eq!(journal.unresolved().unwrap().len(), 1, "{name}");
    }
}

#[tokio::test]
async fn recovery_abandons_only_a_terminal_intent_queued_record_then_rechecks() {
    let directory = tempfile::tempdir().unwrap();
    let identity = TaskInvocationLeaseIdentity {
        task_id: "task".into(),
        task_run_id: "run".into(),
        invocation_id: "queued-terminal-intent".into(),
    };
    let journal = InvocationJournal::new(directory.path().to_path_buf(), "pod".into()).unwrap();
    journal
        .record_at(&identity, None, true, SystemTime::UNIX_EPOCH)
        .unwrap();
    let services = ScriptedServices::new(
        vec![],
        vec![],
        vec![
            status(LeaseState::Queued, None),
            LeaseResult::Abandoned {
                candidate_cleanup: false,
            },
        ],
    );
    services
        .abandon
        .lock()
        .unwrap()
        .push_back(LeaseResult::LeaseUnavailable);
    let clock = TestClock::new(SystemTime::UNIX_EPOCH, Instant::now());
    InvocationRecovery {
        journal: &journal,
        services: &services,
        clock: &clock,
        watchdog_grace: Duration::ZERO,
    }
    .run(|_request| async move { panic!("confirmed cleanup must not notify") })
    .await
    .unwrap();
    assert_eq!(services.abandon_calls.load(Ordering::SeqCst), 1);
    assert_eq!(services.release_calls.load(Ordering::SeqCst), 0);
    assert_eq!(services.status_calls.load(Ordering::SeqCst), 2);
    assert!(journal.unresolved().unwrap().is_empty());
}

#[tokio::test]
async fn recovery_clears_only_after_terminal_intent_and_matching_confirmation() {
    let directory = tempfile::tempdir().unwrap();
    let identity = TaskInvocationLeaseIdentity {
        task_id: "task".into(),
        task_run_id: "run".into(),
        invocation_id: "confirmed-invocation".into(),
    };
    let journal = InvocationJournal::new(directory.path().to_path_buf(), "pod".into()).unwrap();
    journal
        .record_at(
            &identity,
            Some(LeaseFencingToken(5)),
            false,
            SystemTime::UNIX_EPOCH,
        )
        .unwrap();
    let services =
        ScriptedServices::new(vec![], vec![], vec![status(LeaseState::Released, Some(5))]);
    let clock = TestClock::new(SystemTime::UNIX_EPOCH, Instant::now());
    InvocationRecovery {
        journal: &journal,
        services: &services,
        clock: &clock,
        watchdog_grace: Duration::from_secs(1),
    }
    .run(|_request| async move { panic!("confirmed lease must not notify") })
    .await
    .unwrap();
    assert_eq!(journal.unresolved().unwrap().len(), 1);

    journal
        .record_at(
            &identity,
            Some(LeaseFencingToken(5)),
            true,
            SystemTime::UNIX_EPOCH,
        )
        .unwrap();
    services
        .status
        .lock()
        .unwrap()
        .push_back(status(LeaseState::Released, Some(5)));
    InvocationRecovery {
        journal: &journal,
        services: &services,
        clock: &clock,
        watchdog_grace: Duration::from_secs(1),
    }
    .run(|_request| async move { panic!("confirmed lease must not notify") })
    .await
    .unwrap();
    assert!(journal.unresolved().unwrap().is_empty());
}

/// The grace watchdog must target the exact task/task-run/pod UID recorded in
/// the durable journal — even when the reconstructing process runs as a
/// different pod — and fire that exact-pod request at most once. A lost or
/// unavailable termination response leaves the record and its counted lease in
/// place and must not re-issue the callback on the next scan; only a matching
/// durable terminal confirmation ever clears it.
#[tokio::test]
async fn watchdog_targets_recorded_identity_exactly_once_across_pod_reconstruction() {
    let directory = tempfile::tempdir().unwrap();
    let identity = TaskInvocationLeaseIdentity {
        task_id: "task-A".into(),
        task_run_id: "run-A".into(),
        invocation_id: "durable-A".into(),
    };
    // Recorded while running as pod A, then reconstructed by a process whose
    // own current pod identity is pod B.
    let recording = InvocationJournal::new(directory.path().to_path_buf(), "pod-A".into()).unwrap();
    recording
        .record_at(
            &identity,
            Some(LeaseFencingToken(5)),
            true,
            SystemTime::UNIX_EPOCH,
        )
        .unwrap();
    let reconstructed =
        InvocationJournal::new(directory.path().to_path_buf(), "pod-B".into()).unwrap();
    // The coordinator view stays ambiguous (unavailable) across scans: never a
    // terminal confirmation, so the record and its counted lease are retained.
    let services = ScriptedServices::new(vec![], vec![], vec![LeaseResult::LeaseUnavailable; 4]);
    let clock = TestClock::new(SystemTime::UNIX_EPOCH, Instant::now());
    let requests = Arc::new(Mutex::new(Vec::new()));

    for _ in 0..2 {
        InvocationRecovery {
            journal: &reconstructed,
            services: &services,
            clock: &clock,
            watchdog_grace: Duration::ZERO,
        }
        .run(|request| {
            requests.lock().unwrap().push(request);
            std::future::ready(())
        })
        .await
        .unwrap();
    }

    let fired = requests.lock().unwrap();
    assert_eq!(
        fired.len(),
        1,
        "watchdog must fire exactly once for the recorded pod across scans"
    );
    assert_eq!(fired[0].task_id, "task-A");
    assert_eq!(fired[0].task_run_id, "run-A");
    // The recorded pod UID, not this reconstructing process's pod-B identity.
    assert_eq!(fired[0].pod_uid, "pod-A");
    let records = reconstructed.unresolved().unwrap();
    assert_eq!(records.len(), 1);
    assert!(records[0].watchdog_notified);
}
