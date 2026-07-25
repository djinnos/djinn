//! PostgreSQL contract tests for the durable admission journal.

use std::sync::Arc;

use super::*;

fn input(domain: AdmissionDomain, work_id: &str, generation: i64) -> ReserveAdmissionInput {
    ReserveAdmissionInput {
        key: AdmissionJournalKey {
            domain,
            work_id: work_id.into(),
            generation,
        },
        workload_kind: match domain {
            AdmissionDomain::TaskObservation => AdmissionWorkloadKind::Task,
            AdmissionDomain::WarmBuild => AdmissionWorkloadKind::Warm,
            AdmissionDomain::InvocationBuild => AdmissionWorkloadKind::Invocation,
        },
        creator_server_epoch: "epoch-1".into(),
        object_name: format!("admission-{work_id}-{generation}"),
    }
}

fn create_started(input: &ReserveAdmissionInput) -> CreateStartedInput {
    CreateStartedInput {
        key: input.key.clone(),
        creator_server_epoch: input.creator_server_epoch.clone(),
        object_name: input.object_name.clone(),
    }
}

fn uid_input(input: &ReserveAdmissionInput, object_uid: &str) -> UidFencedAdmissionInput {
    UidFencedAdmissionInput {
        key: input.key.clone(),
        object_uid: object_uid.into(),
    }
}

async fn set_state(db: &Database, key: &AdmissionJournalKey, state: &str) {
    sqlx::query(
        "UPDATE admission_journal SET state = $1, terminal_at = \
         CASE WHEN $1 = 'terminal' THEN now() ELSE NULL END WHERE domain = $2 AND work_id = $3 AND generation = $4",
    )
    .bind(state)
    .bind(key.domain.as_str())
    .bind(&key.work_id)
    .bind(key.generation)
    .execute(db.pool())
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cap_one_concurrent_reservation_has_one_winner() {
    let db = Database::open_in_memory().unwrap();
    let repo = Arc::new(AdmissionJournalRepository::new(db));
    let first = {
        let repo = Arc::clone(&repo);
        tokio::spawn(async move {
            repo.reserve(&input(AdmissionDomain::TaskObservation, "a", 0), 1)
                .await
                .unwrap()
        })
    };
    let second = {
        let repo = Arc::clone(&repo);
        tokio::spawn(async move {
            repo.reserve(&input(AdmissionDomain::WarmBuild, "b", 0), 1)
                .await
                .unwrap()
        })
    };
    let results = [first.await.unwrap(), second.await.unwrap()];
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, ReserveAdmissionResult::Reserved { .. }))
            .count(),
        1
    );
    assert_eq!(repo.count_task_or_warm_occupancy().await.unwrap(), 1);
}

#[tokio::test]
async fn duplicate_reservation_is_idempotent() {
    let db = Database::open_in_memory().unwrap();
    let repo = AdmissionJournalRepository::new(db);
    let input = input(AdmissionDomain::TaskObservation, "same", 0);
    assert!(matches!(
        repo.reserve(&input, 1).await.unwrap(),
        ReserveAdmissionResult::Reserved {
            idempotent: false,
            ..
        }
    ));
    assert!(matches!(
        repo.reserve(&input, 1).await.unwrap(),
        ReserveAdmissionResult::Reserved {
            idempotent: true,
            ..
        }
    ));
    assert_eq!(repo.count_task_or_warm_occupancy().await.unwrap(), 1);
}

#[tokio::test]
async fn all_occupying_states_count_but_terminal_history_does_not() {
    let db = Database::open_in_memory().unwrap();
    let repo = AdmissionJournalRepository::new(db.clone());
    for (index, state) in [
        "reserved",
        "create_in_flight",
        "create_unknown",
        "live",
        "terminal",
    ]
    .iter()
    .enumerate()
    {
        let input = input(
            AdmissionDomain::TaskObservation,
            &format!("work-{index}"),
            0,
        );
        repo.reserve(&input, 10).await.unwrap();
        set_state(&db, &input.key, state).await;
    }
    assert_eq!(repo.count_task_or_warm_occupancy().await.unwrap(), 4);
    let history = repo
        .list_history(AdmissionDomain::TaskObservation, "work-4")
        .await
        .unwrap();
    assert_eq!(history[0].state, AdmissionState::Terminal);
}

#[tokio::test]
async fn reservation_domains_are_separate_but_task_warm_share_cap() {
    let db = Database::open_in_memory().unwrap();
    let repo = AdmissionJournalRepository::new(db);
    assert!(matches!(
        repo.reserve(&input(AdmissionDomain::InvocationBuild, "same", 0), 0)
            .await
            .unwrap(),
        ReserveAdmissionResult::Reserved { .. }
    ));
    assert!(matches!(
        repo.reserve(&input(AdmissionDomain::TaskObservation, "same", 0), 1)
            .await
            .unwrap(),
        ReserveAdmissionResult::Reserved { .. }
    ));
    assert!(matches!(
        repo.reserve(&input(AdmissionDomain::WarmBuild, "same", 0), 1)
            .await
            .unwrap(),
        ReserveAdmissionResult::Denied { .. }
    ));
}

#[tokio::test]
async fn dispatch_generation_resolves_per_object_lifecycle() {
    let db = Database::open_in_memory().unwrap();
    let repo = AdmissionJournalRepository::new(db.clone());
    let resolve = async |requested| {
        repo.resolve_dispatch_generation(AdmissionDomain::TaskObservation, "attempts", requested)
            .await
            .unwrap()
    };

    // No retained row: the caller's own numbering is kept.
    assert_eq!(resolve(0).await, 0);
    assert_eq!(resolve(3).await, 3);

    // A nonterminal generation is resumed, never double-reserved, so a
    // duplicate dispatch or a restart is idempotent.
    let first = input(AdmissionDomain::TaskObservation, "attempts", 0);
    repo.reserve(&first, 4).await.unwrap();
    assert_eq!(resolve(0).await, 0);
    set_state(&db, &first.key, "live").await;
    assert_eq!(resolve(0).await, 0);

    // A retired generation is never reused: the next attempt is a new
    // object with its own UID, so it needs a row that has none yet.
    set_state(&db, &first.key, "terminal").await;
    assert_eq!(resolve(0).await, 1);

    // A caller counter that has run ahead of dispatch is still a floor.
    assert_eq!(resolve(7).await, 7);
    assert!(
        repo.resolve_dispatch_generation(AdmissionDomain::TaskObservation, "attempts", -1)
            .await
            .is_err()
    );
}

/// A create outcome that arrives after the generation is already retired is
/// a defined no-op: the terminal row and its released capacity stand.
#[tokio::test]
async fn late_create_unknown_after_terminal_retains_the_terminal_row() {
    let db = Database::open_in_memory().unwrap();
    let repo = AdmissionJournalRepository::new(db.clone());
    let input = input(AdmissionDomain::WarmBuild, "late", 0);
    repo.reserve(&input, 1).await.unwrap();
    repo.mark_create_started(&create_started(&input))
        .await
        .unwrap();
    repo.mark_live(&uid_input(&input, "uid")).await.unwrap();
    repo.mark_terminal(&TerminalAdmissionInput {
        key: input.key.clone(),
        object_uid: Some("uid".into()),
    })
    .await
    .unwrap();

    let row = repo
        .mark_create_unknown(&input.key)
        .await
        .expect("a late create observation is idempotent");
    assert_eq!(row.state, AdmissionState::Terminal);
    assert_eq!(row.object_uid.as_deref(), Some("uid"));
    assert_eq!(repo.count_task_or_warm_occupancy().await.unwrap(), 0);
}

#[tokio::test]
async fn next_generation_requires_terminal_predecessor() {
    let db = Database::open_in_memory().unwrap();
    let repo = AdmissionJournalRepository::new(db.clone());
    let input = input(AdmissionDomain::TaskObservation, "history", 0);
    assert_eq!(
        repo.allocate_next_generation(AdmissionDomain::TaskObservation, "history")
            .await
            .unwrap(),
        0
    );
    repo.reserve(&input, 1).await.unwrap();
    assert!(matches!(
        repo.allocate_next_generation(AdmissionDomain::TaskObservation, "history")
            .await,
        Err(DbError::InvalidTransition(_))
    ));
    set_state(&db, &input.key, "terminal").await;
    assert_eq!(
        repo.allocate_next_generation(AdmissionDomain::TaskObservation, "history")
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn definitive_and_ambiguous_create_failures_have_distinct_occupancy() {
    let db = Database::open_in_memory().unwrap();
    let repo = AdmissionJournalRepository::new(db);
    let reserved = input(AdmissionDomain::TaskObservation, "definitive-reserved", 0);
    let in_flight = input(AdmissionDomain::TaskObservation, "definitive-flight", 0);
    let ambiguous = input(AdmissionDomain::TaskObservation, "ambiguous", 0);
    for reservation in [&reserved, &in_flight, &ambiguous] {
        repo.reserve(reservation, 3).await.unwrap();
    }

    assert_eq!(
        repo.mark_definitive_create_failure(&reserved.key)
            .await
            .unwrap()
            .state,
        AdmissionState::Terminal
    );
    assert_eq!(
        repo.mark_definitive_create_failure(&reserved.key)
            .await
            .unwrap()
            .state,
        AdmissionState::Terminal
    );
    repo.mark_create_started(&create_started(&in_flight))
        .await
        .unwrap();
    assert_eq!(
        repo.mark_definitive_create_failure(&in_flight.key)
            .await
            .unwrap()
            .state,
        AdmissionState::Terminal
    );

    repo.mark_create_started(&create_started(&ambiguous))
        .await
        .unwrap();
    assert_eq!(
        repo.mark_create_started(&create_started(&ambiguous))
            .await
            .unwrap()
            .state,
        AdmissionState::CreateInFlight
    );
    assert_eq!(
        repo.mark_create_unknown(&ambiguous.key)
            .await
            .unwrap()
            .state,
        AdmissionState::CreateUnknown
    );
    assert_eq!(
        repo.mark_create_unknown(&ambiguous.key)
            .await
            .unwrap()
            .state,
        AdmissionState::CreateUnknown
    );
    // LIST absence is deliberately not an input to this repository: ambiguity occupies.
    assert_eq!(repo.count_task_or_warm_occupancy().await.unwrap(), 1);
    assert_eq!(repo.list_active_rows().await.unwrap()[0].key, ambiguous.key);
    assert_eq!(
        repo.mark_live(&uid_input(&ambiguous, "uid-ambiguous"))
            .await
            .unwrap()
            .state,
        AdmissionState::Live
    );
}

#[tokio::test]
async fn cancellation_is_reserved_only_and_idempotent() {
    let db = Database::open_in_memory().unwrap();
    let repo = AdmissionJournalRepository::new(db);
    let cancelled = input(AdmissionDomain::TaskObservation, "cancelled", 0);
    let posted = input(AdmissionDomain::TaskObservation, "posted", 0);
    repo.reserve(&cancelled, 2).await.unwrap();
    repo.reserve(&posted, 2).await.unwrap();
    assert_eq!(
        repo.cancel_reserved(&cancelled.key).await.unwrap().state,
        AdmissionState::Terminal
    );
    assert_eq!(
        repo.cancel_reserved(&cancelled.key).await.unwrap().state,
        AdmissionState::Terminal
    );
    repo.mark_create_started(&create_started(&posted))
        .await
        .unwrap();
    assert!(matches!(
        repo.cancel_reserved(&posted.key).await,
        Err(DbError::InvalidTransition(_))
    ));
    assert_eq!(repo.count_task_or_warm_occupancy().await.unwrap(), 1);
}

#[tokio::test]
async fn stale_generations_and_mismatched_uids_cannot_release_current_work() {
    let db = Database::open_in_memory().unwrap();
    let repo = AdmissionJournalRepository::new(db);
    let first = input(AdmissionDomain::TaskObservation, "fenced", 0);
    repo.reserve(&first, 1).await.unwrap();
    repo.mark_create_started(&create_started(&first))
        .await
        .unwrap();
    repo.mark_live(&uid_input(&first, "uid-first"))
        .await
        .unwrap();
    repo.mark_terminal(&TerminalAdmissionInput {
        key: first.key.clone(),
        object_uid: Some("uid-first".into()),
    })
    .await
    .unwrap();

    let second = input(AdmissionDomain::TaskObservation, "fenced", 1);
    repo.reserve(&second, 1).await.unwrap();
    repo.mark_create_started(&create_started(&second))
        .await
        .unwrap();
    repo.mark_live(&uid_input(&second, "uid-current"))
        .await
        .unwrap();
    assert!(matches!(
        repo.mark_live(&uid_input(&first, "uid-first")).await,
        Err(DbError::InvalidTransition(_))
    ));
    assert!(matches!(
        repo.mark_terminal(&TerminalAdmissionInput {
            key: second.key.clone(),
            object_uid: Some("wrong-uid".into()),
        })
        .await,
        Err(DbError::InvalidTransition(_))
    ));
    assert_eq!(
        repo.mark_live(&uid_input(&second, "uid-current"))
            .await
            .unwrap()
            .state,
        AdmissionState::Live
    );
    assert_eq!(repo.count_task_or_warm_occupancy().await.unwrap(), 1);
    for _ in 0..2 {
        assert_eq!(
            repo.mark_terminal(&TerminalAdmissionInput {
                key: second.key.clone(),
                object_uid: Some("uid-current".into()),
            })
            .await
            .unwrap()
            .state,
            AdmissionState::Terminal
        );
    }
    assert_eq!(repo.count_task_or_warm_occupancy().await.unwrap(), 0);
}

#[tokio::test]
async fn predecessor_recovery_retires_only_reserved_and_retains_ambiguous_work() {
    let db = Database::open_in_memory().unwrap();
    let repo = AdmissionJournalRepository::new(db);
    let reserved = input(AdmissionDomain::TaskObservation, "recover-reserved", 0);
    let flight = input(AdmissionDomain::TaskObservation, "recover-flight", 0);
    let unknown = input(AdmissionDomain::TaskObservation, "recover-unknown", 0);
    let live = input(AdmissionDomain::TaskObservation, "recover-live", 0);
    let mut successor = input(AdmissionDomain::TaskObservation, "recover-successor", 0);
    successor.creator_server_epoch = "epoch-2".into();
    for reservation in [&reserved, &flight, &unknown, &live, &successor] {
        repo.reserve(reservation, 5).await.unwrap();
    }
    repo.mark_create_started(&create_started(&flight))
        .await
        .unwrap();
    repo.mark_create_started(&create_started(&unknown))
        .await
        .unwrap();
    repo.mark_create_unknown(&unknown.key).await.unwrap();
    repo.mark_create_started(&create_started(&live))
        .await
        .unwrap();
    repo.mark_live(&uid_input(&live, "uid-live")).await.unwrap();

    let report = repo.recover_predecessor_epoch("epoch-1").await.unwrap();
    assert_eq!(report.retired_reserved, 1);
    assert_eq!(report.marked_create_unknown, 1);
    let states = report
        .active_rows
        .iter()
        .map(|row| (row.key.work_id.as_str(), row.state))
        .collect::<Vec<_>>();
    assert_eq!(
        states,
        vec![
            ("recover-flight", AdmissionState::CreateUnknown),
            ("recover-live", AdmissionState::Live),
            ("recover-successor", AdmissionState::Reserved),
            ("recover-unknown", AdmissionState::CreateUnknown),
        ]
    );
}

#[tokio::test]
async fn recover_all_predecessors_retires_every_predecessor_epoch_atomically() {
    let db = Database::open_in_memory().unwrap();
    let repo = AdmissionJournalRepository::new(db);
    // Two distinct predecessor epochs plus the current replacement epoch.
    let mut pred_a = input(AdmissionDomain::WarmBuild, "pred-a-reserved", 0);
    pred_a.creator_server_epoch = "epoch-a".into();
    let mut pred_a_flight = input(AdmissionDomain::WarmBuild, "pred-a-flight", 0);
    pred_a_flight.creator_server_epoch = "epoch-a".into();
    let mut pred_b = input(AdmissionDomain::WarmBuild, "pred-b-reserved", 0);
    pred_b.creator_server_epoch = "epoch-b".into();
    let mut current = input(AdmissionDomain::WarmBuild, "current-reserved", 0);
    current.creator_server_epoch = "replacement-epoch".into();
    for reservation in [&pred_a, &pred_a_flight, &pred_b, &current] {
        repo.reserve(reservation, 10).await.unwrap();
    }
    repo.mark_create_started(&create_started(&pred_a_flight))
        .await
        .unwrap();

    // recover_all_predecessors processes every epoch except the current one.
    let report = repo
        .recover_all_predecessors("replacement-epoch")
        .await
        .unwrap();
    assert_eq!(
        report.retired_reserved, 2,
        "both predecessor Reserved retired"
    );
    assert_eq!(
        report.marked_create_unknown, 1,
        "the single predecessor CreateInFlight converted to CreateUnknown"
    );
    // The current-epoch Reserved row is untouched.
    let states = report
        .active_rows
        .iter()
        .map(|row| (row.key.work_id.as_str(), row.state))
        .collect::<Vec<_>>();
    assert_eq!(
        states,
        vec![
            ("current-reserved", AdmissionState::Reserved),
            ("pred-a-flight", AdmissionState::CreateUnknown),
        ]
    );
}

#[tokio::test]
async fn reclaim_absent_object_is_fenced_by_the_full_observed_identity() {
    let repo = AdmissionJournalRepository::new(Database::open_in_memory().unwrap());
    let reserved = input(AdmissionDomain::WarmBuild, "reclaim", 0);
    repo.reserve(&reserved, 4).await.unwrap();
    repo.mark_create_started(&create_started(&reserved))
        .await
        .unwrap();
    repo.recover_all_predecessors("replacement-epoch")
        .await
        .unwrap();
    let row = repo.list_active_rows().await.unwrap().remove(0);
    assert_eq!(row.state, AdmissionState::CreateUnknown);

    let proof = ReclaimAbsentInput {
        key: row.key.clone(),
        observed_state: row.state,
        observed_creator_server_epoch: row.creator_server_epoch.clone(),
        observed_object_name: row.object_name.clone(),
        observed_object_uid: row.object_uid.clone(),
    };

    // A proof taken against a different state writes nothing.
    let stale_proof = ReclaimAbsentInput {
        observed_state: AdmissionState::Reserved,
        ..proof.clone()
    };
    assert!(matches!(
        repo.reclaim_absent_object(&stale_proof).await.unwrap(),
        ReclaimAbsentOutcome::Fenced { .. }
    ));
    assert_eq!(repo.count_task_or_warm_occupancy().await.unwrap(), 1);

    // A proof taken against a different object name writes nothing either: the
    // absence that was proven was some other object's.
    let renamed = ReclaimAbsentInput {
        observed_object_name: "a-different-job".into(),
        ..proof.clone()
    };
    assert!(matches!(
        repo.reclaim_absent_object(&renamed).await.unwrap(),
        ReclaimAbsentOutcome::Fenced { .. }
    ));
    assert_eq!(repo.count_task_or_warm_occupancy().await.unwrap(), 1);

    // The matching proof retires the row and releases its capacity.
    assert!(matches!(
        repo.reclaim_absent_object(&proof).await.unwrap(),
        ReclaimAbsentOutcome::Reclaimed(_)
    ));
    assert_eq!(repo.count_task_or_warm_occupancy().await.unwrap(), 0);

    // Replaying the same proof is an idempotent no-op, not an error.
    assert!(matches!(
        repo.reclaim_absent_object(&proof).await.unwrap(),
        ReclaimAbsentOutcome::AlreadyTerminal(_)
    ));

    // A retired generation still yields the next one to a new dispatch, so
    // reclamation composes with generation resolution instead of stranding the
    // work item on a generation that can never advance.
    assert_eq!(
        repo.resolve_dispatch_generation(AdmissionDomain::WarmBuild, "reclaim", 0)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn settlement_flags_rows_against_the_database_clock() {
    let repo = AdmissionJournalRepository::new(Database::open_in_memory().unwrap());
    let reserved = input(AdmissionDomain::TaskObservation, "settle", 0);
    repo.reserve(&reserved, 4).await.unwrap();

    let settled_now = repo.list_active_rows_with_settlement(0).await.unwrap();
    assert_eq!(settled_now.len(), 1);
    assert!(settled_now[0].1, "a zero window settles immediately");

    let unsettled = repo.list_active_rows_with_settlement(3600).await.unwrap();
    assert!(
        !unsettled[0].1,
        "a row written moments ago is not settled against an hour-long window"
    );
    assert!(repo.list_active_rows_with_settlement(-1).await.is_err());
}
