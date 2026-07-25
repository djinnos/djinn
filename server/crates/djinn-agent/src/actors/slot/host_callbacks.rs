use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::context::AgentContext;
use djinn_supervisor::SupervisorServices;

use super::adapter::{
    AgentHostCallbacks, HostFinalVerificationLease, agent_credential_to_slot, build_slot_context,
    resolve_final_verification_for_task_run,
};

/// Build a dispatch-pathway [`djinn_slot::host::SlotContext`] from an [`AgentContext`].
pub(crate) fn agent_to_dispatch_slot_context(
    agent: &AgentContext,
) -> djinn_slot::host::SlotContext {
    build_slot_context(
        agent,
        std::sync::Arc::new(AgentHostCallbacks::dispatch(agent)),
        None,
    )
}

/// Build a reply-loop [`djinn_slot::host::SlotContext`] that routes liveness
/// and token-flush heartbeats through the live [`SupervisorServices`] handle.
pub(crate) fn agent_to_reply_loop_slot_context(
    agent: &AgentContext,
    services: &dyn SupervisorServices,
) -> djinn_slot::host::SlotContext {
    // SAFETY: the reply loop awaits every callback future before returning.
    let services_static = unsafe {
        std::mem::transmute::<&dyn SupervisorServices, &'static dyn SupervisorServices>(services)
    };
    build_slot_context(
        agent,
        std::sync::Arc::new(AgentHostCallbacks::reply_loop(agent, services_static)),
        None,
    )
}

impl djinn_slot::host::SlotHostCallbacks for AgentHostCallbacks {
    fn run_agent_verification<'a>(
        &'a self,
        task_id: &'a str,
        role_name: &'a str,
        _arguments: Option<serde_json::Map<String, serde_json::Value>>,
        cancellation: tokio_util::sync::CancellationToken,
        ctx: &'a djinn_slot::host::SlotContext,
    ) -> Pin<Box<dyn Future<Output = serde_json::Value> + Send + 'a>> {
        let limiter = Arc::clone(&self.run_verification_limiter);
        Box::pin(async move {
            crate::extension::handlers::verification::run_verification(
                &limiter,
                task_id,
                role_name,
                cancellation,
                ctx,
            )
            .await
        })
    }
    fn resolve_final_verification<'a>(
        &'a self,
        _task_id: &'a str,
        task_run_id: &'a str,
        _attempt: &'a str,
        _verify_run: &'a str,
        _ctx: &'a djinn_slot::host::SlotContext,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Option<djinn_slot::final_verification::FinalVerificationResolvedMaterial>,
                        String,
                    >,
                > + Send
                + 'a,
        >,
    > {
        let db = self.agent.db.clone();
        let id = task_run_id.to_owned();
        #[cfg(test)]
        let probe = self.probe.clone();
        Box::pin(async move {
            #[cfg(test)]
            if let Some(probe) = &probe {
                probe
                    .resolver_calls
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            resolve_final_verification_for_task_run(&db, &id).await
        })
    }
    fn acquire_final_verification_lease<'a>(
        &'a self,
        _task_id: &'a str,
        _task_run_id: &'a str,
        attempt: &'a str,
        ctx: &'a djinn_slot::host::SlotContext,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Box<dyn djinn_slot::final_verification::FinalVerificationInvocationLease>,
                        String,
                    >,
                > + Send
                + 'a,
        >,
    > {
        let db = ctx.db.clone();
        let attempt = attempt.to_owned();
        #[cfg(test)]
        let probe = self.probe.clone();
        Box::pin(async move {
            #[cfg(test)]
            if let Some(probe) = &probe {
                probe
                    .lease_requests
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            let lease = HostFinalVerificationLease::acquire(&db, &attempt).await?;
            #[cfg(test)]
            if let Some(probe) = &probe {
                probe
                    .lease_acquisitions
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            Ok(lease)
        })
    }
    fn interrupt_paused_worker_session<'a>(
        &'a self,
        _task_id: &'a str,
        _ctx: &'a djinn_slot::host::SlotContext,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async {})
    }
    fn resolve_mcp_tools<'a>(
        &'a self,
        _worktree_path: &'a str,
        _role_name: &'a str,
        _ctx: &'a djinn_slot::host::SlotContext,
    ) -> Pin<Box<dyn Future<Output = Result<djinn_slot::host::ResolvedMcpTools, String>> + Send + 'a>>
    {
        Box::pin(async { Err("not available in host adapter".into()) })
    }
    fn render_prompt(
        &self,
        _role_name: &str,
        _task: &djinn_core::models::Task,
        _context_json: &serde_json::Value,
    ) -> String {
        String::new()
    }
    fn initial_user_message<'a>(
        &'a self,
        _task_id: &'a str,
        _ctx: &'a djinn_slot::host::SlotContext,
    ) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
        Box::pin(async { String::new() })
    }
    fn build_mcp_state(
        &self,
        _ctx: &djinn_slot::host::SlotContext,
    ) -> djinn_control_plane::McpState {
        unreachable!("build_mcp_state not available in host adapter")
    }
    fn require_project_id_for_task_ops<'a>(
        &'a self,
        _project: &'a str,
        _ctx: &'a djinn_slot::host::SlotContext,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<String, djinn_control_plane::tools::task_tools::ErrorResponse>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async {
            Err(djinn_control_plane::tools::task_tools::ErrorResponse {
                error: "not available in host adapter".into(),
            })
        })
    }
    fn resolve_provider_credential<'a>(
        &'a self,
        provider_id: &'a str,
        _ctx: &'a djinn_slot::host::SlotContext,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<djinn_slot::helpers::ProviderCredential, String>>
                + Send
                + 'a,
        >,
    > {
        if self.dispatch_mode {
            return Box::pin(async { Err("not available in dispatch callback".into()) });
        }
        let agent = self.agent.clone();
        Box::pin(async move {
            super::helpers::load_provider_credential(provider_id, &agent)
                .await
                .map(agent_credential_to_slot)
                .map_err(|e| {
                    format!(
                        "extraction credential resolution failed for provider {provider_id}: {e}"
                    )
                })
        })
    }
    fn run_task_dispatch<'a>(
        &'a self,
        task_id: String,
        project_path: String,
        model_id: String,
        ctx: djinn_slot::host::SlotContext,
        kill: tokio_util::sync::CancellationToken,
        pause: tokio_util::sync::CancellationToken,
        resume_lifecycle_metadata: Option<serde_json::Value>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        if !self.dispatch_mode {
            return Box::pin(async { Ok(()) });
        }
        // Propagate the slot actor's per-run CompactionCriticalSection into the
        // AgentContext so the reply-loop adapter (which builds SlotContext from
        // AgentContext) shares the same handle the actor retains on
        // ActiveLifecycle.  Without this, the agent-side reply loop would use a
        // stale/default CompactionCriticalSection from the AgentContext that was
        // constructed at server startup, breaking the shared-handle invariant.
        let mut agent = self.agent.clone();
        agent.compaction_cs = ctx.compaction_cs.clone();
        Box::pin(async move {
            super::supervisor_runner::dispatch_task_runtime(
                task_id,
                project_path,
                model_id,
                agent,
                kill,
                pause,
                resume_lifecycle_metadata,
            )
            .await
        })
    }
    fn touch_activity_rpc<'a>(
        &'a self,
        task_id: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        match self.services {
            Some(services) => Box::pin(async move { services.touch_activity(task_id).await }),
            None => Box::pin(async { Ok(()) }),
        }
    }
    fn flush_session_tokens_rpc<'a>(
        &'a self,
        session_id: String,
        tokens_in: i64,
        tokens_out: i64,
        cache_read: i64,
        cache_write: i64,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        match self.services {
            Some(services) => Box::pin(async move {
                services
                    .flush_session_tokens(
                        session_id,
                        tokens_in,
                        tokens_out,
                        cache_read,
                        cache_write,
                    )
                    .await
            }),
            None => Box::pin(async { Ok(()) }),
        }
    }
    #[cfg(test)]
    fn final_verification_outcome_for_test(
        &self,
        _request: &djinn_slot::final_verification::FinalVerificationCoordinatorRequest,
    ) -> Option<djinn_slot::final_verification::FinalVerificationRecordingOutcome> {
        // When the observation probe is present, decline the terminal test
        // shortcut so the coordinator reaches the real repository-backed
        // resolver. The shortcut counter stays at its default 0 because this
        // early return never enters the synthetic branch, so `0` is an explicit
        // assertion that the shortcut did not decide the coordinator regression.
        if self.probe.is_some() {
            return None;
        }
        Some(
            djinn_slot::final_verification::FinalVerificationRecordingOutcome::Stored {
                verification_attempt_id: uuid::Uuid::now_v7().to_string(),
                verify_run_id: uuid::Uuid::now_v7().to_string(),
                evidence: Box::new(
                    djinn_slot::final_verification::FinalVerificationSuccessEvidence {
                        persisted_run_id: uuid::Uuid::now_v7().to_string(),
                        completed_at: "2025-01-01T00:00:00Z".to_owned(),
                        ordered_commands: serde_json::json!([]),
                        covered_checks: serde_json::json!([]),
                        required_checks: vec![],
                        verification_input_fingerprint: "test-fingerprint".to_owned(),
                        manifest_version: "manifest-v1".to_owned(),
                        environment_identity_digest: "test-identity".to_owned(),
                    },
                ),
            },
        )
    }
    #[cfg(test)]
    fn record_final_verification_consultation_outcome_for_test(
        &self,
        outcome: &'static str,
        reason: &'static str,
    ) {
        if let Some(probe) = &self.probe {
            probe
                .consultation_outcomes
                .lock()
                .expect("consultation probe mutex not poisoned")
                .push((outcome, reason));
        }
    }
    #[cfg(test)]
    fn final_verification_evidence_for_test(
        &self,
        _request: &djinn_slot::final_verification::FinalVerificationCoordinatorRequest,
    ) -> Option<djinn_sandbox::final_verification_execution::FinalVerificationExecutionEvidence>
    {
        if let Some(probe) = &self.probe {
            // The expected resolver error makes the coordinator return before the
            // canonical execution checkpoint, so this must never be reached.
            // Incrementing here converts any unexpected traversal into a
            // countable assertion failure rather than injecting passing evidence.
            probe
                .canonical_execution_requests
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        None
    }
}
