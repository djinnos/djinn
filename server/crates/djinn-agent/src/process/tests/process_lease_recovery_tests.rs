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

/// Regression: a non-escalating invocation must leave NO journal record.
///
/// The journal exists to recover a lease this pod may have created at the
/// coordinator. An invocation whose CPU never crosses
/// `cpu_usage_threshold_usec` never calls `queue_lease`, so it owns no durable
/// coordinator state and has nothing to recover.
///
/// Production (2026-07-28) wrote the record unconditionally at the top of
/// `output()` while the terminal clear stayed gated on `queued`. Every cheap
/// command therefore left a permanently-unresolved record; the pod-local
/// recovery sweep read it as an orphan and fired the exact-pod watchdog against
/// its OWN pod. With a 300s grace and a 300s sweep tick, every worker pod
/// deleted itself ~600s after start and its task bounced `in_progress -> open`
/// forever.
///
/// Asserting `unresolved()` is empty is the side effect that matters: it is
/// exactly what the sweep reads. `queue_calls == 0` pins the precondition, so
/// this cannot pass by the invocation quietly having escalated.
#[tokio::test]
async fn non_escalating_invocation_leaves_no_journal_record() {
    let directory = tempfile::tempdir().unwrap();
    let journal =
        Arc::new(InvocationJournal::new(directory.path().to_path_buf(), "pod-uid".into()).unwrap());
    let services = Arc::new(ScriptedServices::new(vec![], vec![], vec![]));
    // CPU stays two orders of magnitude below the escalation threshold, and the
    // child exits naturally after a couple of polls.
    let launcher = Arc::new(FixtureLauncher::new(500, Some(2)));
    let runner = LeaseInvocationRunner::new(
        services.clone(),
        services.clone(),
        launcher.clone(),
        clock(),
    )
    .with_journal(journal.clone());

    let output = runner
        .output(
            // `command()`, not `cmd_from(..)`: the fixture drives escalation
            // from `FixtureLauncher`'s CPU sample, so the command string is
            // decorative — but a real child still spawns. `command()` is the
            // process-group-isolated `sleep` this file already uses; a literal
            // `cargo build` here would contend for the target-dir lock with the
            // harness's own cargo and flake the timeout-sensitive tests.
            command(),
            config_with_threshold(1_000_000),
            CancellationToken::new(),
        )
        .await
        .expect("a cheap command runs to natural exit");

    assert_eq!(output.process.termination, ProcessTermination::Exited);
    assert_eq!(
        services.queue_calls.load(Ordering::SeqCst),
        0,
        "precondition: this invocation must never have escalated to the lease authority"
    );
    assert!(
        journal.unresolved().unwrap().is_empty(),
        "a non-escalating invocation must leave nothing for the recovery sweep to \
         mistake for an orphaned pod; a record here re-arms the ~600s exact-pod watchdog"
    );
}

/// The write-ahead property the journal exists for: for an invocation that DOES
/// escalate, the record must be durable *before* the `queue_lease` request goes
/// out, so a pod that dies mid-request still leaves evidence to reconcile.
///
/// Proven by pausing the queue response and reading the journal while the RPC is
/// genuinely in flight — moving the write any later than the request would make
/// this observation empty.
#[tokio::test]
async fn escalating_invocation_journals_before_the_queue_request() {
    let directory = tempfile::tempdir().unwrap();
    let journal =
        Arc::new(InvocationJournal::new(directory.path().to_path_buf(), "pod-uid".into()).unwrap());
    let services = Arc::new(ScriptedServices::new(vec![], vec![], vec![]));
    services.pause_queue.store(true, Ordering::SeqCst);
    // CPU is above the threshold on the first sample, so the invocation
    // escalates immediately and blocks on the paused queue response.
    let launcher = Arc::new(FixtureLauncher::new(5_000, None));
    let test_clock = clock();
    let runner = LeaseInvocationRunner::new(
        services.clone(),
        services.clone(),
        launcher.clone(),
        test_clock.clone(),
    )
    .with_journal(journal.clone());

    let run_services = services.clone();
    let run = tokio::spawn(async move {
        runner
            .output(
                // Process-group-isolated `sleep`, never a real `cargo build` —
                // see the note in the sibling test above.
                command(),
                config_with_threshold(1_000),
                CancellationToken::new(),
            )
            .await
    });

    poll_until(move || run_services.queue_calls.load(Ordering::SeqCst) >= 1).await;
    let in_flight = journal.unresolved().unwrap();
    assert_eq!(
        in_flight.len(),
        1,
        "the record must already be durable while the queue_lease RPC is in flight"
    );
    assert!(
        !in_flight[0].terminal_intent,
        "the write-ahead record is not terminal yet"
    );

    // Let the paused invocation fall through its deadline and finish cleanly.
    test_clock.advance_mono(Duration::from_secs(120));
    let _ = run.await.expect("join");
}

/// Regression: the sweep must not reap an invocation this process is still
/// executing.
///
/// A durable record exists for the whole life of an escalated command. The
/// sweep cannot tell from the file whether that command is running or was
/// orphaned by a dead predecessor, so it read every long `cargo` build as an
/// orphan and fired the exact-pod watchdog against its OWN Pod. Production
/// (2026-07-28) killed workers mid-compile this way even after the
/// non-escalated record leak was fixed: `kziu` died at 15m and `3iir` at 10m,
/// both on live cargo invocations.
///
/// The record is deliberately recorded far beyond the grace and the clock
/// advanced past it, so the ONLY thing standing between this record and a
/// termination request is the liveness claim.
#[tokio::test]
async fn a_live_invocation_is_never_reaped_by_its_own_watchdog() {
    let directory = tempfile::tempdir().unwrap();
    let identity = TaskInvocationLeaseIdentity {
        task_id: "task".into(),
        task_run_id: "run".into(),
        invocation_id: "long-running-cargo".into(),
    };
    let journal = InvocationJournal::new(directory.path().to_path_buf(), "pod-uid".into()).unwrap();
    journal.begin_live(&identity);
    journal
        .record_at(
            &identity,
            Some(LeaseFencingToken(11)),
            false,
            SystemTime::UNIX_EPOCH,
        )
        .unwrap();

    let services = ScriptedServices::new(vec![], vec![], vec![]);
    let clock = TestClock::new(SystemTime::UNIX_EPOCH, Instant::now());
    clock.advance_wall(Duration::from_secs(3_600));
    let calls = Arc::new(Mutex::new(Vec::new()));
    InvocationRecovery {
        journal: &journal,
        services: &services,
        clock: &clock,
        watchdog_grace: Duration::from_secs(300),
    }
    .run(|request| {
        calls.lock().unwrap().push(request.pod_uid.clone());
        std::future::ready(())
    })
    .await
    .unwrap();

    assert!(
        calls.lock().unwrap().is_empty(),
        "a command still running in this process must never make its own Pod a \
         watchdog target, however long it has been compiling"
    );
    let retained = journal.unresolved().unwrap();
    assert_eq!(
        retained.len(),
        1,
        "the live record is retained, not cleared"
    );
    assert!(
        !retained[0].watchdog_notified,
        "no notification bit may be burned on a live invocation — that bit is \
         one-shot, so setting it here would forfeit the real reap later"
    );
}

/// The complement, and the reason the claim is a drop guard: once the
/// invocation finishes, its record is reapable again. Without this the fix
/// would trade a false kill for a permanently disarmed watchdog, which is the
/// orphan hole the journal exists to close.
#[tokio::test]
async fn a_finished_invocations_record_becomes_reapable_again() {
    let directory = tempfile::tempdir().unwrap();
    let identity = TaskInvocationLeaseIdentity {
        task_id: "task".into(),
        task_run_id: "run".into(),
        invocation_id: "finished-cargo".into(),
    };
    let journal = InvocationJournal::new(directory.path().to_path_buf(), "pod-uid".into()).unwrap();
    journal.begin_live(&identity);
    journal
        .record_at(
            &identity,
            Some(LeaseFencingToken(12)),
            false,
            SystemTime::UNIX_EPOCH,
        )
        .unwrap();
    // The command exited; the drop guard released the claim.
    journal.finish_live(&identity.invocation_id);

    let services = ScriptedServices::new(vec![], vec![], vec![]);
    let clock = TestClock::new(SystemTime::UNIX_EPOCH, Instant::now());
    clock.advance_wall(Duration::from_secs(3_600));
    let calls = Arc::new(Mutex::new(Vec::new()));
    InvocationRecovery {
        journal: &journal,
        services: &services,
        clock: &clock,
        watchdog_grace: Duration::from_secs(300),
    }
    .run(|request| {
        calls.lock().unwrap().push(request.pod_uid.clone());
        std::future::ready(())
    })
    .await
    .unwrap();

    assert_eq!(
        calls.lock().unwrap().len(),
        1,
        "a grace-expired record with no live owner is exactly the orphan the \
         watchdog exists for"
    );
}
