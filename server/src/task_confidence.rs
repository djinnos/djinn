use std::collections::HashSet;

use djinn_core::models::Task;
use djinn_db::{ActivityQuery, NoteRepository, TaskRepository};
use serde::{Deserialize, Serialize};

const TASK_SUCCESS_SIGNAL: f64 = 0.65;
const COMPLETED_STATUS: &str = "closed";
const COMPLETED_REASON: &str = "completed";
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
    updated_notes: Vec<String>,
    missing_notes: usize,
    from_sync: bool,
}

pub(crate) fn spawn_task_outcome_listener(state: crate::server::AppState) {
    let mut rx = state.events().subscribe();
    let task_repo = TaskRepository::new(state.db().clone(), state.event_bus());
    let note_repo = NoteRepository::new(state.db().clone(), state.event_bus());

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

    let memory_refs = parse_memory_refs(&payload.task.memory_refs);
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
    let mut updated_notes = Vec::new();
    let mut missing_refs = 0usize;

    for permalink in memory_refs {
        if !seen.insert(permalink.clone()) {
            continue;
        }

        match note_repo
            .get_by_permalink(&payload.task.project_id, &permalink)
            .await
        {
            Ok(Some(note)) => {
                if let Err(error) = note_repo
                    .update_confidence(&note.id, TASK_SUCCESS_SIGNAL)
                    .await
                {
                    tracing::warn!(
                        error = %error,
                        task_id = %payload.task.id,
                        note_id = %note.id,
                        permalink,
                        "failed to apply task success confidence signal"
                    );
                    continue;
                }
                updated_notes.push(note.id);
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
        updated_notes,
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

fn parse_memory_refs(memory_refs: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(memory_refs).unwrap_or_else(|_| {
        tracing::warn!(memory_refs = %memory_refs, "invalid memory_refs JSON for task completion signal");
        Vec::new()
    })
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
    use std::time::Duration;

    use djinn_core::events::DjinnEventEnvelope;
    use djinn_db::repositories::task::ActivityQuery;
    use djinn_db::{NoteRepository, TaskRepository};
    use tokio::time::timeout;

    use crate::test_helpers;

    const WAIT_TIMEOUT: Duration = Duration::from_secs(3);

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn applies_task_success_signal_to_referenced_notes() {
        let state = test_helpers::test_app_state_in_memory().await;
        spawn_task_outcome_listener(state.clone());

        let task_repo = TaskRepository::new(state.db().clone(), state.event_bus());
        let note_repo = NoteRepository::new(state.db().clone(), state.event_bus());
        let project_repo = djinn_db::ProjectRepository::new(state.db().clone(), state.event_bus());

        let project = project_repo
            .create(
                "task-confidence-project",
                "/tmp/djinn-task-confidence-project",
            )
            .await
            .unwrap();

        std::fs::create_dir_all(&project.path).unwrap();

        let note = note_repo
            .create(
                &project.id,
                std::path::Path::new(&project.path),
                "Task Success Note",
                "notes for task outcome confidence",
                "research",
                "[]",
            )
            .await
            .unwrap();
        let before = note_repo.get(&note.id).await.unwrap().unwrap().confidence;

        let task = task_repo
            .create_in_project(
                &project.id,
                None,
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

        let updated = note_repo.get(&note.id).await.unwrap().unwrap();
        assert!(
            updated.confidence < before,
            "expected confidence to update when task completes"
        );
        assert!(
            (updated.confidence
                - djinn_db::repositories::note::bayesian_update(before, TASK_SUCCESS_SIGNAL))
            .abs()
                < 1e-9
        );

        let activity = task_repo
            .query_activity(ActivityQuery {
                task_id: Some(task.id),
                event_type: Some(CONFIDENCE_ACTIVITY_TYPE.to_owned()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(activity.len(), 1);
        assert_eq!(closed.status, "closed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_memory_refs_is_noop_for_task_completion() {
        let state = test_helpers::test_app_state_in_memory().await;
        spawn_task_outcome_listener(state.clone());

        let task_repo = TaskRepository::new(state.db().clone(), state.event_bus());
        let project_repo = djinn_db::ProjectRepository::new(state.db().clone(), state.event_bus());

        let project = project_repo
            .create(
                "task-confidence-project-empty",
                "/tmp/djinn-task-confidence-empty",
            )
            .await
            .unwrap();

        let task = task_repo
            .create_in_project(
                &project.id,
                None,
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
        let state = test_helpers::test_app_state_in_memory().await;
        spawn_task_outcome_listener(state.clone());

        let task_repo = TaskRepository::new(state.db().clone(), state.event_bus());
        let project_repo = djinn_db::ProjectRepository::new(state.db().clone(), state.event_bus());

        let project = project_repo
            .create(
                "task-confidence-project-missing",
                "/tmp/djinn-task-confidence-missing",
            )
            .await
            .unwrap();

        let task = task_repo
            .create_in_project(
                &project.id,
                None,
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
        let state = test_helpers::test_app_state_in_memory().await;
        spawn_task_outcome_listener(state.clone());

        let task_repo = TaskRepository::new(state.db().clone(), state.event_bus());
        let note_repo = NoteRepository::new(state.db().clone(), state.event_bus());
        let project_repo = djinn_db::ProjectRepository::new(state.db().clone(), state.event_bus());

        let project = project_repo
            .create(
                "task-confidence-project-dupe",
                "/tmp/djinn-task-confidence-dupe",
            )
            .await
            .unwrap();

        let note = note_repo
            .create(
                &project.id,
                std::path::Path::new(&project.path),
                "Dupe Note",
                "notes for duplicate task outcome",
                "research",
                "[]",
            )
            .await
            .unwrap();

        let task = task_repo
            .create_in_project(
                &project.id,
                None,
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
        let _ = state.events().send(duplicate_event);

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
