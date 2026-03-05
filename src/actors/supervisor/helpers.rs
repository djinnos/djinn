use super::*;

impl AgentSupervisor {
    pub(super) async fn transition_start(
        &self,
        task: &Task,
        agent_type: AgentType,
    ) -> Result<(), SupervisorError> {
        let action = match (agent_type, task.status.as_str()) {
            (AgentType::Worker, "open") | (AgentType::ConflictResolver, "open") => {
                Some(TransitionAction::Start)
            }
            (AgentType::TaskReviewer, "needs_task_review") => {
                Some(TransitionAction::TaskReviewStart)
            }
            _ => None,
        };

        if let Some(action) = action {
            let repo =
                TaskRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
            repo.transition(&task.id, action, "agent-supervisor", "system", None, None)
                .await
                .map_err(|e| SupervisorError::TaskTransitionFailed {
                    task_id: task.id.clone(),
                    reason: e.to_string(),
                })?;
        }
        Ok(())
    }

    pub(super) async fn load_task(&self, task_id: &str) -> Result<Task, SupervisorError> {
        let repo =
            TaskRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        let task = repo
            .get(task_id)
            .await
            .map_err(|e| SupervisorError::Goose(e.to_string()))?;
        task.ok_or_else(|| SupervisorError::TaskNotFound {
            task_id: task_id.to_owned(),
        })
    }

    pub(super) async fn conflict_context_for_dispatch(&self, task_id: &str) -> Option<MergeConflictMetadata> {
        let repo =
            TaskRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        let activity = repo.list_activity(task_id).await.ok()?;
        let last_status = activity
            .iter()
            .rev()
            .find(|e| e.event_type == "status_changed")?;
        let payload: serde_json::Value = serde_json::from_str(&last_status.payload).ok()?;
        let from_status = payload
            .get("from_status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let to_status = payload
            .get("to_status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if from_status != "in_task_review" || to_status != "open" {
            return None;
        }
        let reason = payload
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        Self::parse_conflict_metadata(reason)
    }

    pub(super) async fn merge_validation_context_for_dispatch(&self, task_id: &str) -> Option<String> {
        let repo =
            TaskRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        let activity = repo.list_activity(task_id).await.ok()?;
        let last_status = activity
            .iter()
            .rev()
            .find(|e| e.event_type == "status_changed")?;
        let payload: serde_json::Value = serde_json::from_str(&last_status.payload).ok()?;
        let from_status = payload
            .get("from_status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let to_status = payload
            .get("to_status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if from_status != "in_task_review" || to_status != "open" {
            return None;
        }
        let reason = payload
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let metadata = Self::parse_merge_validation_metadata(reason)?;
        Some(metadata.as_prompt_context())
    }

    pub(super) fn parse_conflict_metadata(reason: &str) -> Option<MergeConflictMetadata> {
        let raw = reason.strip_prefix(MERGE_CONFLICT_PREFIX)?;
        serde_json::from_str(raw).ok()
    }

    pub(super) fn parse_merge_validation_metadata(reason: &str) -> Option<MergeValidationFailureMetadata> {
        let raw = reason.strip_prefix(MERGE_VALIDATION_PREFIX)?;
        serde_json::from_str(raw).ok()
    }

    pub(super) async fn find_paused_session_record(&self, task_id: &str) -> Option<SessionRecord> {
        let repo =
            SessionRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        repo.paused_for_task(task_id).await.ok().flatten()
    }

    pub(super) async fn resume_context_for_task(&self, task_id: &str) -> String {
        let repo =
            TaskRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        let activity = repo.list_activity(task_id).await.ok().unwrap_or_default();

        // Check for most recent task reviewer comment (reviewer rejection feedback).
        for entry in activity.iter().rev() {
            if entry.event_type == "comment" && entry.actor_role == "task_reviewer" {
                if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&entry.payload)
                    && let Some(body) = payload.get("body").and_then(|v| v.as_str())
                {
                    return format!(
                        "Your previous work was reviewed and returned with this feedback:\n\n{body}\n\nAddress this feedback, make the necessary changes, then emit:\nWORKER_RESULT: DONE"
                    );
                }
            }
        }

        // Check for merge conflict context.
        if let Some(context) = self.merge_validation_context_for_dispatch(task_id).await {
            return context;
        }

        // Check for merge conflict info in activity.
        for entry in activity.iter().rev() {
            if entry.event_type == "merge_conflict" {
                if let Ok(meta) = serde_json::from_str::<MergeConflictMetadata>(&entry.payload) {
                    let files = meta
                        .conflicting_files
                        .iter()
                        .map(|f| format!("- {f}"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    return format!(
                        "A merge conflict was detected when merging your branch into `{}`. Resolve the conflicts in these files:\n\n{files}\n\nAfter resolving, commit and emit:\nWORKER_RESULT: DONE",
                        meta.merge_target
                    );
                }
            }
        }

        // Default fallback.
        "Your previous submission needs revision. Review your work, address any issues, then emit:\nWORKER_RESULT: DONE".to_string()
    }
}
