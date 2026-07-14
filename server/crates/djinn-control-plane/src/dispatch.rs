use rmcp::{Json, handler::server::wrapper::Parameters};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::server::DjinnMcpServer;
use crate::tools::agent_tools::{
    AgentCreateParams, AgentListParams, AgentMetricsParams, AgentShowParams, AgentUpdateParams,
};
use crate::tools::credential_tools::{
    CredentialDeleteInput, CredentialListInput, CredentialSetInput,
};
use crate::tools::debate_tools::{
    ProposalDebateAppendParams, ProposalDebateListParams, ProposalDebateReopenParams,
    ProposalDebateResolveParams,
};
use crate::tools::dispatch_pause_tools::{
    DispatchPauseParams, DispatchPauseStatusParams, DispatchResumeParams,
};
use crate::tools::doctor_tools::{DoctorFixParams, DoctorListFindingsParams, DoctorRunParams};
use crate::tools::epic_tools::{
    EpicBlockersParams, EpicCloseParams, EpicCountParams, EpicCreateParams, EpicDeleteParams,
    EpicListParams, EpicReadSourceParams, EpicReopenParams, EpicShowParams, EpicTasksParams,
    EpicUpdateParams,
};
use crate::tools::execution_tools::{ExecutionKillTaskParams, SessionForTaskParams};
use crate::tools::github_app_tools::{GithubAppInstallUrlParams, GithubAppInstallationsParams};
use crate::tools::github_tools::{GithubFetchFileParams, GithubSearchParams};
use crate::tools::graph_tools::CodeGraphParams;
use crate::tools::image_tools::{
    ImageCreateParams, ImageDeleteParams, ImageListParams, ImageSetServicesParams,
    ImageUpdateParams, ProjectSetImageParams, ToolchainVersionsParams,
};
use crate::tools::memory_tools::{
    AssociationsParams, BrokenLinksParams, BuildContextParams, CatalogParams, DeleteParams,
    DiffParams, EditParams, ExtractedAuditParams, GraphParams, HealthParams, HistoryParams,
    ListParams, MemoryConfirmParams, MoveParams, OrphansParams, ReadParams, RecallTraceParams,
    RecentParams, RepairEmbeddingsParams, RunEnrichmentParams, SearchParams, TaskRefsParams,
    WriteParams,
};
use crate::tools::org_policy_tools::{OrgPolicyGetParams, OrgPolicySetParams};
use crate::tools::pr_review_tools::PrReviewContextParams;
use crate::tools::project_tools::{
    GetProjectDevcontainerStatusParams, GetProjectStackParams, GithubListReposParams,
    ProjectAddFromGithubParams, ProjectBranchesParams, ProjectConfigGetParams,
    ProjectConfigSetParams, ProjectEnvironmentConfigGetParams, ProjectEnvironmentConfigResetParams,
    ProjectEnvironmentConfigSetParams, ProjectGraphExclusionsGetParams,
    ProjectGraphExclusionsSetParams, ProjectRemoveParams, RetriggerImageBuildParams,
};
use crate::tools::proposal_blocks::{GetBlockCatalogParams, ProposalBlocksParams};
use crate::tools::proposal_tools::{
    ProposalBlockPatchParams, ProposalCreateParams, ProposalDeleteParams, ProposalExportParams,
    ProposalFeedbackAddParams, ProposalFeedbackResolveParams, ProposalGraduateParams,
    ProposalImportParams, ProposalListParams, ProposalReconcileObsoleteEpicParams,
    ProposalShowParams, ProposalSignoffParams, ProposalStopBuildParams, ProposalTargetParams,
    ProposalUpdateParams,
};
use crate::tools::provider_tools::{
    ModelHealthInput, ProviderCatalogInput, ProviderConnectedInput, ProviderModelLookupInput,
    ProviderModelsConnectedInput, ProviderModelsInput, ProviderOauthStartInput,
    ProviderRemoveInput, ProviderValidateInput,
};
use crate::tools::refinement_tools::{
    ProposalRefinementDemandEvidenceParams, ProposalRefinementDemandRoundParams,
    ProposalRefinementResolveParams, ProposalRefinementStartParams, ProposalRefinementStatusParams,
    ProposalVerdictOverrideParams,
};
use crate::tools::service_tools::ServicePresetListParams;
use crate::tools::session_tools::{
    SessionActiveParams, SessionListParams, SessionMessagesParams, SessionShowParams,
    TaskTimelineParams,
};
use crate::tools::settings_tools::{SettingsGetParams, SettingsResetParams, SettingsSetParams};
use crate::tools::task_tools::{
    BoardHealthParams, BoardReconcileParams, ErrorOr, TaskActivityListParams,
    TaskBlockedListParams, TaskBlockersListParams, TaskClaimParams, TaskCommentAddParams,
    TaskCountParams, TaskCreateParams, TaskListParams, TaskMemoryRefsParams, TaskReadyParams,
    TaskShowParams, TaskTransitionParams, TaskUpdateParams,
};
use crate::tools::tool_error::ToolOutcome;
use crate::tools::user_settings_tools::{UserSettingsGetParams, UserSettingsSetParams};

fn decode_args<T: DeserializeOwned>(tool: &str, args: Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|e| {
        let msg = e.to_string();
        // Omitted and null reasons fail schema decoding before the mutation
        // handler can apply its non-blank validation. Keep that boundary
        // envelope identical to the handler's whitespace rejection.
        if matches!(tool, "memory_write" | "memory_edit" | "memory_delete")
            && msg.contains("reason")
        {
            "invalid parameters: field: reason, message: reason must be non-blank".to_owned()
        } else if (tool == "task_create" || tool == "task_update")
            && msg.contains("acceptance_criter")
        {
            format!(
                "invalid arguments for tool '{tool}': {msg}. \
                 Hint: acceptance_criteria must be an array of strings, \
                 e.g. [\"criterion 1\", \"criterion 2\"]"
            )
        } else {
            format!("invalid arguments for tool '{tool}': {msg}")
        }
    })
}

fn map_error_or<T: Serialize>(tool: &str, out: Json<ErrorOr<T>>) -> Result<Value, String> {
    match out.0 {
        ErrorOr::Ok(v) => serde_json::to_value(v)
            .map_err(|e| format!("failed to serialize tool result for '{tool}': {e}")),
        ErrorOr::Error(e) => Err(format!("tool '{tool}' failed: {}", e.error)),
    }
}

fn map_json<T: Serialize>(tool: &str, out: Json<T>) -> Result<Value, String> {
    serde_json::to_value(out.0)
        .map_err(|e| format!("failed to serialize tool result for '{tool}': {e}"))
}

/// Dispatch mapper for memory mutation tools (`memory_write`, `memory_edit`,
/// `memory_delete`). These handlers embed failures as a non-null `error` field
/// inside the success JSON (`MemoryNoteResponse` / `MemoryDeleteResponse`).
/// Propagate that as a dispatch `Err` so the dispatch boundary uniformly rejects
/// all invalid reasons and other handler-level failures before the caller can
/// treat the result as a successful mutation. This matches the AC intent that
/// rejection occurs without durable mutation — an `Err` guarantees the caller
/// never sees a note-shaped payload on a rejected write.
fn map_memory_mutation<T: Serialize>(tool: &str, out: Json<T>) -> Result<Value, String> {
    let value = serde_json::to_value(&out.0)
        .map_err(|e| format!("failed to serialize tool result for '{tool}': {e}"))?;
    if let Some(error) = value.get("error").and_then(|v| v.as_str())
        && !error.is_empty()
    {
        return Err(format!("tool '{tool}' failed: {error}"));
    }
    Ok(value)
}

/// Map a G3 structured-error `ToolOutcome<T>` to the dispatch `Result`. On the
/// error arm we serialize the full [`ToolError`] envelope to JSON so the
/// structure (`status`/`method`/`path`/`body`/`hint`) survives the flattened
/// `Err(String)` channel and reaches the agent — not just the human message.
fn map_tool_outcome<T: Serialize>(tool: &str, out: Json<ToolOutcome<T>>) -> Result<Value, String> {
    match out.0 {
        ToolOutcome::Ok(v) => serde_json::to_value(v)
            .map_err(|e| format!("failed to serialize tool result for '{tool}': {e}")),
        ToolOutcome::Err(e) => Err(serde_json::to_string(&e)
            .unwrap_or_else(|_| format!("tool '{tool}' failed: {}", e.error))),
    }
}

impl DjinnMcpServer {
    /// Compatibility entry point retained for callers that still pass a
    /// worktree_root through `dispatch_tool_with_worktree`. The
    /// worktree-scoped variants of memory_write/edit/delete/move are gone
    /// since notes are db-only; the path is ignored and the call is
    /// forwarded to the canonical dispatch.
    pub async fn dispatch_tool_with_worktree(
        &self,
        name: &str,
        args: Value,
        _worktree_root: Option<std::path::PathBuf>,
    ) -> Result<Value, String> {
        self.dispatch_tool(name, args).await
    }

    pub async fn dispatch_tool(&self, name: &str, args: Value) -> Result<Value, String> {
        match name {
            "credential_set" => map_json(
                name,
                self.credential_set(Parameters(decode_args::<CredentialSetInput>(name, args)?))
                    .await,
            ),
            "credential_list" => map_json(
                name,
                self.credential_list(Parameters(decode_args::<CredentialListInput>(name, args)?))
                    .await,
            ),
            "credential_delete" => map_json(
                name,
                self.credential_delete(Parameters(decode_args::<CredentialDeleteInput>(
                    name, args,
                )?))
                .await,
            ),
            "dispatch_pause" => map_json(
                name,
                self.dispatch_pause(Parameters(decode_args::<DispatchPauseParams>(name, args)?))
                    .await,
            ),
            "dispatch_resume" => map_json(
                name,
                self.dispatch_resume(Parameters(decode_args::<DispatchResumeParams>(name, args)?))
                    .await,
            ),
            "dispatch_pause_status" => map_json(
                name,
                self.dispatch_pause_status(Parameters(decode_args::<DispatchPauseStatusParams>(
                    name, args,
                )?))
                .await,
            ),
            "doctor_run" => map_json(
                name,
                self.doctor_run(Parameters(decode_args::<DoctorRunParams>(name, args)?))
                    .await,
            ),
            "doctor_fix" => map_json(
                name,
                self.doctor_fix(Parameters(decode_args::<DoctorFixParams>(name, args)?))
                    .await,
            ),
            "doctor_list_findings" => map_json(
                name,
                self.doctor_list_findings(Parameters(decode_args::<DoctorListFindingsParams>(
                    name, args,
                )?))
                .await,
            ),
            "epic_create" => map_json(
                name,
                self.epic_create(Parameters(decode_args::<EpicCreateParams>(name, args)?))
                    .await,
            ),
            "epic_show" => map_json(
                name,
                self.epic_show(Parameters(decode_args::<EpicShowParams>(name, args)?))
                    .await,
            ),
            "epic_list" => map_json(
                name,
                self.epic_list(Parameters(decode_args::<EpicListParams>(name, args)?))
                    .await,
            ),
            "epic_update" => map_json(
                name,
                self.epic_update(Parameters(decode_args::<EpicUpdateParams>(name, args)?))
                    .await,
            ),
            "epic_close" => map_json(
                name,
                self.epic_close(Parameters(decode_args::<EpicCloseParams>(name, args)?))
                    .await,
            ),
            "epic_reopen" => map_json(
                name,
                self.epic_reopen(Parameters(decode_args::<EpicReopenParams>(name, args)?))
                    .await,
            ),
            "epic_delete" => map_json(
                name,
                self.epic_delete(Parameters(decode_args::<EpicDeleteParams>(name, args)?))
                    .await,
            ),
            "epic_blockers_list" => map_json(
                name,
                self.epic_blockers_list(Parameters(decode_args::<EpicBlockersParams>(name, args)?))
                    .await,
            ),
            "epic_blocked_list" => map_json(
                name,
                self.epic_blocked_list(Parameters(decode_args::<EpicBlockersParams>(name, args)?))
                    .await,
            ),
            "epic_tasks" => map_json(
                name,
                self.epic_tasks(Parameters(decode_args::<EpicTasksParams>(name, args)?))
                    .await,
            ),
            "epic_count" => map_json(
                name,
                self.epic_count(Parameters(decode_args::<EpicCountParams>(name, args)?))
                    .await,
            ),
            "epic_add_read_source" => map_json(
                name,
                self.epic_add_read_source(Parameters(decode_args::<EpicReadSourceParams>(
                    name, args,
                )?))
                .await,
            ),
            "epic_remove_read_source" => map_json(
                name,
                self.epic_remove_read_source(Parameters(decode_args::<EpicReadSourceParams>(
                    name, args,
                )?))
                .await,
            ),
            "epic_list_read_sources" => map_json(
                name,
                self.epic_list_read_sources(Parameters(decode_args::<EpicShowParams>(name, args)?))
                    .await,
            ),
            "execution_kill_task" => map_json(
                name,
                self.execution_kill_task(Parameters(decode_args::<ExecutionKillTaskParams>(
                    name, args,
                )?))
                .await,
            ),
            "session_for_task" => map_json(
                name,
                self.session_for_task(Parameters(decode_args::<SessionForTaskParams>(name, args)?))
                    .await,
            ),
            "project_remove" => map_json(
                name,
                self.project_remove(Parameters(decode_args::<ProjectRemoveParams>(name, args)?))
                    .await,
            ),
            "project_list" => map_json(name, self.project_list().await),
            "project_config_get" => map_json(
                name,
                self.project_config_get(Parameters(decode_args::<ProjectConfigGetParams>(
                    name, args,
                )?))
                .await,
            ),
            "project_config_set" => map_json(
                name,
                self.project_config_set(Parameters(decode_args::<ProjectConfigSetParams>(
                    name, args,
                )?))
                .await,
            ),
            "project_graph_exclusions_get" => map_json(
                name,
                self.project_graph_exclusions_get(Parameters(decode_args::<
                    ProjectGraphExclusionsGetParams,
                >(name, args)?))
                    .await,
            ),
            "project_graph_exclusions_set" => map_json(
                name,
                self.project_graph_exclusions_set(Parameters(decode_args::<
                    ProjectGraphExclusionsSetParams,
                >(name, args)?))
                    .await,
            ),
            "get_project_stack" => map_json(
                name,
                self.get_project_stack(Parameters(decode_args::<GetProjectStackParams>(
                    name, args,
                )?))
                .await,
            ),
            "get_project_devcontainer_status" => map_json(
                name,
                self.get_project_devcontainer_status(Parameters(decode_args::<
                    GetProjectDevcontainerStatusParams,
                >(name, args)?))
                    .await,
            ),
            "retrigger_image_build" => map_json(
                name,
                self.retrigger_image_build(Parameters(decode_args::<RetriggerImageBuildParams>(
                    name, args,
                )?))
                .await,
            ),
            "project_environment_config_get" => map_json(
                name,
                self.project_environment_config_get(Parameters(decode_args::<
                    ProjectEnvironmentConfigGetParams,
                >(name, args)?))
                    .await,
            ),
            "project_environment_config_set" => map_json(
                name,
                self.project_environment_config_set(Parameters(decode_args::<
                    ProjectEnvironmentConfigSetParams,
                >(name, args)?))
                    .await,
            ),
            "project_environment_config_reset" => map_json(
                name,
                self.project_environment_config_reset(Parameters(decode_args::<
                    ProjectEnvironmentConfigResetParams,
                >(name, args)?))
                    .await,
            ),
            "toolchain_versions" => map_json(
                name,
                self.toolchain_versions(Parameters(decode_args::<ToolchainVersionsParams>(
                    name, args,
                )?))
                .await,
            ),
            "image_list" => map_json(
                name,
                self.image_list(Parameters(decode_args::<ImageListParams>(name, args)?))
                    .await,
            ),
            "image_create" => map_json(
                name,
                self.image_create(Parameters(decode_args::<ImageCreateParams>(name, args)?))
                    .await,
            ),
            "image_update" => map_json(
                name,
                self.image_update(Parameters(decode_args::<ImageUpdateParams>(name, args)?))
                    .await,
            ),
            "image_delete" => map_json(
                name,
                self.image_delete(Parameters(decode_args::<ImageDeleteParams>(name, args)?))
                    .await,
            ),
            "project_set_image" => map_json(
                name,
                self.project_set_image(Parameters(decode_args::<ProjectSetImageParams>(
                    name, args,
                )?))
                .await,
            ),
            "image_set_services" => map_json(
                name,
                self.image_set_services(Parameters(decode_args::<ImageSetServicesParams>(
                    name, args,
                )?))
                .await,
            ),
            "service_preset_list" => map_json(
                name,
                self.service_preset_list(Parameters(decode_args::<ServicePresetListParams>(
                    name, args,
                )?))
                .await,
            ),
            "proposal_create" => map_json(
                name,
                self.proposal_create(Parameters(decode_args::<ProposalCreateParams>(name, args)?))
                    .await,
            ),
            "proposal_import" => map_json(
                name,
                self.proposal_import(Parameters(decode_args::<ProposalImportParams>(name, args)?))
                    .await,
            ),
            "proposal_export" => map_json(
                name,
                self.proposal_export(Parameters(decode_args::<ProposalExportParams>(name, args)?))
                    .await,
            ),
            "proposal_blocks" => map_json(
                name,
                self.proposal_blocks(Parameters(decode_args::<ProposalBlocksParams>(name, args)?))
                    .await,
            ),
            "get_block_catalog" => map_json(
                name,
                self.get_block_catalog(Parameters(decode_args::<GetBlockCatalogParams>(
                    name, args,
                )?))
                .await,
            ),
            "proposal_show" => map_json(
                name,
                self.proposal_show(Parameters(decode_args::<ProposalShowParams>(name, args)?))
                    .await,
            ),
            "proposal_list" => map_json(
                name,
                self.proposal_list(Parameters(decode_args::<ProposalListParams>(name, args)?))
                    .await,
            ),
            "proposal_update" => map_json(
                name,
                self.proposal_update(Parameters(decode_args::<ProposalUpdateParams>(name, args)?))
                    .await,
            ),
            "proposal_block_patch" => map_json(
                name,
                self.proposal_block_patch(Parameters(decode_args::<ProposalBlockPatchParams>(
                    name, args,
                )?))
                .await,
            ),
            "proposal_delete" => map_json(
                name,
                self.proposal_delete(Parameters(decode_args::<ProposalDeleteParams>(name, args)?))
                    .await,
            ),
            "proposal_add_target" => map_json(
                name,
                self.proposal_add_target(Parameters(decode_args::<ProposalTargetParams>(
                    name, args,
                )?))
                .await,
            ),
            "proposal_remove_target" => map_json(
                name,
                self.proposal_remove_target(Parameters(decode_args::<ProposalTargetParams>(
                    name, args,
                )?))
                .await,
            ),
            "proposal_feedback_add" => map_json(
                name,
                self.proposal_feedback_add(Parameters(decode_args::<ProposalFeedbackAddParams>(
                    name, args,
                )?))
                .await,
            ),
            "proposal_feedback_resolve" => map_json(
                name,
                self.proposal_feedback_resolve(Parameters(decode_args::<
                    ProposalFeedbackResolveParams,
                >(name, args)?))
                    .await,
            ),
            "proposal_graduate" => map_json(
                name,
                self.proposal_graduate(Parameters(decode_args::<ProposalGraduateParams>(
                    name, args,
                )?))
                .await,
            ),
            "proposal_stop_build" => map_json(
                name,
                self.proposal_stop_build(Parameters(decode_args::<ProposalStopBuildParams>(
                    name, args,
                )?))
                .await,
            ),
            "proposal_reconcile_obsolete_epic" => map_json(
                name,
                self.proposal_reconcile_obsolete_epic(Parameters(decode_args::<
                    ProposalReconcileObsoleteEpicParams,
                >(name, args)?))
                    .await,
            ),
            "proposal_signoff" => map_json(
                name,
                self.proposal_signoff(Parameters(decode_args::<ProposalSignoffParams>(
                    name, args,
                )?))
                .await,
            ),
            "proposal_signoff_clear" => map_json(
                name,
                self.proposal_signoff_clear(Parameters(decode_args::<ProposalSignoffParams>(
                    name, args,
                )?))
                .await,
            ),
            "proposal_debate_append" => map_json(
                name,
                self.proposal_debate_append(Parameters(decode_args::<ProposalDebateAppendParams>(
                    name, args,
                )?))
                .await,
            ),
            "proposal_debate_list" => map_json(
                name,
                self.proposal_debate_list(Parameters(decode_args::<ProposalDebateListParams>(
                    name, args,
                )?))
                .await,
            ),
            "proposal_debate_resolve" => map_json(
                name,
                self.proposal_debate_resolve(Parameters(
                    decode_args::<ProposalDebateResolveParams>(name, args)?,
                ))
                .await,
            ),
            "proposal_debate_reopen" => map_json(
                name,
                self.proposal_debate_reopen(Parameters(decode_args::<ProposalDebateReopenParams>(
                    name, args,
                )?))
                .await,
            ),
            "proposal_refinement_start" => map_json(
                name,
                self.proposal_refinement_start(Parameters(decode_args::<
                    ProposalRefinementStartParams,
                >(name, args)?))
                    .await,
            ),
            "proposal_refinement_status" => map_json(
                name,
                self.proposal_refinement_status(Parameters(decode_args::<
                    ProposalRefinementStatusParams,
                >(name, args)?))
                    .await,
            ),
            "proposal_refinement_demand_round" => map_json(
                name,
                self.proposal_refinement_demand_round(Parameters(decode_args::<
                    ProposalRefinementDemandRoundParams,
                >(name, args)?))
                    .await,
            ),
            "proposal_refinement_resolve" => map_json(
                name,
                self.proposal_refinement_resolve(Parameters(decode_args::<
                    ProposalRefinementResolveParams,
                >(name, args)?))
                    .await,
            ),
            "proposal_verdict_override" => map_json(
                name,
                self.proposal_verdict_override(Parameters(decode_args::<
                    ProposalVerdictOverrideParams,
                >(name, args)?))
                    .await,
            ),
            "proposal_refinement_demand_evidence" => map_json(
                name,
                self.proposal_refinement_demand_evidence(Parameters(decode_args::<
                    ProposalRefinementDemandEvidenceParams,
                >(name, args)?))
                    .await,
            ),
            "model_health" => map_json(
                name,
                self.model_health(Parameters(decode_args::<ModelHealthInput>(name, args)?))
                    .await,
            ),
            "provider_catalog" => map_json(
                name,
                self.provider_catalog(Parameters(decode_args::<ProviderCatalogInput>(name, args)?))
                    .await,
            ),
            "provider_connected" => map_json(
                name,
                self.provider_connected(Parameters(decode_args::<ProviderConnectedInput>(
                    name, args,
                )?))
                .await,
            ),
            "provider_models" => map_json(
                name,
                self.provider_models(Parameters(decode_args::<ProviderModelsInput>(name, args)?))
                    .await,
            ),
            "provider_models_connected" => map_json(
                name,
                self.provider_models_connected(Parameters(decode_args::<
                    ProviderModelsConnectedInput,
                >(name, args)?))
                    .await,
            ),
            "provider_oauth_start" => map_json(
                name,
                self.provider_oauth_start(Parameters(decode_args::<ProviderOauthStartInput>(
                    name, args,
                )?))
                .await,
            ),
            "provider_model_lookup" => map_json(
                name,
                self.provider_model_lookup(Parameters(decode_args::<ProviderModelLookupInput>(
                    name, args,
                )?))
                .await,
            ),
            "provider_validate" => map_json(
                name,
                self.provider_validate(Parameters(decode_args::<ProviderValidateInput>(
                    name, args,
                )?))
                .await,
            ),
            "provider_remove" => map_json(
                name,
                self.provider_remove(Parameters(decode_args::<ProviderRemoveInput>(name, args)?))
                    .await,
            ),
            "settings_get" => map_json(
                name,
                self.settings_get(Parameters(decode_args::<SettingsGetParams>(name, args)?))
                    .await,
            ),
            "settings_set" => map_json(
                name,
                self.settings_set(Parameters(decode_args::<SettingsSetParams>(name, args)?))
                    .await,
            ),
            "settings_reset" => map_json(
                name,
                self.settings_reset(Parameters(decode_args::<SettingsResetParams>(name, args)?))
                    .await,
            ),
            "user_settings_get" => map_json(
                name,
                self.user_settings_get(Parameters(decode_args::<UserSettingsGetParams>(
                    name, args,
                )?))
                .await,
            ),
            "user_settings_set" => map_json(
                name,
                self.user_settings_set(Parameters(decode_args::<UserSettingsSetParams>(
                    name, args,
                )?))
                .await,
            ),
            "org_policy_get" => map_json(
                name,
                self.org_policy_get(Parameters(decode_args::<OrgPolicyGetParams>(name, args)?))
                    .await,
            ),
            "org_policy_set" => map_json(
                name,
                self.org_policy_set(Parameters(decode_args::<OrgPolicySetParams>(name, args)?))
                    .await,
            ),
            "system_ping" => map_json(name, self.system_ping().await),
            "memory_read" => map_json(
                name,
                self.memory_read(Parameters(decode_args::<ReadParams>(name, args)?))
                    .await,
            ),
            "memory_confirm" => map_json(
                name,
                self.memory_confirm(Parameters(decode_args::<MemoryConfirmParams>(name, args)?))
                    .await,
            ),
            "memory_list" => map_json(
                name,
                self.memory_list(Parameters(decode_args::<ListParams>(name, args)?))
                    .await,
            ),
            "memory_catalog" => map_json(
                name,
                self.memory_catalog(Parameters(decode_args::<CatalogParams>(name, args)?))
                    .await,
            ),
            "memory_health" => map_json(
                name,
                self.memory_health(Parameters(decode_args::<HealthParams>(name, args)?))
                    .await,
            ),
            "memory_extracted_audit" => map_json(
                name,
                self.memory_extracted_audit(Parameters(decode_args::<ExtractedAuditParams>(
                    name, args,
                )?))
                .await,
            ),
            "memory_recent" => map_json(
                name,
                self.memory_recent(Parameters(decode_args::<RecentParams>(name, args)?))
                    .await,
            ),
            "memory_history" => map_json(
                name,
                self.memory_history(Parameters(decode_args::<HistoryParams>(name, args)?))
                    .await,
            ),
            "memory_task_refs" => map_json(
                name,
                self.memory_task_refs(Parameters(decode_args::<TaskRefsParams>(name, args)?))
                    .await,
            ),
            "memory_broken_links" => map_json(
                name,
                self.memory_broken_links(Parameters(decode_args::<BrokenLinksParams>(name, args)?))
                    .await,
            ),
            "memory_orphans" => map_json(
                name,
                self.memory_orphans(Parameters(decode_args::<OrphansParams>(name, args)?))
                    .await,
            ),
            "memory_search" => map_json(
                name,
                self.memory_search(Parameters(decode_args::<SearchParams>(name, args)?))
                    .await,
            ),
            "memory_graph" => map_json(
                name,
                self.memory_graph(Parameters(decode_args::<GraphParams>(name, args)?))
                    .await,
            ),
            "memory_diff" => map_json(
                name,
                self.memory_diff(Parameters(decode_args::<DiffParams>(name, args)?))
                    .await,
            ),
            "memory_build_context" => map_json(
                name,
                self.memory_build_context(Parameters(decode_args::<BuildContextParams>(
                    name, args,
                )?))
                .await,
            ),
            "memory_write" => map_memory_mutation(
                name,
                self.memory_write(Parameters(decode_args::<WriteParams>(name, args)?))
                    .await,
            ),
            "memory_edit" => map_memory_mutation(
                name,
                self.memory_edit(Parameters(decode_args::<EditParams>(name, args)?))
                    .await,
            ),
            "memory_delete" => map_memory_mutation(
                name,
                self.memory_delete(Parameters(decode_args::<DeleteParams>(name, args)?))
                    .await,
            ),
            "memory_move" => map_json(
                name,
                self.memory_move(Parameters(decode_args::<MoveParams>(name, args)?))
                    .await,
            ),
            "memory_repair_embeddings" => map_json(
                name,
                self.memory_repair_embeddings(Parameters(decode_args::<RepairEmbeddingsParams>(
                    name, args,
                )?))
                .await,
            ),
            "memory_run_enrichment" => map_json(
                name,
                self.memory_run_enrichment(Parameters(decode_args::<RunEnrichmentParams>(
                    name, args,
                )?))
                .await,
            ),
            "memory_associations" => map_json(
                name,
                self.memory_associations(Parameters(decode_args::<AssociationsParams>(
                    name, args,
                )?))
                .await,
            ),
            "memory_recall_trace" => map_json(
                name,
                self.memory_recall_trace(Parameters(decode_args::<RecallTraceParams>(name, args)?))
                    .await,
            ),
            "session_list" => map_json(
                name,
                self.session_list(Parameters(decode_args::<SessionListParams>(name, args)?))
                    .await,
            ),
            "session_active" => map_json(
                name,
                self.session_active(Parameters(decode_args::<SessionActiveParams>(name, args)?))
                    .await,
            ),
            "session_show" => map_json(
                name,
                self.session_show(Parameters(decode_args::<SessionShowParams>(name, args)?))
                    .await,
            ),
            "session_messages" => map_json(
                name,
                self.session_messages(Parameters(decode_args::<SessionMessagesParams>(
                    name, args,
                )?))
                .await,
            ),
            "task_timeline" => map_json(
                name,
                self.task_timeline(Parameters(decode_args::<TaskTimelineParams>(name, args)?))
                    .await,
            ),
            "task_create" => map_error_or(
                name,
                self.task_create(Parameters(decode_args::<TaskCreateParams>(name, args)?))
                    .await,
            ),
            "task_update" => map_error_or(
                name,
                self.task_update(Parameters(decode_args::<TaskUpdateParams>(name, args)?))
                    .await,
            ),
            "task_show" => map_error_or(
                name,
                self.task_show(Parameters(decode_args::<TaskShowParams>(name, args)?))
                    .await,
            ),
            "task_list" => map_json(
                name,
                self.task_list(Parameters(decode_args::<TaskListParams>(name, args)?))
                    .await,
            ),
            "task_count" => map_json(
                name,
                self.task_count(Parameters(decode_args::<TaskCountParams>(name, args)?))
                    .await,
            ),
            "task_blockers_list" => map_error_or(
                name,
                self.task_blockers_list(Parameters(decode_args::<TaskBlockersListParams>(
                    name, args,
                )?))
                .await,
            ),
            "task_blocked_list" => map_error_or(
                name,
                self.task_blocked_list(Parameters(decode_args::<TaskBlockedListParams>(
                    name, args,
                )?))
                .await,
            ),
            "task_ready" => map_error_or(
                name,
                self.task_ready(Parameters(decode_args::<TaskReadyParams>(name, args)?))
                    .await,
            ),
            "task_transition" => map_error_or(
                name,
                self.task_transition(Parameters(decode_args::<TaskTransitionParams>(name, args)?))
                    .await,
            ),
            "task_claim" => map_error_or(
                name,
                self.task_claim(Parameters(decode_args::<TaskClaimParams>(name, args)?))
                    .await,
            ),
            "task_comment_add" => map_error_or(
                name,
                self.task_comment_add(Parameters(decode_args::<TaskCommentAddParams>(name, args)?))
                    .await,
            ),
            "task_activity_list" => map_error_or(
                name,
                self.task_activity_list(Parameters(decode_args::<TaskActivityListParams>(
                    name, args,
                )?))
                .await,
            ),
            "board_health" => map_error_or(
                name,
                self.board_health(Parameters(decode_args::<BoardHealthParams>(name, args)?))
                    .await,
            ),
            "board_reconcile" => map_error_or(
                name,
                self.board_reconcile(Parameters(decode_args::<BoardReconcileParams>(name, args)?))
                    .await,
            ),
            "task_memory_refs" => map_error_or(
                name,
                self.task_memory_refs(Parameters(decode_args::<TaskMemoryRefsParams>(name, args)?))
                    .await,
            ),
            "agent_create" => map_json(
                name,
                self.agent_create(Parameters(decode_args::<AgentCreateParams>(name, args)?))
                    .await,
            ),
            "agent_show" => map_json(
                name,
                self.agent_show(Parameters(decode_args::<AgentShowParams>(name, args)?))
                    .await,
            ),
            "agent_list" => map_json(
                name,
                self.agent_list(Parameters(decode_args::<AgentListParams>(name, args)?))
                    .await,
            ),
            "agent_update" => map_json(
                name,
                self.agent_update(Parameters(decode_args::<AgentUpdateParams>(name, args)?))
                    .await,
            ),
            "agent_metrics" => map_json(
                name,
                self.agent_metrics(Parameters(decode_args::<AgentMetricsParams>(name, args)?))
                    .await,
            ),
            "code_graph" => map_error_or(
                name,
                self.code_graph(Parameters(decode_args::<CodeGraphParams>(name, args)?))
                    .await,
            ),
            "pr_review_context" => map_error_or(
                name,
                self.pr_review_context(Parameters(decode_args::<PrReviewContextParams>(
                    name, args,
                )?))
                .await,
            ),
            "github_search" => map_tool_outcome(
                name,
                self.github_search(Parameters(decode_args::<GithubSearchParams>(name, args)?))
                    .await,
            ),
            "github_fetch_file" => map_tool_outcome(
                name,
                self.github_fetch_file(Parameters(decode_args::<GithubFetchFileParams>(
                    name, args,
                )?))
                .await,
            ),
            "github_app_installations" => map_json(
                name,
                self.github_app_installations(Parameters(decode_args::<
                    GithubAppInstallationsParams,
                >(name, args)?))
                    .await,
            ),
            "github_app_install_url" => map_json(
                name,
                self.github_app_install_url(Parameters(decode_args::<GithubAppInstallUrlParams>(
                    name, args,
                )?))
                .await,
            ),
            "github_list_repos" => map_json(
                name,
                self.github_list_repos(Parameters(decode_args::<GithubListReposParams>(
                    name, args,
                )?))
                .await,
            ),
            "project_add_from_github" => map_json(
                name,
                self.project_add_from_github(Parameters(
                    decode_args::<ProjectAddFromGithubParams>(name, args)?,
                ))
                .await,
            ),
            "project_branches" => map_json(
                name,
                self.project_branches(Parameters(decode_args::<ProjectBranchesParams>(
                    name, args,
                )?))
                .await,
            ),
            _ => Err(format!("unknown MCP tool: '{name}'")),
        }
    }
}
