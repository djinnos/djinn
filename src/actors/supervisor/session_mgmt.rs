use super::*;

impl AgentSupervisor {
    pub(super) fn remove_session(&mut self, task_id: &str) -> SessionClosure {
        let removed = self.sessions.remove(task_id);
        let goose_session_id = removed
            .as_ref()
            .map(|h| h.session_id.clone())
            .unwrap_or_else(|| format!("unknown-session-{task_id}"));
        self.decrement_capacity(task_id);
        self.session_projects.remove(task_id);
        SessionClosure {
            model_id: self.session_models.remove(task_id),
            agent_type: self
                .session_agent_types
                .remove(task_id)
                .unwrap_or(AgentType::Worker),
            goose_session_id,
            record_id: self.task_session_records.remove(task_id),
            worktree_path: removed.and_then(|h| h.worktree_path),
        }
    }

    pub(super) fn decrement_capacity(&mut self, task_id: &str) {
        if let Some(model_id) = self.session_models.get(task_id)
            && let Some(model_capacity) = self.capacity.get_mut(model_id)
            && model_capacity.active > 0
        {
            model_capacity.active -= 1;
        }
    }

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
        sessions.sort_by(|a, b| a.task_id.cmp(&b.task_id));
        sessions
    }

    pub(super) fn session_snapshot(&self, task_id: &str, handle: &GooseSessionHandle) -> RunningSessionInfo {
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

    pub(super) async fn drain_pending_sessions(&mut self, pending: &mut Vec<PendingInterrupt>, reason: &str) {
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
        let mut pending: Vec<PendingInterrupt> = Vec::new();
        for (task_id, handle) in std::mem::take(&mut self.sessions) {
            self.session_projects.remove(&task_id);
            pending.push(self.collect_pending_session(task_id, handle));
        }
        self.drain_pending_sessions(&mut pending, reason).await;
    }

    pub(super) async fn interrupt_project_sessions(&mut self, project_id: &str, reason: &str) {
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
    }

    pub(super) fn missing_required_marker(agent_type: AgentType, output: &ParsedAgentOutput) -> bool {
        match agent_type {
            AgentType::Worker | AgentType::ConflictResolver => output.worker_signal.is_none(),
            AgentType::TaskReviewer => output.reviewer_verdict.is_none(),
            AgentType::EpicReviewer => output.epic_verdict.is_none(),
        }
    }

    pub(super) fn missing_marker_nudge(
        agent_type: AgentType,
        output: &ParsedAgentOutput,
    ) -> Option<&'static str> {
        if !Self::missing_required_marker(agent_type, output) {
            return None;
        }

        match agent_type {
            AgentType::Worker | AgentType::ConflictResolver => Some(
                "Emit exactly one final marker now: WORKER_RESULT: DONE.",
            ),
            AgentType::TaskReviewer => Some(
                "Emit exactly one final marker now: REVIEW_RESULT: VERIFIED | REOPEN. If REOPEN, also emit FEEDBACK: <what is missing>.",
            ),
            AgentType::EpicReviewer => Some(
                "Emit exactly one final marker now: EPIC_REVIEW_RESULT: CLEAN | ISSUES_FOUND. If ISSUES_FOUND, include concise actionable findings and create follow-up tasks in this epic before finishing.",
            ),
        }
    }

    pub(super) fn agent_type_for_task(&self, task: &Task, has_conflict_context: bool) -> AgentType {
        match task.status.as_str() {
            "needs_task_review" | "in_task_review" => AgentType::TaskReviewer,
            "open" if has_conflict_context => AgentType::ConflictResolver,
            _ => AgentType::Worker,
        }
    }
}
