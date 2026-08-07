use std::collections::HashSet;

use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::models::Task;
use djinn_db::{ActivityQuery, Database, NoteRepository, TaskRepository};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

fn parse_task_memory_refs(memory_refs: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(memory_refs).unwrap_or_else(|_| {
        tracing::warn!(memory_refs = %memory_refs, "invalid memory_refs JSON for task completion signal");
        Vec::new()
    })
}

const COMPLETED_STATUS: &str = "closed";
const COMPLETED_REASON: &str = "completed";

/// Activity event type recorded on successful task completion.
///
/// The name is historical. Since 9xih this listener applies NO confidence
/// signal: the `TASK_SUCCESS_SIGNAL = 0.65` `update_confidence` call it used to
/// make was removed. A task closing successfully while referencing a note is
/// evidence that the note was USED, not that it is TRUE, and `notes.confidence`
/// also gates injection eligibility and archival lifecycle — so writing a task
/// outcome into it silently changed what the system may inject and archive.
///
/// The event type string is deliberately NOT renamed: it is the dedupe key read
/// back by `has_already_applied`, and rows carrying it already exist in every
/// deployed database. Renaming it would make every historically-completed task
/// look unprocessed.
const CONFIDENCE_ACTIVITY_TYPE: &str = "confidence_signal_applied";

#[derive(Debug, Deserialize)]
struct TaskUpdatedPayload {
    task: Task,
    #[serde(default)]
    from_sync: bool,
}

#[derive(Debug, Serialize)]
struct ConfidenceSignalPayload {
    reason: &'static str,
    /// Note ids the task's `memory_refs` resolved to.
    ///
    /// The JSON key stays `updated_notes` because it is already persisted in
    /// `task_activity` rows across every deployment and is read back by
    /// operators and the timeline UI; since 9xih no confidence value is
    /// actually updated, so read it as "notes this completion referenced".
    updated_notes: Vec<String>,
    missing_notes: usize,
    from_sync: bool,
}

/// Spawn the task-outcome listener. When a task transitions to a successful
/// terminal state, a completion-activity row is recorded naming the notes it
/// referenced.
///
/// Since 9xih this listener writes NO note confidence. It sits on the forbidden
/// side of the epistemic write boundary described in
/// `djinn_db::repositories::note::scoring`: a task outcome is evidence about
/// usefulness, never about truth.
///
/// `events` is the broadcast sender the listener subscribes to. `db` and
/// `event_bus` are used to construct the underlying repositories.
pub fn spawn_task_outcome_listener(
    db: Database,
    event_bus: EventBus,
    events: &broadcast::Sender<DjinnEventEnvelope>,
) {
    let mut rx = events.subscribe();
    let task_repo = TaskRepository::new(db.clone(), event_bus.clone());
    let note_repo = NoteRepository::new(db, event_bus);

    tokio::spawn(async move {
        loop {
            let envelope = match rx.recv().await {
                Ok(envelope) => envelope,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    tracing::warn!("task outcome listener missed events due lag");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    tracing::debug!("task outcome listener stopping: event bus closed");
                    break;
                }
            };

            if envelope.entity_type != "task" || envelope.action != "updated" {
                continue;
            }

            let payload = match envelope.parse_payload::<TaskUpdatedPayload>() {
                Some(payload) => payload,
                None => {
                    tracing::warn!(
                        "failed to deserialize task_updated payload in task outcome listener"
                    );
                    continue;
                }
            };

            if !is_successful_completion(&payload.task) {
                continue;
            }

            handle_successful_task_completion(&payload, &task_repo, &note_repo).await;
        }
    });
}

fn is_successful_completion(task: &Task) -> bool {
    task.status == COMPLETED_STATUS && task.close_reason.as_deref() == Some(COMPLETED_REASON)
}

async fn has_already_applied(task_repo: &TaskRepository, task_id: &str) -> bool {
    let query = ActivityQuery {
        task_id: Some(task_id.to_owned()),
        event_type: Some(CONFIDENCE_ACTIVITY_TYPE.to_owned()),
        limit: 200,
        ..Default::default()
    };

    let entries = match task_repo.query_activity(query).await {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(error=%error, task_id = %task_id, "failed to read task activity for confidence signal dedupe");
            return false;
        }
    };

    entries.iter().any(|entry| {
        serde_json::from_str::<serde_json::Value>(&entry.payload)
            .ok()
            .and_then(|payload| {
                payload
                    .get("reason")
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned)
            })
            == Some(COMPLETED_REASON.to_owned())
    })
}

async fn handle_successful_task_completion(
    payload: &TaskUpdatedPayload,
    task_repo: &TaskRepository,
    note_repo: &NoteRepository,
) {
    if has_already_applied(task_repo, &payload.task.id).await {
        return;
    }

    let memory_refs = parse_task_memory_refs(&payload.task.memory_refs);
    if memory_refs.is_empty() {
        if let Err(error) = record_confidence_signal(
            task_repo,
            &payload.task.id,
            Vec::new(),
            0,
            payload.from_sync,
        )
        .await
        {
            tracing::warn!(
                error = %error,
                task_id = %payload.task.id,
                "failed to record confidence signal activity for task completion"
            );
        }
        return;
    }

    let mut seen = HashSet::new();
    let mut resolved_notes = Vec::new();
    let mut missing_refs = 0usize;

    for permalink in memory_refs {
        if !seen.insert(permalink.clone()) {
            continue;
        }

        match note_repo
            .get_by_permalink(&payload.task.project_id, &permalink)
            .await
        {
            // Resolution only. This deliberately does NOT call
            // `update_confidence` (9xih): task completion is a retrieval/task
            // outcome, not epistemic evidence about the note. The resolved ids
            // are still recorded so completion remains observable.
            Ok(Some(note)) => {
                resolved_notes.push(note.id);
            }
            Ok(None) => {
                missing_refs += 1;
                tracing::debug!(task_id = %payload.task.id, permalink, "skipping missing task memory reference");
            }
            Err(error) => {
                missing_refs += 1;
                tracing::warn!(
                    error = %error,
                    task_id = %payload.task.id,
                    permalink,
                    "failed to resolve task memory reference"
                );
            }
        }
    }

    if let Err(error) = record_confidence_signal(
        task_repo,
        &payload.task.id,
        resolved_notes,
        missing_refs,
        payload.from_sync,
    )
    .await
    {
        tracing::warn!(
            error = %error,
            task_id = %payload.task.id,
            "failed to record confidence signal activity for task completion"
        );
    }
}

async fn record_confidence_signal(
    task_repo: &TaskRepository,
    task_id: &str,
    updated_notes: Vec<String>,
    missing_notes: usize,
    from_sync: bool,
) -> djinn_db::Result<()> {
    let payload = ConfidenceSignalPayload {
        reason: COMPLETED_REASON,
        updated_notes,
        missing_notes,
        from_sync,
    };

    let payload = serde_json::to_string(&payload).unwrap_or_else(|error| {
        tracing::warn!(
            error = %error,
            task_id,
            "failed to serialize confidence signal payload, falling back to empty json"
        );
        "{}".to_owned()
    });

    task_repo
        .log_activity(
            Some(task_id),
            "task-confidence-listener",
            "system",
            CONFIDENCE_ACTIVITY_TYPE,
            &payload,
        )
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    use djinn_core::events::DjinnEventEnvelope;
    use djinn_db::repositories::task::ActivityQuery;
    use djinn_db::test_support::event_bus_for;
    use djinn_db::{EffectiveCreatorProvenance, NoteRepository, TaskRepository};
    use tokio::sync::broadcast;
    use tokio::time::timeout;

    const WAIT_TIMEOUT: Duration = Duration::from_secs(3);
    const EVENT_CHANNEL_CAPACITY: usize = 1024;

    struct TestHarness {
        db: Database,
        events: broadcast::Sender<DjinnEventEnvelope>,
        event_bus: EventBus,
    }

    async fn make_harness() -> TestHarness {
        let db = crate::test_helpers::create_test_db();
        db.ensure_initialized().await.expect("ensure initialized");
        let (events, _rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let event_bus = event_bus_for(&events);
        TestHarness {
            db,
            events,
            event_bus,
        }
    }

    fn temp_project_path(name: &str) -> PathBuf {
        std::env::current_dir()
            .expect("current dir")
            .join("target")
            .join("test-tmp")
            .join(name)
    }

    async fn create_test_project(
        project_repo: &djinn_db::ProjectRepository,
        slug: &str,
        path_name: &str,
    ) -> djinn_db::Result<djinn_core::models::Project> {
        // `path_name` survives as the fake `github_repo` so the runtime-
        // derived project_dir stays stable across the existing test asserts.
        let path = temp_project_path(path_name);
        std::fs::create_dir_all(&path).unwrap();
        project_repo.create(slug, "test", path_name).await
    }

    /// AC1 (9xih), positive + negative in one run against the real listener:
    ///
    /// * NEGATIVE — a successful completion carrying a resolvable `memory_refs`
    ///   entry must leave `notes.confidence` byte-identical. This is the exact
    ///   scenario that previously applied `TASK_SUCCESS_SIGNAL`, so if the
    ///   `update_confidence` call ever comes back this assertion fails.
    /// * POSITIVE — the completion activity row must still be written, with the
    ///   referenced note id in its payload, so removing the confidence write did
    ///   not cost observability.
    ///
    /// Both assertions read state back out of the database; neither is
    /// satisfied by the listener merely running.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn task_completion_records_activity_without_touching_note_confidence() {
        let harness = make_harness().await;
        spawn_task_outcome_listener(
            harness.db.clone(),
            harness.event_bus.clone(),
            &harness.events,
        );

        let task_repo = TaskRepository::new(harness.db.clone(), harness.event_bus.clone());
        let note_repo = NoteRepository::new(harness.db.clone(), harness.event_bus.clone());
        let project_repo =
            djinn_db::ProjectRepository::new(harness.db.clone(), harness.event_bus.clone());

        let project = create_test_project(
            &project_repo,
            "task-confidence-project",
            "djinn-task-confidence-project",
        )
        .await
        .unwrap();

        let project_path =
            djinn_core::paths::project_dir(&project.github_owner, &project.github_repo);
        std::fs::create_dir_all(&project_path).unwrap();

        let note = note_repo
            .create(
                &project.id,
                "Task Success Note",
                "notes for task outcome confidence",
                "research",
                "[]",
            )
            .await
            .unwrap();
        let _ = project_path;
        // Park the note at a mid prior on purpose. At the new default
        // (CONFIDENCE_CEILING) the removed 0.65 signal would have clamped back
        // to the ceiling and moved nothing, so "confidence did not change"
        // would prove nothing. From 0.5 the removed signal WOULD have moved it,
        // which is what the vacuity guard below asserts.
        note_repo.set_confidence(&note.id, 0.5).await.unwrap();
        let before = note_repo.get(&note.id).await.unwrap().unwrap().confidence;

        let creator = crate::test_helpers::create_test_creator(&harness.db).await;
        let task = task_repo
            .create_in_project_with_provenance(
                &project.id,
                None,
                EffectiveCreatorProvenance {
                    explicit_user_id: Some(&creator.id),
                    source_task_id: None,
                    proposal_id: None,
                },
                "Confidence task",
                "Close applies confidence",
                "",
                "task",
                1,
                "system",
                Some("open"),
                None,
            )
            .await
            .unwrap();

        let memory_refs = serde_json::json!([note.permalink]).to_string();
        task_repo
            .update_memory_refs(&task.id, &memory_refs)
            .await
            .unwrap();

        let closed = task_repo
            .transition(
                &task.id,
                djinn_core::models::TransitionAction::Close,
                "system",
                "system",
                None,
                None,
            )
            .await
            .unwrap();

        wait_for_confidence_signal(&task_repo, &closed.id).await;

        // NEGATIVE: the note's stored confidence is re-read from the database
        // and must be bit-identical to the value before completion.
        let after = note_repo.get(&note.id).await.unwrap().unwrap().confidence;
        assert_eq!(
            after, before,
            "task completion must not mutate note confidence \
             (before {before}, after {after}); TASK_SUCCESS_SIGNAL is removed by 9xih"
        );

        // Guard against the assertion above passing vacuously: prove the
        // removed signal WOULD have moved this exact prior, so an unchanged
        // value is a real behavioural difference and not an arithmetic no-op.
        let would_have_been = djinn_db::repositories::note::bayesian_update(before, 0.65);
        assert!(
            (would_have_been - before).abs() > 1e-9,
            "test is vacuous: the removed 0.65 task-success signal would not have \
             moved a prior of {before} anyway"
        );

        // POSITIVE: completion activity survives, and still names the note.
        let activity = task_repo
            .query_activity(ActivityQuery {
                task_id: Some(task.id),
                event_type: Some(CONFIDENCE_ACTIVITY_TYPE.to_owned()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(activity.len(), 1);
        let payload = serde_json::from_str::<serde_json::Value>(&activity[0].payload).unwrap();
        assert_eq!(payload["reason"], COMPLETED_REASON);
        assert_eq!(payload["missing_notes"], 0);
        assert_eq!(
            payload["updated_notes"]
                .as_array()
                .expect("updated_notes array")
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>(),
            vec![note.id.as_str()],
            "completion activity must still record the referenced note"
        );
        assert_eq!(closed.status, "closed");
    }

    /// AC1 (9xih), call-site guard. `handle_successful_task_completion` is the
    /// production task-outcome writer; this pins that its source text contains
    /// no confidence-mutation call at all, so a future edit cannot reintroduce
    /// one without failing here.
    ///
    /// A source-text assertion is a weak guard on its own, which is why it is
    /// paired with the behavioural test above rather than standing in for it.
    #[test]
    fn task_outcome_listener_source_contains_no_confidence_writer() {
        let source = include_str!("task_confidence.rs");
        // Strip this test's own body so its literals do not match themselves.
        let production = source
            .split_once("#[cfg(test)]")
            .expect("task_confidence.rs has a test module")
            .0;
        // Call syntax, not bare identifiers: the doc comments in this file
        // deliberately NAME the removed writers to explain why they are gone,
        // and that prose must not trip the guard.
        for forbidden in [
            "update_confidence(",
            "set_confidence(",
            "const TASK_SUCCESS_SIGNAL",
        ] {
            assert!(
                !production.contains(forbidden),
                "task-outcome production code must not contain `{forbidden}`: \
                 task/review/merge outcomes are barred from the confidence write boundary"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_memory_refs_is_noop_for_task_completion() {
        let harness = make_harness().await;
        spawn_task_outcome_listener(
            harness.db.clone(),
            harness.event_bus.clone(),
            &harness.events,
        );

        let task_repo = TaskRepository::new(harness.db.clone(), harness.event_bus.clone());
        let project_repo =
            djinn_db::ProjectRepository::new(harness.db.clone(), harness.event_bus.clone());

        let project = create_test_project(
            &project_repo,
            "task-confidence-project-empty",
            "djinn-task-confidence-empty",
        )
        .await
        .unwrap();

        let creator = crate::test_helpers::create_test_creator(&harness.db).await;
        let task = task_repo
            .create_in_project_with_provenance(
                &project.id,
                None,
                EffectiveCreatorProvenance {
                    explicit_user_id: Some(&creator.id),
                    source_task_id: None,
                    proposal_id: None,
                },
                "Empty Memory Task",
                "No refs",
                "",
                "task",
                1,
                "system",
                Some("open"),
                None,
            )
            .await
            .unwrap();

        let closed = task_repo
            .transition(
                &task.id,
                djinn_core::models::TransitionAction::Close,
                "system",
                "system",
                None,
                None,
            )
            .await
            .unwrap();

        wait_for_confidence_signal(&task_repo, &closed.id).await;

        let activity = task_repo
            .query_activity(ActivityQuery {
                task_id: Some(task.id),
                event_type: Some(CONFIDENCE_ACTIVITY_TYPE.to_owned()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(activity.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missing_memory_refs_are_skipped() {
        let harness = make_harness().await;
        spawn_task_outcome_listener(
            harness.db.clone(),
            harness.event_bus.clone(),
            &harness.events,
        );

        let task_repo = TaskRepository::new(harness.db.clone(), harness.event_bus.clone());
        let project_repo =
            djinn_db::ProjectRepository::new(harness.db.clone(), harness.event_bus.clone());

        let project = create_test_project(
            &project_repo,
            "task-confidence-project-missing",
            "djinn-task-confidence-missing",
        )
        .await
        .unwrap();

        let creator = crate::test_helpers::create_test_creator(&harness.db).await;
        let task = task_repo
            .create_in_project_with_provenance(
                &project.id,
                None,
                EffectiveCreatorProvenance {
                    explicit_user_id: Some(&creator.id),
                    source_task_id: None,
                    proposal_id: None,
                },
                "Missing Memory Task",
                "Has missing refs",
                "",
                "task",
                1,
                "system",
                Some("open"),
                None,
            )
            .await
            .unwrap();

        task_repo
            .update_memory_refs(
                &task.id,
                &serde_json::json!(["decisions/missing-note"]).to_string(),
            )
            .await
            .unwrap();

        let closed = task_repo
            .transition(
                &task.id,
                djinn_core::models::TransitionAction::Close,
                "system",
                "system",
                None,
                None,
            )
            .await
            .unwrap();

        wait_for_confidence_signal(&task_repo, &closed.id).await;

        let activity = task_repo
            .query_activity(ActivityQuery {
                task_id: Some(task.id),
                event_type: Some(CONFIDENCE_ACTIVITY_TYPE.to_owned()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(activity.len(), 1);

        let payload = serde_json::from_str::<serde_json::Value>(&activity[0].payload).unwrap();
        assert_eq!(payload["reason"], COMPLETED_REASON);
        assert_eq!(payload["missing_notes"], 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn duplicate_task_completion_events_are_ignored() {
        let harness = make_harness().await;
        spawn_task_outcome_listener(
            harness.db.clone(),
            harness.event_bus.clone(),
            &harness.events,
        );

        let task_repo = TaskRepository::new(harness.db.clone(), harness.event_bus.clone());
        let note_repo = NoteRepository::new(harness.db.clone(), harness.event_bus.clone());
        let project_repo =
            djinn_db::ProjectRepository::new(harness.db.clone(), harness.event_bus.clone());

        let project = create_test_project(
            &project_repo,
            "task-confidence-project-dupe",
            "djinn-task-confidence-dupe",
        )
        .await
        .unwrap();

        let project_path =
            djinn_core::paths::project_dir(&project.github_owner, &project.github_repo);
        std::fs::create_dir_all(&project_path).unwrap();
        let note = note_repo
            .create(
                &project.id,
                "Dupe Note",
                "notes for duplicate task outcome",
                "research",
                "[]",
            )
            .await
            .unwrap();
        let _ = project_path;

        let creator = crate::test_helpers::create_test_creator(&harness.db).await;
        let task = task_repo
            .create_in_project_with_provenance(
                &project.id,
                None,
                EffectiveCreatorProvenance {
                    explicit_user_id: Some(&creator.id),
                    source_task_id: None,
                    proposal_id: None,
                },
                "Duplicate Delivery Task",
                "Emit duplicate events",
                "",
                "task",
                1,
                "system",
                Some("open"),
                None,
            )
            .await
            .unwrap();

        task_repo
            .update_memory_refs(&task.id, &serde_json::json!([note.permalink]).to_string())
            .await
            .unwrap();

        let closed = task_repo
            .transition(
                &task.id,
                djinn_core::models::TransitionAction::Close,
                "system",
                "system",
                None,
                None,
            )
            .await
            .unwrap();

        wait_for_confidence_signal(&task_repo, &closed.id).await;

        let first = note_repo.get(&note.id).await.unwrap().unwrap().confidence;

        let duplicate_event = DjinnEventEnvelope::task_updated(&closed, false);
        let _ = harness.events.send(duplicate_event);

        tokio::time::sleep(Duration::from_millis(200)).await;

        let second = note_repo.get(&note.id).await.unwrap().unwrap().confidence;
        assert!((first - second).abs() < f64::EPSILON);

        let activity = task_repo
            .query_activity(ActivityQuery {
                task_id: Some(task.id),
                event_type: Some(CONFIDENCE_ACTIVITY_TYPE.to_owned()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(activity.len(), 1);
    }

    async fn wait_for_confidence_signal(task_repo: &TaskRepository, task_id: &str) {
        let wait = async {
            loop {
                let activity = task_repo
                    .query_activity(ActivityQuery {
                        task_id: Some(task_id.to_owned()),
                        event_type: Some(CONFIDENCE_ACTIVITY_TYPE.to_owned()),
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                if !activity.is_empty() {
                    return;
                }

                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        };

        timeout(WAIT_TIMEOUT, wait).await.unwrap();
    }
}
