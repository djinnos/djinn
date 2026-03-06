use super::*;

impl AgentSupervisor {
    pub(super) fn decrement_capacity_for_model(&mut self, model_id: Option<&str>) {
        if let Some(model_id) = model_id
            && let Some(model_capacity) = self.capacity.get_mut(model_id)
            && model_capacity.active > 0
        {
            model_capacity.active -= 1;
        }
    }

    pub(super) fn running_sessions_snapshot(&self) -> Vec<RunningSessionInfo> {
        let mut sessions: Vec<RunningSessionInfo> = self
            .sessions
            .iter()
            .map(|(task_id, handle)| self.session_snapshot(task_id, handle))
            .collect();
        sessions.extend(self.lifecycle_handles.iter().map(|(task_id, handle)| {
            RunningSessionInfo {
                task_id: task_id.clone(),
                model_id: handle.model_id.clone(),
                session_id: "lifecycle".to_string(),
                duration_seconds: handle.started_at.elapsed().as_secs(),
                worktree_path: None,
            }
        }));
        sessions.sort_by(|a, b| a.task_id.cmp(&b.task_id));
        sessions
    }

    pub(super) fn session_snapshot(
        &self,
        task_id: &str,
        handle: &GooseSessionHandle,
    ) -> RunningSessionInfo {
        let model_id = self
            .session_models
            .get(task_id)
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());
        RunningSessionInfo {
            task_id: task_id.to_string(),
            model_id,
            session_id: handle.session_id.clone(),
            duration_seconds: handle.started_at.elapsed().as_secs(),
            worktree_path: handle
                .worktree_path
                .as_ref()
                .map(|path| path.display().to_string()),
        }
    }

    pub(super) fn collect_pending_session(
        &mut self,
        task_id: String,
        mut handle: GooseSessionHandle,
    ) -> PendingInterrupt {
        handle.cancel.cancel();
        self.interrupted_sessions.insert(task_id.clone());
        PendingInterrupt {
            model_id: self.session_models.remove(&task_id),
            agent_type: self
                .session_agent_types
                .remove(&task_id)
                .unwrap_or(AgentType::Worker),
            session_record_id: self.task_session_records.remove(&task_id),
            goose_session_id: handle.session_id,
            join: handle.join,
            worktree_path: handle.worktree_path.take(),
            task_id,
        }
    }

    pub(super) async fn drain_pending_sessions(
        &mut self,
        pending: &mut Vec<PendingInterrupt>,
        reason: &str,
    ) {
        let deadline = Instant::now() + Duration::from_secs(30);
        for item in pending.iter_mut() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                item.join.abort();
                continue;
            }

            if tokio::time::timeout(remaining, &mut item.join)
                .await
                .is_err()
            {
                tracing::warn!(task_id = %item.task_id, "session join timed out during shutdown; aborting");
                item.join.abort();
            }
        }

        for item in pending.drain(..) {
            self.decrement_capacity_for_model(item.model_id.as_deref());
            self.in_flight.remove(&item.task_id);
            if let Some(worktree_path) = item.worktree_path.as_ref() {
                self.commit_wip_if_needed(&item.task_id, worktree_path)
                    .await;
                self.cleanup_worktree(&item.task_id, worktree_path).await;
            }
            let (tokens_in, tokens_out) = self.tokens_for_session(&item.goose_session_id).await;
            self.update_session_record(
                item.session_record_id.as_deref(),
                SessionStatus::Interrupted,
                tokens_in,
                tokens_out,
            )
            .await;
            self.transition_interrupted(&item.task_id, item.agent_type, reason)
                .await;
        }
    }

    pub(super) async fn interrupt_all_sessions(&mut self, reason: &str) {
        for handle in self.lifecycle_handles.values() {
            handle.kill.cancel();
        }

        let mut pending: Vec<PendingInterrupt> = Vec::new();
        for (task_id, handle) in std::mem::take(&mut self.sessions) {
            self.session_projects.remove(&task_id);
            pending.push(self.collect_pending_session(task_id, handle));
        }
        self.drain_pending_sessions(&mut pending, reason).await;

        let mut lifecycle_pending = std::mem::take(&mut self.lifecycle_handles);
        for (task_id, mut handle) in lifecycle_pending.drain() {
            let _ = tokio::time::timeout(Duration::from_secs(30), &mut handle.join).await;
            self.decrement_capacity_for_model(Some(&handle.model_id));
            self.in_flight.remove(&task_id);
        }
    }

    pub(super) async fn interrupt_project_sessions(&mut self, project_id: &str, reason: &str) {
        for (_task_id, handle) in self
            .lifecycle_handles
            .iter()
            .filter(|(_, h)| h.project_id == project_id)
        {
            handle.kill.cancel();
        }

        let matching_task_ids: Vec<String> = self
            .session_projects
            .iter()
            .filter(|(_, pid)| *pid == project_id)
            .map(|(tid, _)| tid.clone())
            .collect();

        let mut pending: Vec<PendingInterrupt> = Vec::new();
        for task_id in matching_task_ids {
            self.session_projects.remove(&task_id);
            if let Some(handle) = self.sessions.remove(&task_id) {
                pending.push(self.collect_pending_session(task_id, handle));
            }
        }
        self.drain_pending_sessions(&mut pending, reason).await;

        let lifecycle_task_ids: Vec<String> = self
            .lifecycle_handles
            .iter()
            .filter(|(_, h)| h.project_id == project_id)
            .map(|(task_id, _)| task_id.clone())
            .collect();
        for task_id in lifecycle_task_ids {
            if let Some(mut handle) = self.lifecycle_handles.remove(&task_id) {
                let _ = tokio::time::timeout(Duration::from_secs(30), &mut handle.join).await;
                self.decrement_capacity_for_model(Some(&handle.model_id));
                self.in_flight.remove(&task_id);
            }
        }
    }
}
