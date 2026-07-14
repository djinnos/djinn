use crate::models::Agent;
use crate::models::Credential;
use crate::models::CustomProvider;
use crate::models::DispatchPause;
use crate::models::DispatchPauseScope;
use crate::models::Epic;
use crate::models::GitSettings;
use crate::models::Project;
use crate::models::Proposal;
use crate::models::ProposalDebateTrail;
use crate::models::ProposalFeedback;
use crate::models::Task;
use serde::de::DeserializeOwned;

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DjinnEventEnvelope {
    pub entity_type: &'static str,
    pub action: &'static str,
    pub payload: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(skip)]
    pub from_sync: bool,
}

impl DjinnEventEnvelope {
    pub fn project_created(project: &Project) -> Self {
        Self {
            entity_type: "project",
            action: "created",
            payload: serde_json::to_value(project)
                .expect("serializing DjinnEventEnvelope payload to Value should not fail"),
            id: None,
            project_id: None,
            from_sync: false,
        }
    }

    pub fn project_updated(project: &Project) -> Self {
        Self {
            entity_type: "project",
            action: "updated",
            payload: serde_json::to_value(project)
                .expect("serializing DjinnEventEnvelope payload to Value should not fail"),
            id: None,
            project_id: None,
            from_sync: false,
        }
    }

    pub fn project_deleted(id: &str) -> Self {
        Self {
            entity_type: "project",
            action: "deleted",
            payload: serde_json::to_value(serde_json::json!({ "id": id }))
                .expect("serializing DjinnEventEnvelope payload to Value should not fail"),
            id: Some(id.to_string()),
            project_id: None,
            from_sync: false,
        }
    }

    pub fn project_config_updated(project_id: &str, config: &impl serde::Serialize) -> Self {
        Self {
            entity_type: "project_config",
            action: "updated",
            payload: serde_json::to_value(
                serde_json::json!({ "project_id": project_id, "config": config }),
            )
            .expect("serializing DjinnEventEnvelope payload to Value should not fail"),
            id: None,
            project_id: Some(project_id.to_string()),
            from_sync: false,
        }
    }

    /// Emitted when dispatch pause state changes for a global, project, or user
    /// scope. Project-scoped changes carry `project_id` on the envelope so SSE
    /// subscribers can filter without decoding the payload.
    pub fn dispatch_pause_changed(
        scope: DispatchPauseScope,
        target_id: Option<&str>,
        current: Option<&DispatchPause>,
        previous: Option<&DispatchPause>,
        resumed_by: Option<&str>,
        resumed_at: Option<&str>,
    ) -> Self {
        let (paused_by, paused_at, reason) = current
            .map(|pause| {
                (
                    Some(pause.paused_by.as_str()),
                    Some(pause.paused_at.as_str()),
                    Some(pause.reason.as_str()),
                )
            })
            .unwrap_or((None, None, None));
        let actor = paused_by.or(resumed_by);
        let changed_at = paused_at.or(resumed_at);

        Self {
            entity_type: "dispatch_pause",
            action: "changed",
            payload: serde_json::to_value(serde_json::json!({
                "scope": scope,
                "target_id": target_id,
                "current": current,
                "previous": previous,
                "paused_by": paused_by,
                "resumed_by": resumed_by,
                "actor": actor,
                "changed_at": changed_at,
                "reason": reason,
            }))
            .expect("serializing DjinnEventEnvelope payload to Value should not fail"),
            id: target_id.map(str::to_owned),
            project_id: if scope == DispatchPauseScope::Project {
                target_id.map(str::to_owned)
            } else {
                None
            },
            from_sync: false,
        }
    }

    pub fn epic_created(epic: &Epic) -> Self {
        Self {
            entity_type: "epic",
            action: "created",
            payload: serde_json::to_value(epic).unwrap(),
            id: None,
            project_id: None,
            from_sync: false,
        }
    }
    pub fn epic_updated(epic: &Epic) -> Self {
        Self {
            entity_type: "epic",
            action: "updated",
            payload: serde_json::to_value(epic).unwrap(),
            id: None,
            project_id: None,
            from_sync: false,
        }
    }
    pub fn epic_deleted(id: &str) -> Self {
        Self {
            entity_type: "epic",
            action: "deleted",
            payload: serde_json::to_value(serde_json::json!({"id": id})).unwrap(),
            id: Some(id.to_string()),
            project_id: None,
            from_sync: false,
        }
    }
    pub fn proposal_created(proposal: &Proposal) -> Self {
        Self {
            entity_type: "proposal",
            action: "created",
            payload: serde_json::to_value(proposal).unwrap(),
            id: None,
            project_id: None,
            from_sync: false,
        }
    }
    pub fn proposal_updated(proposal: &Proposal) -> Self {
        Self {
            entity_type: "proposal",
            action: "updated",
            payload: serde_json::to_value(proposal).unwrap(),
            id: None,
            project_id: None,
            from_sync: false,
        }
    }
    pub fn proposal_deleted(id: &str) -> Self {
        Self {
            entity_type: "proposal",
            action: "deleted",
            payload: serde_json::to_value(serde_json::json!({"id": id})).unwrap(),
            id: Some(id.to_string()),
            project_id: None,
            from_sync: false,
        }
    }
    /// A feedback entry (discussion or suggestion) was added to a proposal.
    /// Carries the parent `proposal_id` so the UI can target the right detail
    /// view without a refetch.
    pub fn proposal_feedback_created(proposal_id: &str, feedback: &ProposalFeedback) -> Self {
        Self {
            entity_type: "proposal_feedback",
            action: "created",
            payload: serde_json::to_value(
                serde_json::json!({"proposal_id": proposal_id, "feedback": feedback}),
            )
            .unwrap(),
            id: None,
            project_id: None,
            from_sync: false,
        }
    }

    /// A debate-trail entry was appended to a proposal.
    pub fn proposal_debate_trail_created(proposal_id: &str, entry: &ProposalDebateTrail) -> Self {
        Self {
            entity_type: "proposal_debate_trail",
            action: "created",
            payload: serde_json::to_value(
                serde_json::json!({"proposal_id": proposal_id, "entry": entry}),
            )
            .unwrap(),
            id: None,
            project_id: None,
            from_sync: false,
        }
    }

    /// A debate-trail entry was updated (resolved or reopened).
    pub fn proposal_debate_trail_updated(proposal_id: &str, entry: &ProposalDebateTrail) -> Self {
        Self {
            entity_type: "proposal_debate_trail",
            action: "updated",
            payload: serde_json::to_value(
                serde_json::json!({"proposal_id": proposal_id, "entry": entry}),
            )
            .unwrap(),
            id: None,
            project_id: None,
            from_sync: false,
        }
    }

    pub fn task_created(task: &Task, from_sync: bool) -> Self {
        Self {
            entity_type: "task",
            action: "created",
            payload: serde_json::to_value(
                serde_json::json!({"task": task, "from_sync": from_sync}),
            )
            .unwrap(),
            id: None,
            project_id: None,
            from_sync,
        }
    }
    pub fn task_updated(task: &Task, from_sync: bool) -> Self {
        Self {
            entity_type: "task",
            action: "updated",
            payload: serde_json::to_value(
                serde_json::json!({"task": task, "from_sync": from_sync}),
            )
            .unwrap(),
            id: None,
            project_id: None,
            from_sync,
        }
    }
    pub fn task_deleted(id: &str) -> Self {
        Self {
            entity_type: "task",
            action: "deleted",
            payload: serde_json::to_value(serde_json::json!({"id": id})).unwrap(),
            id: Some(id.to_string()),
            project_id: None,
            from_sync: false,
        }
    }
    pub fn git_settings_updated(project_id: &str, settings: &GitSettings) -> Self {
        Self {
            entity_type: "git_settings",
            action: "updated",
            payload: serde_json::to_value(
                serde_json::json!({"project_id": project_id, "settings": settings}),
            )
            .unwrap(),
            id: None,
            project_id: Some(project_id.to_string()),
            from_sync: false,
        }
    }
    pub fn custom_provider_upserted(provider: &CustomProvider) -> Self {
        Self {
            entity_type: "custom_provider",
            action: "updated",
            payload: serde_json::to_value(provider).unwrap(),
            id: None,
            project_id: None,
            from_sync: false,
        }
    }
    pub fn custom_provider_deleted(id: &str) -> Self {
        Self {
            entity_type: "custom_provider",
            action: "deleted",
            payload: serde_json::to_value(serde_json::json!({"id": id})).unwrap(),
            id: Some(id.to_string()),
            project_id: None,
            from_sync: false,
        }
    }
    pub fn agent_created(role: &Agent) -> Self {
        Self {
            entity_type: "agent",
            action: "created",
            payload: serde_json::to_value(role).unwrap(),
            id: None,
            project_id: Some(role.project_id.clone()),
            from_sync: false,
        }
    }
    pub fn agent_updated(role: &Agent) -> Self {
        Self {
            entity_type: "agent",
            action: "updated",
            payload: serde_json::to_value(role).unwrap(),
            id: None,
            project_id: Some(role.project_id.clone()),
            from_sync: false,
        }
    }
    pub fn agent_deleted(id: &str, project_id: &str) -> Self {
        Self {
            entity_type: "agent",
            action: "deleted",
            payload: serde_json::to_value(serde_json::json!({"id": id})).unwrap(),
            id: Some(id.to_string()),
            project_id: Some(project_id.to_string()),
            from_sync: false,
        }
    }
    pub fn credential_created(credential: &Credential) -> Self {
        Self {
            entity_type: "credential",
            action: "created",
            payload: serde_json::to_value(credential).unwrap(),
            id: None,
            project_id: None,
            from_sync: false,
        }
    }
    pub fn credential_updated(credential: &Credential) -> Self {
        Self {
            entity_type: "credential",
            action: "updated",
            payload: serde_json::to_value(credential).unwrap(),
            id: None,
            project_id: None,
            from_sync: false,
        }
    }
    pub fn credential_deleted(id: &str) -> Self {
        Self {
            entity_type: "credential",
            action: "deleted",
            payload: serde_json::to_value(serde_json::json!({"id": id})).unwrap(),
            id: Some(id.to_string()),
            project_id: None,
            from_sync: false,
        }
    }
    /// A stored credential was rejected by the provider as revoked/invalid (a
    /// 401 during a task/chat run) and marked revoked. The UI surfaces this as a
    /// "reconnect <provider>" prompt; `user_id` scopes it to the owner (`None`
    /// for an org-shared credential). The persisted `revoked_at`/`revoked_reason`
    /// on the row is the F5-safe source of truth — this event is just the live
    /// nudge.
    pub fn credential_revoked(user_id: Option<&str>, provider_id: &str, reason: &str) -> Self {
        Self {
            entity_type: "credential",
            action: "revoked",
            payload: serde_json::to_value(serde_json::json!({
                "user_id": user_id,
                "provider_id": provider_id,
                "reason": reason,
            }))
            .unwrap(),
            id: None,
            project_id: None,
            from_sync: false,
        }
    }
    pub fn session_dispatched(
        project_id: &str,
        task_id: &str,
        model_id: &str,
        agent_type: &str,
    ) -> Self {
        Self { entity_type: "session", action: "dispatched", payload: serde_json::to_value(serde_json::json!({"project_id": project_id, "task_id": task_id, "model_id": model_id, "agent_type": agent_type})).unwrap(), id: None, project_id: Some(project_id.to_string()), from_sync: false }
    }
    #[allow(clippy::too_many_arguments)]
    pub fn session_token_update(
        session_id: &str,
        task_id: &str,
        tokens_in: i64,
        tokens_out: i64,
        context_window: i64,
        usage_pct: f64,
        cache_read: i64,
        cache_write: i64,
        reasoning_out: i64,
    ) -> Self {
        Self { entity_type: "session", action: "token_update", payload: serde_json::to_value(serde_json::json!({"session_id": session_id, "task_id": task_id, "tokens_in": tokens_in, "tokens_out": tokens_out, "context_window": context_window, "usage_pct": usage_pct, "cache_read": cache_read, "cache_write": cache_write, "reasoning_out": reasoning_out})).unwrap(), id: None, project_id: None, from_sync: false }
    }
    pub fn session_message(
        session_id: &str,
        task_id: &str,
        agent_type: &str,
        message: &serde_json::Value,
    ) -> Self {
        Self { entity_type: "session", action: "message", payload: serde_json::to_value(serde_json::json!({"session_id": session_id, "task_id": task_id, "agent_type": agent_type, "message": message})).unwrap(), id: None, project_id: None, from_sync: false }
    }
    pub fn sync_completed(
        channel: &str,
        direction: &str,
        count: usize,
        error: Option<&str>,
    ) -> Self {
        Self { entity_type: "sync", action: "completed", payload: serde_json::to_value(serde_json::json!({"channel": channel, "direction": direction, "count": count, "error": error})).unwrap(), id: None, project_id: None, from_sync: false }
    }
    pub fn project_health_changed(project_id: &str, healthy: bool, error: Option<&str>) -> Self {
        Self {
            entity_type: "project",
            action: if healthy { "health_ok" } else { "health_error" },
            payload: serde_json::to_value(
                serde_json::json!({"project_id": project_id, "healthy": healthy, "error": error}),
            )
            .unwrap(),
            id: None,
            project_id: Some(project_id.to_string()),
            from_sync: false,
        }
    }
    pub fn task_lifecycle_step(task_id: &str, step: &str, detail: &serde_json::Value) -> Self {
        Self {
            entity_type: "lifecycle",
            action: "step",
            payload: serde_json::to_value(
                serde_json::json!({"task_id": task_id, "step": step, "detail": detail}),
            )
            .unwrap(),
            id: None,
            project_id: None,
            from_sync: false,
        }
    }
    pub fn activity_logged(
        task_id: Option<&str>,
        action: &str,
        actor: &str,
        actor_role: &str,
        payload: &serde_json::Value,
    ) -> Self {
        Self { entity_type: "activity", action: "logged", payload: serde_json::to_value(serde_json::json!({"task_id": task_id, "action": action, "actor": actor, "actor_role": actor_role, "payload": payload})).unwrap(), id: None, project_id: None, from_sync: false }
    }

    /// Emitted by OAuth flows so the Electron desktop app can open the
    /// authorization URL in the user's default browser. Necessary because the
    /// server runs inside a Docker container and can't `xdg-open` anything
    /// itself.
    pub fn oauth_open_browser(provider: &str, url: &str) -> Self {
        Self {
            entity_type: "oauth",
            action: "open_browser",
            payload: serde_json::to_value(serde_json::json!({
                "provider": provider,
                "url": url,
            }))
            .expect("serializing DjinnEventEnvelope payload to Value should not fail"),
            id: None,
            project_id: None,
            from_sync: false,
        }
    }

    /// Emitted when a provider kicks off an OAuth **device-code** flow. The
    /// UI displays `user_code` + `verification_uri` (or the convenience
    /// `verification_uri_complete` with the code pre-filled) and waits for a
    /// subsequent `credential.updated` event to confirm sign-in. Replaces the
    /// browser-redirect flow for providers whose first-party OAuth clients
    /// don't accept arbitrary redirect URIs (e.g. ChatGPT / Codex).
    pub fn oauth_device_code(
        provider: &str,
        verification_uri: &str,
        verification_uri_complete: &str,
        user_code: &str,
        interval: i64,
        expires_in: i64,
    ) -> Self {
        Self {
            entity_type: "oauth",
            action: "device_code",
            payload: serde_json::to_value(serde_json::json!({
                "provider": provider,
                "verification_uri": verification_uri,
                "verification_uri_complete": verification_uri_complete,
                "user_code": user_code,
                "interval": interval,
                "expires_in": expires_in,
            }))
            .expect("serializing DjinnEventEnvelope payload to Value should not fail"),
            id: None,
            project_id: None,
            from_sync: false,
        }
    }

    pub fn entity_type(&self) -> &'static str {
        self.entity_type
    }
    pub fn action(&self) -> &'static str {
        self.action
    }
    pub fn from_sync(&self) -> bool {
        self.from_sync
    }
    pub fn payload(&self) -> &serde_json::Value {
        &self.payload
    }
    pub fn parse_payload<T: DeserializeOwned>(&self) -> Option<T> {
        serde_json::from_value(self.payload.clone()).ok()
    }
}

/// A type-erased event sink. Wraps a callback so that djinn-db repositories
/// can emit events without depending on tokio broadcast.
///
/// Cheap to clone — the inner callback is reference-counted.
#[derive(Clone)]
pub struct EventBus(std::sync::Arc<dyn Fn(DjinnEventEnvelope) + Send + Sync>);

impl EventBus {
    pub fn new(f: impl Fn(DjinnEventEnvelope) + Send + Sync + 'static) -> Self {
        EventBus(std::sync::Arc::new(f))
    }

    pub fn send(&self, event: DjinnEventEnvelope) {
        (self.0)(event);
    }

    pub fn noop() -> Self {
        EventBus(std::sync::Arc::new(|_| {}))
    }

    /// Build an `EventBus` whose `send` delegates to the caller-supplied
    /// closure on a freshly-spawned tokio task.
    ///
    /// Used by `djinn-agent-worker` to bridge worker-emitted envelopes onto
    /// its host-bound `RpcServices::emit_djinn_event` call without blocking
    /// the calling stage on the RPC round-trip (which would deadlock the
    /// reply-loop's streaming path).
    ///
    /// The closure must be `Fn + Send + Sync + 'static` because the
    /// underlying `Arc<dyn Fn>` is shared across every clone of the bus and
    /// invoked from arbitrary contexts (repository writes, stage drivers,
    /// background actor loops).
    pub fn spawning<F, Fut>(f: F) -> Self
    where
        F: Fn(DjinnEventEnvelope) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let f = std::sync::Arc::new(f);
        EventBus(std::sync::Arc::new(move |event| {
            let f = f.clone();
            // Spawn-and-forget: the caller does not care about the result;
            // any RPC-level errors are logged by the closure body.
            tokio::spawn(async move {
                f(event).await;
            });
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::DjinnEventEnvelope;
    use crate::models::{DispatchPause, DispatchPauseScope, Project, Setting, Task};
    use serde_json::json;

    fn task_with_merge_commit_sha(merge_commit_sha: Option<&str>) -> Task {
        Task {
            id: "task-1".into(),
            project_id: "p1".into(),
            short_id: "T-1".into(),
            epic_id: None,
            title: "Title".into(),
            description: "".into(),
            design: "".into(),
            issue_type: "task".into(),
            status: "open".into(),
            priority: 1,
            owner: "".into(),
            labels: "[]".into(),
            acceptance_criteria: "[]".into(),
            reopen_count: 0,
            continuation_count: 0,
            created_at: "2025-01-01T00:00:00Z".into(),
            updated_at: "2025-01-01T00:00:00Z".into(),
            closed_at: None,
            close_reason: None,
            merge_commit_sha: merge_commit_sha.map(str::to_owned),
            pr_url: None,
            merge_conflict_metadata: None,
            memory_refs: "[]".into(),
            agent_type: None,
            created_by_user_id: None,
            ci_status: "unknown".into(),
            ci_head_sha: None,
            ci_pr_number: None,
            ci_blocking_required_check_names: "[]".into(),
            ci_failure_fingerprint: None,
            ci_first_seen_at: None,
            ci_last_seen_at: None,
            ci_same_signature_count: 0,
            ci_last_remediation_base_sha: None,
            ci_mirror_head_sha: None,
            ci_github_head_sha: None,
            ci_heads_diverged: None,
            ci_head_observation_error: None,
            ci_mq_state: None,
            ci_mq_run_id: None,
            ci_mq_head_sha: None,
            ci_mq_failed_check_names: None,
            ci_mq_failure_fingerprint: None,
            ci_mq_same_signature_count: None,
            ci_mq_first_seen_at: None,
            ci_mq_last_seen_at: None,
            unresolved_blocker_count: 0,
            total_reopen_count: 0,
            intervention_count: 0,
            last_intervention_at: None,
        }
    }

    #[test]
    fn envelope_task_created_round_trip_and_parse_payload() {
        let task = task_with_merge_commit_sha(None);

        let envelope = DjinnEventEnvelope::task_created(&task, true);
        assert_eq!(envelope.entity_type(), "task");
        assert_eq!(envelope.action(), "created");
        assert!(envelope.from_sync());
        assert_eq!(envelope.id, None);
        assert_eq!(envelope.project_id, None);

        let parsed: Option<serde_json::Value> = envelope.parse_payload();
        assert_eq!(parsed, Some(json!({ "task": task, "from_sync": true })));
    }

    #[test]
    fn task_created_and_updated_payloads_include_merge_commit_sha() {
        let sha = "abc123def4567890abc123def4567890abc123de";
        let task = task_with_merge_commit_sha(Some(sha));

        for envelope in [
            DjinnEventEnvelope::task_created(&task, false),
            DjinnEventEnvelope::task_updated(&task, false),
        ] {
            assert_eq!(envelope.payload()["task"]["merge_commit_sha"], sha);
        }
    }

    #[test]
    fn envelope_project_deleted_has_id_only() {
        let envelope = DjinnEventEnvelope::project_deleted("proj-123");

        assert_eq!(envelope.entity_type(), "project");
        assert_eq!(envelope.action(), "deleted");
        assert_eq!(envelope.id.as_deref(), Some("proj-123"));
        assert_eq!(envelope.project_id, None);
        assert!(!envelope.from_sync());
        assert_eq!(envelope.payload(), &json!({"id": "proj-123"}));
    }

    #[test]
    fn envelope_session_message_nested_payload() {
        let msg = json!({"content": [{"type":"text","text":"hello"}]});
        let envelope = DjinnEventEnvelope::session_message("s1", "t1", "worker", &msg);

        assert_eq!(envelope.entity_type(), "session");
        assert_eq!(envelope.action(), "message");
        assert_eq!(
            envelope.payload(),
            &json!({
                "session_id": "s1",
                "task_id": "t1",
                "agent_type": "worker",
                "message": msg,
            })
        );
    }

    #[test]
    fn envelope_dispatch_pause_changed_includes_project_scope_and_pause_metadata() {
        let current = DispatchPause {
            paused_by: "admin-user".into(),
            paused_at: "2026-06-12T00:00:00.000Z".into(),
            reason: "maintenance".into(),
            expires_at: None,
        };

        let envelope = DjinnEventEnvelope::dispatch_pause_changed(
            DispatchPauseScope::Project,
            Some("project-1"),
            Some(&current),
            None,
            None,
            None,
        );

        assert_eq!(envelope.entity_type(), "dispatch_pause");
        assert_eq!(envelope.action(), "changed");
        assert_eq!(envelope.id.as_deref(), Some("project-1"));
        assert_eq!(envelope.project_id.as_deref(), Some("project-1"));
        assert_eq!(envelope.payload()["scope"], "project");
        assert_eq!(envelope.payload()["target_id"], "project-1");
        assert_eq!(envelope.payload()["current"]["paused_by"], "admin-user");
        assert_eq!(envelope.payload()["current"]["reason"], "maintenance");
        assert_eq!(envelope.payload()["previous"], serde_json::Value::Null);
        assert_eq!(envelope.payload()["paused_by"], "admin-user");
        assert_eq!(envelope.payload()["actor"], "admin-user");
        assert_eq!(envelope.payload()["changed_at"], "2026-06-12T00:00:00.000Z");
        assert_eq!(envelope.payload()["reason"], "maintenance");
    }

    #[test]
    fn envelope_dispatch_pause_changed_includes_resume_actor_and_previous_state() {
        let previous = DispatchPause {
            paused_by: "admin-user".into(),
            paused_at: "2026-06-12T00:00:00.000Z".into(),
            reason: "incident".into(),
            expires_at: None,
        };

        let envelope = DjinnEventEnvelope::dispatch_pause_changed(
            DispatchPauseScope::User,
            Some("user-1"),
            None,
            Some(&previous),
            Some("resumer"),
            Some("2026-06-12T00:05:00.000Z"),
        );

        assert_eq!(envelope.entity_type(), "dispatch_pause");
        assert_eq!(envelope.action(), "changed");
        assert_eq!(envelope.project_id, None);
        assert_eq!(envelope.payload()["scope"], "user");
        assert_eq!(envelope.payload()["target_id"], "user-1");
        assert_eq!(envelope.payload()["current"], serde_json::Value::Null);
        assert_eq!(envelope.payload()["previous"]["reason"], "incident");
        assert_eq!(envelope.payload()["resumed_by"], "resumer");
        assert_eq!(envelope.payload()["actor"], "resumer");
        assert_eq!(envelope.payload()["changed_at"], "2026-06-12T00:05:00.000Z");
        assert_eq!(envelope.payload()["reason"], serde_json::Value::Null);
    }

    #[test]
    fn envelope_task_lifecycle_step_maps_entity_action_and_payload() {
        let detail = json!({ "path": "/tmp/worktree" });
        let envelope = DjinnEventEnvelope::task_lifecycle_step("t1", "worktree_creating", &detail);

        assert_eq!(envelope.entity_type(), "lifecycle");
        assert_eq!(envelope.action(), "step");
        assert_eq!(envelope.project_id, None);
        assert_eq!(
            envelope.payload(),
            &json!({
                "task_id": "t1",
                "step": "worktree_creating",
                "detail": { "path": "/tmp/worktree" }
            })
        );
    }

    #[test]
    fn envelope_setting_updated_parse_payload_typed() {
        let setting = Setting {
            key: "foo".into(),
            value: "bar".into(),
            updated_at: "2025-01-01T00:00:00Z".into(),
        };
        let envelope = DjinnEventEnvelope {
            entity_type: "setting",
            action: "updated",
            payload: serde_json::to_value(&setting).unwrap(),
            id: None,
            project_id: None,
            from_sync: false,
        };

        assert_eq!(envelope.entity_type(), "setting");
        assert_eq!(envelope.action(), "updated");

        let parsed: Option<Setting> = envelope.parse_payload();
        assert!(parsed.is_some());
        let parsed = parsed.expect("setting payload parses");
        assert_eq!(parsed.key, setting.key);
        assert_eq!(parsed.value, setting.value);
        assert_eq!(parsed.updated_at, setting.updated_at);
    }

    #[test]
    fn envelope_serializes_flat_json() {
        let project = Project {
            id: "proj-1".into(),
            name: "name".into(),
            github_owner: "test".into(),
            github_repo: "proj".into(),
            created_at: "2025-01-01T00:00:00Z".into(),
            target_branch: "main".into(),
            auto_merge: false,
            sync_enabled: false,
            sync_remote: None,
        };
        let envelope = DjinnEventEnvelope::project_created(&project);
        let value = serde_json::to_value(envelope).expect("envelope serializes");

        assert!(value.get("entity_type").is_some());
        assert!(value.get("action").is_some());
        assert!(value.get("payload").is_some());
        assert!(value.get("from_sync").is_none());
    }

    #[test]
    fn event_bus_noop_does_not_panic() {
        let bus = super::EventBus::noop();
        let project = Project {
            id: "proj-1".into(),
            name: "name".into(),
            github_owner: "test".into(),
            github_repo: "proj".into(),
            created_at: "2025-01-01T00:00:00Z".into(),
            target_branch: "main".into(),
            auto_merge: false,
            sync_enabled: false,
            sync_remote: None,
        };
        bus.send(DjinnEventEnvelope::project_created(&project));
    }

    #[test]
    fn event_bus_new_receives_event() {
        use std::sync::{Arc, Mutex};
        let received = Arc::new(Mutex::new(Vec::new()));
        let received_clone = received.clone();
        let bus = super::EventBus::new(move |e| {
            received_clone.lock().unwrap().push(e.entity_type);
        });
        bus.send(DjinnEventEnvelope::project_deleted("x"));
        bus.send(DjinnEventEnvelope::epic_deleted("y"));
        let got = received.lock().unwrap();
        assert_eq!(*got, vec!["project", "epic"]);
    }
}
