//! Deterministic retention-boundary tests for retrieval traces.

use super::*;

#[test]
fn minimum_retrieval_trace_retention_window_is_thirty_days() {
    assert_eq!(
        MINIMUM_RETRIEVAL_TRACE_RETENTION_WINDOW,
        std::time::Duration::from_secs(30 * 24 * 60 * 60)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pruning_protects_boundary_and_retained_disabled_outcomes() {
    let db = test_db();
    let project_id = "019f4900-0000-7000-8000-000000000017";
    let other_project = "019f4900-0000-7000-8000-000000000018";
    seed_project(&db, project_id).await;
    seed_project(&db, other_project).await;
    let repo = RetrievalTraceRepository::new(db.clone());
    let skipped = json!([skipped_candidate(
        "suppressed",
        1,
        0.1,
        SkippedReason::NotTopK
    )]);

    let older = insert_trace(
        &repo,
        project_id,
        &json!([]),
        DEFAULT_CANDIDATE_CAP,
        false,
        None,
    )
    .await;
    let boundary = insert_trace(
        &repo,
        project_id,
        &json!([]),
        DEFAULT_CANDIDATE_CAP,
        false,
        None,
    )
    .await;
    let disabled_off = repo
        .insert_with_semantics(CreateRetrievalTraceWithSemanticsParams {
            trace: CreateRetrievalTraceParams {
                project_id,
                session_id: None,
                task_run_id: None,
                task_id: None,
                entry_point: RetrievalTraceEntryPoint::LoadKnowledgeContext,
                trigger: None,
                candidates: &skipped,
                candidate_cap: DEFAULT_CANDIDATE_CAP,
                candidate_cap_exceeded: false,
                sampling_metadata: None,
                durations_ms: &json!({}),
                estimated_injected_tokens: 0,
            },
            rollout_label: "off",
            outcome: RetrievalTraceOutcome::DisabledOff,
        })
        .await
        .unwrap();
    let disabled_kill_switch = repo
        .insert_with_semantics(CreateRetrievalTraceWithSemanticsParams {
            trace: CreateRetrievalTraceParams {
                project_id,
                session_id: None,
                task_run_id: None,
                task_id: None,
                entry_point: RetrievalTraceEntryPoint::LoadKnowledgeContext,
                trigger: None,
                candidates: &skipped,
                candidate_cap: DEFAULT_CANDIDATE_CAP,
                candidate_cap_exceeded: false,
                sampling_metadata: None,
                durations_ms: &json!({}),
                estimated_injected_tokens: 0,
            },
            rollout_label: "kill_switch",
            outcome: RetrievalTraceOutcome::DisabledKillSwitch,
        })
        .await
        .unwrap();
    let other_old = insert_trace(
        &repo,
        other_project,
        &json!([]),
        DEFAULT_CANDIDATE_CAP,
        false,
        None,
    )
    .await;

    backdate_created_at(&db, &older.id, "2026-07-31T23:59:59.999Z").await;
    backdate_created_at(&db, &boundary.id, "2026-08-01T00:00:00.000Z").await;
    backdate_created_at(&db, &disabled_off.id, "2026-08-02T00:00:00.000Z").await;
    backdate_created_at(&db, &disabled_kill_switch.id, "2026-08-03T00:00:00.000Z").await;
    backdate_created_at(&db, &other_old.id, "2026-07-01T00:00:00.000Z").await;

    let protected = repo
        .prune_older_than(
            project_id,
            "2026-08-02T00:00:00.000Z",
            "2026-08-31T00:00:00.000Z",
        )
        .await
        .unwrap_err();
    assert!(format!("{protected}").contains("protected retrieval-trace retention window"));

    let malformed_cutoff = repo
        .prune_older_than(project_id, "not-a-timestamp", "2026-08-31T00:00:00.000Z")
        .await
        .unwrap_err();
    assert!(format!("{malformed_cutoff}").contains("before_cutoff must be a valid"));
    let malformed_reference = repo
        .prune_older_than(
            project_id,
            "2026-08-01T00:00:00.000Z",
            "2026-08-31T00:00:00+00:00",
        )
        .await
        .unwrap_err();
    assert!(format!("{malformed_reference}").contains("reference_time must be an ISO-8601 UTC"));

    let pruned = repo
        .prune_older_than(
            project_id,
            "2026-08-01T00:00:00.000Z",
            "2026-08-31T00:00:00.000Z",
        )
        .await
        .unwrap();
    assert_eq!(
        pruned, 1,
        "only rows strictly older than the boundary are eligible"
    );
    assert!(repo.get_by_id(&older.id).await.unwrap().is_none());
    assert!(
        repo.get_by_id(&boundary.id).await.unwrap().is_some(),
        "cutoff is strict"
    );
    assert!(
        repo.get_by_id(&other_old.id).await.unwrap().is_some(),
        "pruning is project-scoped"
    );

    let off = repo
        .list_by_project(
            project_id,
            RetrievalTraceListFilter {
                trace_outcome: Some(RetrievalTraceOutcome::DisabledOff),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(off.len(), 1);
    assert_eq!(off[0].id, disabled_off.id);
    assert_eq!(off[0].outcome, RetrievalTraceOutcome::DisabledOff);
    let kill = repo
        .list_by_project(
            project_id,
            RetrievalTraceListFilter {
                trace_outcome: Some(RetrievalTraceOutcome::DisabledKillSwitch),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(kill.len(), 1);
    assert_eq!(kill[0].id, disabled_kill_switch.id);
    assert_eq!(kill[0].outcome, RetrievalTraceOutcome::DisabledKillSwitch);
}
