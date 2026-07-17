// Shared private adapter factory used by host dispatch, reply-loop, and
// extraction adapters to build a [`djinn_slot::host::SlotContext`] from an
// [`AgentContext`].

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use djinn_core::canonical_verify::{
    CanonicalCommandDescriptorV1, CanonicalFinalVerificationPlanV1, CanonicalHermeticityV1,
    DeclaredExternalInputV1, ImmutableImageV1, ResolvedEnvironmentIdentityInputV1, ToolProbeStatus,
    ToolProbeV1, VerificationInputManifestV1,
};
use djinn_core::clock::SystemClock;
use djinn_core::models::VerifySource;
use djinn_db::TaskRunRepository;
use djinn_git::verification_input::{ResolvedExternalInputV1, VerificationInputFingerprintConfig};
use djinn_sandbox::final_verification_execution::{
    EnvironmentIdentityResolver, FinalVerificationExecutionRequest,
};
use djinn_supervisor::SupervisorServices;

use crate::context::AgentContext;

const UNKNOWN_IMAGE_DIGEST: &str =
    "sha256:0000000000000000000000000000000000000000000000000000000000000000";

/// Production lease callback. The coordinator owns release-before-persist; the
/// host must not inherit the trait's deliberately fail-closed test default.
struct HostFinalVerificationLease;
impl djinn_slot::final_verification::FinalVerificationInvocationLease
    for HostFinalVerificationLease
{
    fn release<'a>(&'a mut self) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}

fn output_directories(globs: &[String]) -> Result<Vec<PathBuf>, String> {
    let mut directories = std::collections::BTreeSet::new();
    for glob in globs {
        let mut prefix = PathBuf::new();
        for component in Path::new(glob).components() {
            let std::path::Component::Normal(component) = component else {
                return Err("final-verification output glob is not a safe relative path".into());
            };
            let component = component.to_string_lossy();
            if component.contains(['*', '?', '[', '{']) {
                break;
            }
            prefix.push(component.as_ref());
        }
        if prefix.as_os_str().is_empty() {
            return Err("final-verification output glob has no literal directory prefix".into());
        }
        directories.insert(prefix);
    }
    Ok(directories.into_iter().collect())
}

pub(crate) fn build_slot_context(
    agent: &AgentContext,
    callbacks: Arc<dyn djinn_slot::host::SlotHostCallbacks>,
    tool_dispatcher: Option<Arc<dyn djinn_slot::host::SlotToolDispatcher>>,
) -> djinn_slot::host::SlotContext {
    djinn_slot::host::SlotContext {
        db: agent.db.clone(),
        event_bus: agent.event_bus.clone(),
        catalog: agent.catalog.clone(),
        health_tracker: agent.health_tracker.clone(),
        background_work_tasks: agent.background_work_tasks.clone(),
        active_tasks: agent.active_tasks.clone(),
        default_project_id: agent.default_project_id.clone(),
        working_root: agent.working_root.clone(),
        coordinator_trigger: None,
        runtime_ops: agent.runtime_ops.clone(),
        repo_graph_ops: agent.repo_graph_ops.clone(),
        clock: Arc::new(SystemClock::new()),
        callbacks,
        tool_dispatcher,
        compaction_cs: agent.compaction_cs.clone(),
    }
}

/// Convert `&AgentContext` into a `SlotContext` with extraction host callbacks.
pub(crate) fn agent_to_slot_context(agent: &AgentContext) -> djinn_slot::host::SlotContext {
    build_slot_context(agent, Arc::new(AgentHostCallbacks::extraction(agent)), None)
}

/// Run `f` with a temporary `SlotContext` built from `&AgentContext`. This lets
/// thin agent-side wrappers avoid repeating the `agent_to_slot_context` call.
#[macro_export]
macro_rules! with_slot_context {
    ($app_state:expr, $body:expr) => {{
        let slot_ctx = $crate::actors::slot::adapter::agent_to_slot_context($app_state);
        $body(&slot_ctx).await
    }};
}

fn agent_credential_to_slot(
    credential: super::helpers::ProviderCredential,
) -> djinn_slot::helpers::ProviderCredential {
    match credential {
        super::helpers::ProviderCredential::ApiKey(key_name, api_key) => {
            djinn_slot::helpers::ProviderCredential::ApiKey(key_name, api_key)
        }
        super::helpers::ProviderCredential::OAuthConfig(config) => {
            djinn_slot::helpers::ProviderCredential::OAuthConfig(config)
        }
    }
}

/// Shared host-callback implementation for dispatch, reply-loop, and extraction.
pub(crate) struct AgentHostCallbacks {
    agent: AgentContext,
    services: Option<&'static dyn SupervisorServices>,
    dispatch_mode: bool,
}

impl AgentHostCallbacks {
    pub(crate) fn dispatch(agent: &AgentContext) -> Self {
        Self {
            agent: agent.clone(),
            services: None,
            dispatch_mode: true,
        }
    }
    pub(crate) fn reply_loop(
        agent: &AgentContext,
        services: &'static dyn SupervisorServices,
    ) -> Self {
        Self {
            agent: agent.clone(),
            services: Some(services),
            dispatch_mode: true,
        }
    }
    pub(crate) fn extraction(agent: &AgentContext) -> Self {
        Self {
            agent: agent.clone(),
            services: None,
            dispatch_mode: false,
        }
    }
}

impl djinn_slot::host::SlotHostCallbacks for AgentHostCallbacks {
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
                        djinn_slot::final_verification::FinalVerificationResolvedMaterial,
                        String,
                    >,
                > + Send
                + 'a,
        >,
    > {
        let agent = self.agent.clone();
        let id = task_run_id.to_owned();
        Box::pin(async move {
            let run = TaskRunRepository::new(agent.db.clone())
                .get(&id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or("task run missing")?;
            let worktree = run
                .workspace_path
                .map(PathBuf::from)
                .ok_or("task run has no worktree")?;
            let plan =
                crate::environment::environment_config_for_project_id(&agent.db, &run.project_id)
                    .await
                    .lifecycle
                    .final_verification;
            if plan.commands.is_empty() {
                return Err("final-verification plan is not configured".into());
            }
            let commands: Vec<_> = plan
                .commands
                .iter()
                .map(|c| CanonicalCommandDescriptorV1 {
                    check_id: c.check_id.clone(),
                    executable: c.executable.clone(),
                    argv: c.argv.clone(),
                    working_directory: c.working_directory.clone(),
                    environment_names: c.environment_names.clone(),
                    timeout_seconds: c.timeout_seconds,
                    descriptor_revision: c.descriptor_revision,
                })
                .collect();
            let manifest = VerificationInputManifestV1 {
                version: plan.input_manifest.version,
                repo_paths: plan.input_manifest.repo_paths.clone(),
                environment_names: plan.input_manifest.environment_names.clone(),
                read_only_external_inputs: plan
                    .read_only_external_inputs
                    .iter()
                    .map(|i| DeclaredExternalInputV1 {
                        id: i.id.clone(),
                        locator: i.locator.clone(),
                    })
                    .collect(),
                output_only_globs: plan.output_only_globs.clone(),
            };
            let external_inputs: Vec<_> = plan
                .read_only_external_inputs
                .iter()
                .map(|i| {
                    Ok(ResolvedExternalInputV1 {
                        id: i.id.clone(),
                        path: PathBuf::from(&i.locator),
                    })
                })
                .collect::<Result<_, String>>()?;
            let identity = ResolvedEnvironmentIdentityInputV1 {
                schema_version: 1,
                canonicalization_version: 1,
                plan: CanonicalFinalVerificationPlanV1 {
                    version: plan.version,
                    profile_id: plan.profile_id.clone(),
                    profile_revision: plan.profile_revision,
                    commands: commands.clone(),
                    required_checks: plan.required_checks.clone(),
                    hermeticity: CanonicalHermeticityV1 {
                        hermetic: plan.hermeticity.hermetic,
                        reusable: plan.hermeticity.reusable,
                        network_access: plan.hermeticity.network_access,
                    },
                },
                input_manifest: manifest.clone(),
                image: ImmutableImageV1 {
                    reference: "host".into(),
                    digest: UNKNOWN_IMAGE_DIGEST.into(),
                },
                tool_probes: commands
                    .iter()
                    .map(|c| ToolProbeV1 {
                        tool: c.executable.clone(),
                        version: "host".into(),
                        executable_digest: UNKNOWN_IMAGE_DIGEST.into(),
                        status: ToolProbeStatus::Passed,
                    })
                    .collect(),
                runner_version: env!("CARGO_PKG_VERSION").into(),
                lockfile_digests: Vec::new(),
                target: std::env::consts::ARCH.into(),
                features: Vec::new(),
                allowlisted_environment: BTreeMap::new(),
            };
            let resolver: EnvironmentIdentityResolver = Arc::new(move || Ok(identity.clone()));
            let output_directories = output_directories(&manifest.output_only_globs)?;
            Ok(
                djinn_slot::final_verification::FinalVerificationResolvedMaterial {
                    execution_request: FinalVerificationExecutionRequest {
                        worktree,
                        resolve_environment_identity: resolver,
                        fingerprint_config: VerificationInputFingerprintConfig {
                            base_ref: "main".into(),
                            manifest,
                            external_inputs: external_inputs.clone(),
                        },
                        tool_runtime: Vec::new(),
                        read_only_external_mounts: external_inputs
                            .into_iter()
                            .map(|i| i.path)
                            .collect(),
                        output_directories,
                    },
                    verify_source: VerifySource::Worker,
                    required_checks: plan.required_checks,
                    diff_fingerprint: String::new(),
                },
            )
        })
    }
    fn acquire_final_verification_lease<'a>(
        &'a self,
        _task_id: &'a str,
        _task_run_id: &'a str,
        _attempt: &'a str,
        _ctx: &'a djinn_slot::host::SlotContext,
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
        Box::pin(async {
            Ok(Box::new(HostFinalVerificationLease)
                as Box<
                    dyn djinn_slot::final_verification::FinalVerificationInvocationLease,
                >)
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
}
