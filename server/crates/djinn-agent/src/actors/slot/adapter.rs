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
use djinn_db::advisory_lock;
use djinn_db::{ImageRepository, TaskRunRepository};
use djinn_git::verification_input::{
    ResolvedExternalInputV1, VerificationInputFingerprintConfig, collect_verification_changed_paths,
};
use djinn_k8s::sidecar::resolve_image_services_strict;
use djinn_sandbox::final_verification_execution::{
    EnvironmentIdentityResolver, FinalVerificationExecutionRequest,
};
use djinn_supervisor::SupervisorServices;
use globset::{Glob, GlobSetBuilder};

use super::catalog_endpoints::catalog_loopback_endpoints;
use crate::context::AgentContext;

const UNKNOWN_IMAGE_DIGEST: &str =
    "sha256:0000000000000000000000000000000000000000000000000000000000000000";

/// Advisory-lock class id for final-verification invocation leases. Chosen from
/// the application-specific range to avoid collisions with the migrator and
/// template-bootstrap locks.
const FINAL_VERIFICATION_LOCK_CLASS: i32 = 0x4656_5246; // "FVRF"
/// Per-attempt object id derived by hashing the verification attempt id, so
/// distinct concurrent attempts do not contend for the same lock row.
fn final_verification_lock_object(verification_attempt_id: &str) -> i32 {
    let hash = verification_attempt_id
        .as_bytes()
        .iter()
        .fold(0u32, |acc, byte| {
            acc.wrapping_mul(31).wrapping_add(*byte as u32)
        });
    hash as i32
}

/// Production invocation lease backed by a dedicated Postgres connection holding
/// a session-scoped advisory lock. The coordinator explicitly releases it before
/// persistence; releasing drops the connection, which also releases the lock.
struct HostFinalVerificationLease {
    conn: Option<sqlx::postgres::PgConnection>,
    class_id: i32,
    object_id: i32,
}
impl HostFinalVerificationLease {
    async fn acquire(
        db: &djinn_db::Database,
        verification_attempt_id: &str,
    ) -> Result<Box<dyn djinn_slot::final_verification::FinalVerificationInvocationLease>, String>
    {
        let class_id = FINAL_VERIFICATION_LOCK_CLASS;
        let object_id = final_verification_lock_object(verification_attempt_id);
        // Open a dedicated connection (not a pooled one) so the session-scoped
        // advisory lock persists until the connection is dropped on release.
        let opts = db.pool().connect_options();
        let mut conn = sqlx::Connection::connect_with(&*opts)
            .await
            .map_err(|e| format!("final-verification lease connection failed: {e}"))?;
        let acquired = advisory_lock::try_acquire(&mut conn, class_id, object_id)
            .await
            .map_err(|e| format!("final-verification lock query failed: {e}"))?;
        if !acquired {
            return Err("final-verification invocation lock is held by another attempt".to_owned());
        }
        Ok(Box::new(Self {
            conn: Some(conn),
            class_id,
            object_id,
        })
            as Box<
                dyn djinn_slot::final_verification::FinalVerificationInvocationLease,
            >)
    }
}
impl djinn_slot::final_verification::FinalVerificationInvocationLease
    for HostFinalVerificationLease
{
    fn release<'a>(&'a mut self) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        let class_id = self.class_id;
        let object_id = self.object_id;
        Box::pin(async move {
            if let Some(mut conn) = self.conn.take() {
                advisory_lock::release(&mut conn, class_id, object_id)
                    .await
                    .map_err(|e| format!("final-verification lock release failed: {e}"))?;
            }
            Ok(())
        })
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

/// Canonical host runtime directories that the strict Landlock launcher must
/// grant read/execute access to for final-verification commands. The launcher
/// requires `tool_runtime` to include the executable and every runtime
/// directory it needs (`/usr`, `/bin`, `/lib`, `/lib64`). Only existing
/// directories are included so symlink-free hosts are handled correctly.
fn canonical_tool_runtime() -> Vec<PathBuf> {
    ["/usr", "/bin", "/lib", "/lib64"]
        .into_iter()
        .filter(|path| Path::new(path).is_dir())
        .map(PathBuf::from)
        .collect()
}

fn canonical_command(
    c: &djinn_stack::environment::FinalVerificationCommand,
) -> CanonicalCommandDescriptorV1 {
    CanonicalCommandDescriptorV1 {
        check_id: c.check_id.clone(),
        executable: c.executable.clone(),
        argv: c.argv.clone(),
        working_directory: c.working_directory.clone(),
        environment_names: c.environment_names.clone(),
        timeout_seconds: c.timeout_seconds,
        descriptor_revision: c.descriptor_revision,
    }
}
fn resolve_selected_final_verification(
    plan: &djinn_stack::environment::FinalVerificationPlan,
    paths: &[String],
) -> Result<(Vec<CanonicalCommandDescriptorV1>, Vec<String>), String> {
    if plan.command_groups.is_empty() {
        return Ok((
            plan.commands.iter().map(canonical_command).collect(),
            vec![],
        ));
    }
    let mut selected = std::collections::BTreeSet::new();
    for rule in &plan.selection_rules {
        // The schema-mandated `**` rule is the all-groups fail-safe, not a
        // normal selector. Matching it alongside a specific rule would make
        // every changed path select every command group.
        if rule.match_globs.iter().any(|glob| glob == "**") {
            continue;
        }
        let mut b = GlobSetBuilder::new();
        for g in &rule.match_globs {
            b.add(Glob::new(g).map_err(|e| e.to_string())?);
        }
        let m = b.build().map_err(|e| e.to_string())?;
        if paths.iter().any(|p| m.is_match(p)) {
            selected.extend(rule.command_groups.iter().cloned());
        }
    }
    if paths.is_empty() || selected.is_empty() {
        selected.extend(plan.command_groups.iter().map(|g| g.name.clone()));
    }
    let names = plan
        .command_groups
        .iter()
        .filter(|g| selected.contains(&g.name))
        .map(|g| g.name.clone())
        .collect();
    let commands: Vec<_> = plan
        .command_groups
        .iter()
        .filter(|g| selected.contains(&g.name))
        .flat_map(|g| g.commands.iter().map(canonical_command))
        .collect();
    if commands.is_empty() {
        Err("final-verification plan resolved to no commands".into())
    } else {
        Ok((commands, names))
    }
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
    /// Test-private observation probe. `None` in every production constructor
    /// and in existing tests preserves all current behavior. When `Some`, it
    /// disables the deterministic `final_verification_outcome_for_test`
    /// short-circuit so a coordinator regression reaches the real
    /// repository-backed resolver, and it counts traversal of the production
    /// resolve/lease/evidence boundaries. See the NULL-workspace coordinator
    /// regression `configured_null_pod_workspace_fails_closed_through_coordinator_and_production_resolver`.
    #[cfg(test)]
    probe: Option<Arc<FinalVerificationProbe>>,
}

/// Test-only counters proving a configured NULL-workspace task run fails closed
/// before consultation, lease acquisition, canonical execution, and persistence.
/// All members stay `#[cfg(test)]` and private to this module; production builds
/// never construct it.
#[cfg(test)]
#[derive(Default)]
struct FinalVerificationProbe {
    terminal_outcome_shortcuts: std::sync::atomic::AtomicUsize,
    resolver_calls: std::sync::atomic::AtomicUsize,
    consultation_outcomes: std::sync::Mutex<Vec<(&'static str, &'static str)>>,
    lease_requests: std::sync::atomic::AtomicUsize,
    lease_acquisitions: std::sync::atomic::AtomicUsize,
    canonical_execution_requests: std::sync::atomic::AtomicUsize,
}

impl AgentHostCallbacks {
    pub(crate) fn dispatch(agent: &AgentContext) -> Self {
        Self {
            agent: agent.clone(),
            services: None,
            dispatch_mode: true,
            #[cfg(test)]
            probe: None,
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
            #[cfg(test)]
            probe: None,
        }
    }
    pub(crate) fn extraction(agent: &AgentContext) -> Self {
        Self {
            agent: agent.clone(),
            services: None,
            dispatch_mode: false,
            #[cfg(test)]
            probe: None,
        }
    }
}

impl AgentHostCallbacks {
    /// Test-private constructor that returns a dispatch-shaped callback with the
    /// terminal-outcome short-circuit disabled and a shared observation probe.
    /// The probe lets a coordinator regression prove the production resolver is
    /// traversed exactly once and no consultation/lease/execution/persistence
    /// boundary is reached. Production code never calls this.
    #[cfg(test)]
    fn dispatch_with_final_verification_probe(
        agent: &AgentContext,
    ) -> (Self, Arc<FinalVerificationProbe>) {
        let probe = Arc::new(FinalVerificationProbe::default());
        (
            Self {
                agent: agent.clone(),
                services: None,
                dispatch_mode: true,
                probe: Some(probe.clone()),
            },
            probe,
        )
    }
}

/// Resolve production final-verification material from the persisted task-run.
/// The completion callback delegates here so pod tests exercise the same
/// configured-plan/worktree resolution boundary as completion.
pub async fn resolve_final_verification_for_task_run(
    db: &djinn_db::Database,
    task_run_id: &str,
) -> Result<Option<djinn_slot::final_verification::FinalVerificationResolvedMaterial>, String> {
    let run = TaskRunRepository::new(db.clone())
        .get(task_run_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("task run missing")?;
    let plan = crate::environment::environment_config_for_project_id(db, &run.project_id)
        .await
        .lifecycle
        .final_verification;
    // Typed skip, checked BEFORE the worktree requirement: a project
    // with zero final-verification commands has no completion boundary
    // to enforce, so submission must not depend on a resolvable
    // worktree. A configured plan keeps the fail-closed worktree
    // requirement below.
    if plan.normalized_commands().is_empty() {
        return Ok(None);
    }
    let worktree = run
        .workspace_path
        .map(PathBuf::from)
        .ok_or("task run has no worktree")?;
    // Catalog authorization is bound at dispatch, never re-selected from a
    // mutable project. A changed project selection makes verification ineligible.
    // Resolve it only for a configured plan with a usable worktree: an empty
    // plan is a typed skip and cannot consume catalog identity material.
    let runs = TaskRunRepository::new(db.clone());
    let bound_image_id = runs
        .catalog_image_id(task_run_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("task run has no bound catalog image")?;
    let image_repo = ImageRepository::new(db.clone());
    let current_image = image_repo
        .resolve_for_project(&run.project_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("project has no selected catalog image")?;
    if current_image.id != bound_image_id {
        return Err("task-run/current-image mismatch".into());
    }
    let catalog_image = image_repo
        .get(&bound_image_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("task-run catalog image missing")?;
    let services = resolve_image_services_strict(db, &bound_image_id)
        .await
        .map_err(|e| format!("strict catalog resolution failed: {e}"))?;
    let catalog_loopback_endpoints = catalog_loopback_endpoints(&services)?;
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
    let fingerprint_config = VerificationInputFingerprintConfig {
        base_ref: "main".into(),
        manifest: manifest.clone(),
        external_inputs: external_inputs.clone(),
    };
    let changed_paths = collect_verification_changed_paths(&worktree, &fingerprint_config)
        .await
        .map_err(|e| e.to_string())?;
    let (commands, selected_groups) = resolve_selected_final_verification(&plan, &changed_paths)?;
    let required_checks = commands
        .iter()
        .map(|c| c.check_id.clone())
        .collect::<Vec<_>>();
    let identity = ResolvedEnvironmentIdentityInputV1 {
        schema_version: 2,
        canonicalization_version: 2,
        plan: CanonicalFinalVerificationPlanV1 {
            version: plan.version,
            profile_id: plan.profile_id.clone(),
            profile_revision: plan.profile_revision,
            commands: commands.clone(),
            required_checks: required_checks.clone(),
            hermeticity: CanonicalHermeticityV1 {
                hermetic: plan.hermeticity.hermetic,
                reusable: plan.hermeticity.reusable,
                network_access: plan.hermeticity.network_access,
            },
        },
        selection: if plan.command_groups.is_empty() {
            djinn_core::canonical_verify::ResolvedVerificationSelectionV1::legacy_flat_plan()
        } else {
            djinn_core::canonical_verify::ResolvedVerificationSelectionV1::SelectedGroups {
                selection_rules: plan
                    .selection_rules
                    .iter()
                    .map(
                        |r| djinn_core::canonical_verify::ResolvedVerificationSelectionRuleV1 {
                            matches: r.match_globs.clone(),
                            command_groups: r.command_groups.clone(),
                        },
                    )
                    .collect(),
                selected_command_groups: selected_groups,
            }
        },
        input_manifest: manifest.clone(),
        image: ImmutableImageV1 {
            reference: catalog_image
                .tag
                .clone()
                .ok_or("task-run catalog image has no reference")?,
            digest: catalog_image
                .registry_digest
                .clone()
                .ok_or("task-run catalog image has no digest")?,
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
        services: services
            .into_iter()
            .map(
                |service| djinn_core::canonical_verify::ResolvedCatalogServiceIdentityV1 {
                    preset_id: service.preset_id,
                    service_type: service.service_type,
                    image_reference: service.image_reference,
                    image_digest: service.image_digest,
                    port: service.port,
                    exported_environment_names: service.exported_environment_names,
                    verification_protocol_revision: service.verification_protocol_revision,
                    effective_configuration_digest: service.effective_configuration_digest,
                },
            )
            .collect(),
    };
    let resolver: EnvironmentIdentityResolver = Arc::new(move || Ok(identity.clone()));
    let output_directories = output_directories(&manifest.output_only_globs)?;
    Ok(Some(
        djinn_slot::final_verification::FinalVerificationResolvedMaterial {
            execution_request: FinalVerificationExecutionRequest {
                worktree,
                resolve_environment_identity: resolver,
                fingerprint_config,
                tool_runtime: canonical_tool_runtime(),
                read_only_external_mounts: external_inputs.into_iter().map(|i| i.path).collect(),
                output_directories,
                catalog_loopback_endpoints,
            },
            verify_source: VerifySource::Worker,
            required_checks,
            diff_fingerprint: String::new(),
        },
    ))
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

#[cfg(test)]
mod resolve_final_verification_tests {
    use djinn_core::events::EventBus;
    use djinn_db::ProjectRepository;
    use djinn_db::repositories::task_run::CreateTaskRunParams;
    use djinn_slot::host::SlotHostCallbacks;
    use djinn_stack::environment::{
        EnvironmentConfig, FinalVerificationCommand, FinalVerificationCommandGroup,
        FinalVerificationPlan, FinalVerificationSelectionRule,
    };
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::test_helpers::{
        agent_context_from_db, create_test_db, create_test_epic, create_test_project,
        create_test_task, test_tempdir,
    };

    struct Fixture {
        agent: AgentContext,
        project_id: String,
        task_id: String,
        task_run_id: String,
    }

    /// Seed a project/epic/task and a task_run row shaped like a K8s pod run
    /// (coordinator-inserted, `workspace_path` as given).
    async fn fixture(workspace_path: Option<&str>) -> Fixture {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let task = create_test_task(&db, &project.id, &epic.id).await;
        let task_run_id = uuid::Uuid::now_v7().to_string();
        TaskRunRepository::new(db.clone())
            .create(CreateTaskRunParams {
                id: &task_run_id,
                project_id: &project.id,
                task_id: &task.id,
                trigger_type: "dispatch",
                status: Some("running"),
                workspace_path,
                mirror_ref: None,
                dispatch_group_id: None,
            })
            .await
            .unwrap();
        Fixture {
            agent: agent_context_from_db(db, CancellationToken::new()),
            project_id: project.id,
            task_id: task.id,
            task_run_id,
        }
    }

    async fn run_git(worktree: &std::path::Path, args: &[&str]) {
        djinn_git::run_git_command_in(worktree, args.iter().map(|arg| (*arg).to_owned()).collect())
            .await
            .unwrap_or_else(|error| panic!("git {args:?} failed: {error}"));
    }

    /// The production resolver derives selection paths from the persisted
    /// worktree, so tests that exercise a configured worktree need a minimal
    /// repository with a `main` merge-base rather than a placeholder path.
    async fn initialized_worktree() -> tempfile::TempDir {
        let worktree = test_tempdir("final-verification-worktree-");
        run_git(worktree.path(), &["init", "-b", "main"]).await;
        run_git(
            worktree.path(),
            &["config", "user.email", "test@example.com"],
        )
        .await;
        run_git(
            worktree.path(),
            &["config", "user.name", "Final Verification Test"],
        )
        .await;
        std::fs::write(worktree.path().join("README.md"), "base\n")
            .expect("write initial worktree file");
        run_git(worktree.path(), &["add", "README.md"]).await;
        run_git(worktree.path(), &["commit", "-m", "initial commit"]).await;
        worktree
    }

    async fn configure_final_verification_plan(fx: &Fixture) {
        let mut cfg = EnvironmentConfig::empty();
        cfg.lifecycle.final_verification.commands = vec![FinalVerificationCommand {
            check_id: "lint".into(),
            executable: "true".into(),
            ..Default::default()
        }];
        cfg.lifecycle.final_verification.required_checks = vec!["lint".into()];
        ProjectRepository::new(fx.agent.db.clone(), EventBus::noop())
            .set_environment_config(&fx.project_id, &serde_json::to_string(&cfg).unwrap())
            .await
            .unwrap();
    }

    async fn resolve(
        fx: &Fixture,
    ) -> Result<Option<djinn_slot::final_verification::FinalVerificationResolvedMaterial>, String>
    {
        let callbacks = AgentHostCallbacks::dispatch(&fx.agent);
        let ctx = agent_to_slot_context(&fx.agent);
        callbacks
            .resolve_final_verification(&fx.task_id, &fx.task_run_id, "attempt", "run", &ctx)
            .await
    }

    fn grouped_plan() -> FinalVerificationPlan {
        let plan = FinalVerificationPlan {
            profile_id: "resolver-test".into(),
            profile_revision: 1,
            command_groups: vec![
                FinalVerificationCommandGroup {
                    name: "server".into(),
                    commands: vec![FinalVerificationCommand {
                        check_id: "server-check".into(),
                        executable: "server-test".into(),
                        timeout_seconds: 300,
                        descriptor_revision: 1,
                        ..Default::default()
                    }],
                },
                FinalVerificationCommandGroup {
                    name: "ui".into(),
                    commands: vec![FinalVerificationCommand {
                        check_id: "ui-check".into(),
                        executable: "ui-test".into(),
                        timeout_seconds: 300,
                        descriptor_revision: 1,
                        ..Default::default()
                    }],
                },
            ],
            // Keep the rule order deliberately opposite the group order: a
            // union of both rules must still emit server before ui.
            selection_rules: vec![
                FinalVerificationSelectionRule {
                    match_globs: vec!["ui/**".into()],
                    command_groups: vec!["ui".into()],
                },
                FinalVerificationSelectionRule {
                    match_globs: vec!["server/**".into()],
                    command_groups: vec!["server".into()],
                },
                FinalVerificationSelectionRule {
                    match_globs: vec!["**".into()],
                    command_groups: vec!["server".into(), "ui".into()],
                },
            ],
            ..Default::default()
        };
        let mut config = EnvironmentConfig::empty();
        config.lifecycle.final_verification = plan.clone();
        config
            .validate()
            .expect("grouped resolver fixture must satisfy environment validation");
        plan
    }

    fn assert_selection(paths: &[&str], expected_checks: &[&str], expected_groups: &[&str]) {
        let paths = paths.iter().map(ToString::to_string).collect::<Vec<_>>();
        let (commands, groups) = resolve_selected_final_verification(&grouped_plan(), &paths)
            .expect("grouped plan must resolve selected commands");
        assert_eq!(
            commands
                .iter()
                .map(|command| command.check_id.as_str())
                .collect::<Vec<_>>(),
            expected_checks
        );
        assert_eq!(groups, expected_groups);
    }

    #[test]
    fn path_rules_select_groups_with_configuration_ordered_commands() {
        // A single matching path selects only that rule's group.
        assert_selection(&["server/src/lib.rs"], &["server-check"], &["server"]);
        assert_selection(&["ui/src/app.tsx"], &["ui-check"], &["ui"]);

        // No matching path, or no changed paths, is the fail-safe all-groups
        // selection. Commands follow command_groups configuration order rather
        // than the reverse selection-rule order above.
        assert_selection(
            &["README.md"],
            &["server-check", "ui-check"],
            &["server", "ui"],
        );
        assert_selection(&[], &["server-check", "ui-check"], &["server", "ui"]);
        assert_selection(
            &["ui/src/app.tsx", "server/src/lib.rs"],
            &["server-check", "ui-check"],
            &["server", "ui"],
        );
    }

    #[test]
    fn overlapping_rules_union_groups_without_duplicate_commands() {
        let mut plan = grouped_plan();
        plan.selection_rules.push(FinalVerificationSelectionRule {
            match_globs: vec!["server/src/**".into()],
            command_groups: vec!["server".into()],
        });

        let (commands, groups) =
            resolve_selected_final_verification(&plan, &["server/src/lib.rs".into()])
                .expect("overlapping rules must resolve a deduplicated group union");
        assert_eq!(
            commands
                .iter()
                .map(|command| command.check_id.as_str())
                .collect::<Vec<_>>(),
            ["server-check"]
        );
        assert_eq!(groups, ["server"]);
    }

    #[test]
    fn grouped_plan_that_selects_no_commands_fails_closed() {
        let plan = FinalVerificationPlan {
            command_groups: vec![FinalVerificationCommandGroup {
                name: "empty".into(),
                commands: vec![],
            }],
            selection_rules: vec![FinalVerificationSelectionRule {
                match_globs: vec!["server/**".into()],
                command_groups: vec!["empty".into()],
            }],
            ..Default::default()
        };

        assert_eq!(
            resolve_selected_final_verification(&plan, &["server/src/lib.rs".into()])
                .expect_err("a selected group without commands must fail closed"),
            "final-verification plan resolved to no commands"
        );
    }

    /// Defect 2 regression: a project with no final-verification plan resolves
    /// the typed `Ok(None)` skip — even when the pod run's `workspace_path` is
    /// NULL — instead of erroring and blocking every submission.
    #[tokio::test]
    async fn unconfigured_plan_resolves_typed_skip_even_without_worktree() {
        let fx = fixture(None).await;
        let resolved = resolve(&fx).await;
        assert!(
            matches!(resolved, Ok(None)),
            "unconfigured plan must resolve the typed skip, got {:?}",
            resolved.map(|m| m.map(|_| "material"))
        );
    }

    /// A configured plan keeps the fail-closed worktree requirement: a run row
    /// without a workspace_path is a hard resolution error, never a skip.
    #[tokio::test]
    async fn configured_plan_with_missing_worktree_fails_closed() {
        let fx = fixture(None).await;
        configure_final_verification_plan(&fx).await;
        let resolved = resolve(&fx).await;
        match resolved {
            Err(detail) => assert!(
                detail.contains("task run has no worktree"),
                "configured plan without a worktree must fail closed, got {detail}"
            ),
            Ok(material) => panic!(
                "configured plan without a worktree must not resolve, got {:?}",
                material.map(|_| "material")
            ),
        }
    }

    #[tokio::test]
    async fn task_run_catalog_image_drift_fails_before_catalog_resolution() {
        let worktree = initialized_worktree().await;
        let fx = fixture(Some(
            worktree.path().to_str().expect("worktree path is UTF-8"),
        ))
        .await;
        configure_final_verification_plan(&fx).await;
        let images = ImageRepository::new(fx.agent.db.clone());
        images
            .create("catalog-image-at-dispatch", "dispatch", None, "{}")
            .await
            .unwrap();
        images
            .create("catalog-image-now", "current", None, "{}")
            .await
            .unwrap();
        TaskRunRepository::new(fx.agent.db.clone())
            .set_catalog_image_id(&fx.task_run_id, "catalog-image-at-dispatch")
            .await
            .unwrap();
        images
            .set_project_image(&fx.project_id, Some("catalog-image-now"))
            .await
            .unwrap();

        match resolve(&fx).await {
            Err(detail) => assert_eq!(detail, "task-run/current-image mismatch"),
            Ok(_) => {
                panic!("verification must never substitute a project image selected after dispatch")
            }
        }
    }

    /// A configured plan reads the path persisted onto a pod-shaped NULL row.
    #[tokio::test]
    async fn configured_plan_reads_persisted_pod_workspace_path() {
        let fx = fixture(None).await;
        let worktree = initialized_worktree().await;
        TaskRunRepository::new(fx.agent.db.clone())
            .set_workspace_path(
                &fx.task_run_id,
                worktree.path().to_str().expect("worktree path is UTF-8"),
            )
            .await
            .expect("first in-pod stage persists workspace path");
        configure_final_verification_plan(&fx).await;
        let material = resolve(&fx)
            .await
            .expect("configured plan with persisted worktree must resolve")
            .expect("configured plan must not be a typed skip");
        assert_eq!(material.execution_request.worktree, worktree.path());
    }

    /// A configured plan with a recorded worktree resolves full material.
    #[tokio::test]
    async fn configured_plan_with_worktree_resolves_material() {
        let worktree = initialized_worktree().await;
        let fx = fixture(Some(
            worktree.path().to_str().expect("worktree path is UTF-8"),
        ))
        .await;
        configure_final_verification_plan(&fx).await;
        let material = resolve(&fx)
            .await
            .expect("configured plan with worktree must resolve")
            .expect("configured plan must not be a typed skip");
        assert_eq!(material.execution_request.worktree, worktree.path());
        assert_eq!(material.required_checks, vec!["lint"]);
    }

    /// Production-incident regression (proposal czd9 rev 10): a k8s-shaped
    /// task run created exactly as dispatch does — `workspace_path = NULL` at
    /// insert — with a configured non-empty final-verification plan fails
    /// closed through the real `coordinate_final_verification` coordinator and
    /// repository-backed `resolve_final_verification_for_task_run`. The resolver
    /// error terminates coordination before consultation, invocation-lease
    /// request/acquisition, canonical execution, persistence, or any derivable
    /// success evidence/finalize payload.
    #[tokio::test]
    async fn configured_null_pod_workspace_fails_closed_through_coordinator_and_production_resolver()
     {
        use djinn_db::repositories::verify_run::VerifyRunRepository;
        use djinn_slot::final_verification::{
            FinalVerificationCoordinatorRequest, FinalVerificationRecordingOutcome,
            coordinate_final_verification,
        };
        use std::sync::atomic::Ordering;

        // Exactly like k8s dispatch: NULL workspace_path at insert.
        let fx = fixture(None).await;
        configure_final_verification_plan(&fx).await;

        // Confirm the configured row is NULL before coordination.
        let run_before = TaskRunRepository::new(fx.agent.db.clone())
            .get(&fx.task_run_id)
            .await
            .expect("task run exists")
            .expect("task run row present");
        assert!(
            run_before.workspace_path.is_none(),
            "fixture must insert workspace_path = NULL exactly as k8s dispatch does"
        );

        // Production callbacks with the test-only probe: it disables the
        // terminal `final_verification_outcome_for_test` short-circuit and counts
        // traversal of the production resolve/lease/evidence boundaries. No
        // resolution logic is duplicated — the callback delegates unchanged to
        // `resolve_final_verification_for_task_run`.
        let (callbacks, probe) =
            AgentHostCallbacks::dispatch_with_final_verification_probe(&fx.agent);
        let ctx = build_slot_context(&fx.agent, Arc::new(callbacks), None);

        let outcome = coordinate_final_verification(
            FinalVerificationCoordinatorRequest {
                task_id: fx.task_id.clone(),
                task_run_id: fx.task_run_id.clone(),
                cancellation: CancellationToken::new(),
            },
            &ctx,
        )
        .await;

        // 1. Match only the typed resolver error outcome. The diagnostic text is
        //    asserted for explanation only; production control flow must not
        //    string-match it.
        let detail = match &outcome {
            FinalVerificationRecordingOutcome::Error { detail, .. } => detail.clone(),
            other => panic!(
                "configured NULL-workspace run must fail closed with a resolver error, got {other:?}"
            ),
        };
        assert!(
            detail.contains("task run has no worktree"),
            "resolver diagnostic should explain the missing worktree, got {detail}"
        );

        // 2. No reusable/stored success evidence or finalize payload is derivable.
        let evidence = match &outcome {
            FinalVerificationRecordingOutcome::Stored { evidence, .. }
            | FinalVerificationRecordingOutcome::Reused { evidence, .. } => Some(&**evidence),
            _ => None,
        };
        assert!(
            evidence.is_none(),
            "no success evidence or finalize payload may be derivable from the failed resolution"
        );

        // 3. The terminal test shortcut did not decide the coordinator, and the
        //    real host resolver was traversed exactly once.
        assert_eq!(
            probe.terminal_outcome_shortcuts.load(Ordering::Relaxed),
            0,
            "final_verification_outcome_for_test must not short-circuit this case"
        );
        assert_eq!(
            probe.resolver_calls.load(Ordering::Relaxed),
            1,
            "the production resolver must be traversed exactly once"
        );

        // 4. Zero consultation outcomes: there is no lookup hit or other
        //    synthesized reuse outcome.
        assert!(
            probe.consultation_outcomes.lock().unwrap().is_empty(),
            "no consultation outcome may be emitted before the resolver error"
        );

        // 5. Zero canonical execution requests.
        assert_eq!(
            probe.canonical_execution_requests.load(Ordering::Relaxed),
            0,
            "no canonical execution request may occur after the resolver error"
        );

        // 6. Zero invocation-lease requests and zero acquisitions.
        assert_eq!(
            probe.lease_requests.load(Ordering::Relaxed),
            0,
            "no invocation-lease request may occur after the resolver error"
        );
        assert_eq!(
            probe.lease_acquisitions.load(Ordering::Relaxed),
            0,
            "no invocation-lease acquisition may occur after the resolver error"
        );

        // 7. Durable proof: no stored/success verify-run row survived.
        let stored = VerifyRunRepository::new(fx.agent.db.clone())
            .list_for_task_run(&fx.task_run_id)
            .await
            .expect("verify run list query succeeds");
        assert!(
            stored.is_empty(),
            "VerifyRunRepository::list_for_task_run must remain empty after the failed resolution"
        );

        // 8. The fixture did not accidentally repair the row.
        let run_after = TaskRunRepository::new(fx.agent.db.clone())
            .get(&fx.task_run_id)
            .await
            .expect("task run exists")
            .expect("task run row present");
        assert!(
            run_after.workspace_path.is_none(),
            "the persisted task-run workspace must remain NULL throughout the failed resolution"
        );
    }
}
