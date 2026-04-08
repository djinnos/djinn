use rmcp::{Json, handler::server::wrapper::Parameters};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::server::DjinnMcpServer;
use crate::tools::agent_tools::{
    AgentCreateParams, AgentListParams, AgentMetricsParams, AgentShowParams, AgentUpdateParams,
};
use crate::tools::credential_tools::{CredentialDeleteInput, CredentialSetInput};
use crate::tools::epic_tools::{
    EpicCloseParams, EpicCountParams, EpicCreateParams, EpicDeleteParams, EpicListParams,
    EpicReopenParams, EpicShowParams, EpicTasksParams, EpicUpdateParams,
};
use crate::tools::execution_tools::{
    ExecutionKillTaskParams, ExecutionPauseParams, ExecutionResumeParams, ExecutionStartParams,
    ExecutionStatusParams, SessionForTaskParams,
};
use crate::tools::graph_tools::CodeGraphParams;
use crate::tools::memory_tools::{
    AssociationsParams, BrokenLinksParams, BuildContextParams, CatalogParams, DeleteParams,
    DiffParams, EditParams, GraphParams, HealthParams, HistoryParams, ListParams,
    MemoryConfirmParams, MoveParams, OrphansParams, ReadParams, RecentParams, ReindexParams,
    SearchParams, TaskRefsParams, WriteParams,
};
use crate::tools::project_tools::{
    ProjectAddParams, ProjectConfigGetParams, ProjectConfigSetParams, ProjectRemoveParams,
    ProjectSettingsValidateParams,
};
use crate::tools::proposal_tools::{
    ProposeAdrAcceptParams, ProposeAdrListParams, ProposeAdrRejectParams, ProposeAdrShowParams,
};
use crate::tools::provider_tools::{
    ModelHealthInput, ProviderAddCustomInput, ProviderModelLookupInput, ProviderModelsInput,
    ProviderOauthStartInput, ProviderRemoveInput, ProviderValidateInput,
};
use crate::tools::session_tools::{
    SessionActiveParams, SessionListParams, SessionMessagesParams, SessionShowParams,
    TaskTimelineParams,
};
use crate::tools::settings_tools::{SettingsGetParams, SettingsResetParams, SettingsSetParams};
use crate::tools::sync_tools::{
    TaskSyncDisableParams, TaskSyncEnableParams, TaskSyncExportParams, TaskSyncImportParams,
    TaskSyncStatusParams,
};
use crate::tools::system_tools::SystemLogsInput;
use crate::tools::task_tools::{
    BoardHealthParams, BoardReconcileParams, ErrorOr, TaskActivityListParams,
    TaskBlockedListParams, TaskBlockersListParams, TaskClaimParams, TaskCommentAddParams,
    TaskCountParams, TaskCreateParams, TaskListParams, TaskMemoryRefsParams, TaskReadyParams,
    TaskShowParams, TaskTransitionParams, TaskUpdateParams,
};

fn decode_args<T: DeserializeOwned>(tool: &str, args: Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|e| {
        let msg = e.to_string();
        // Surface a clearer hint when acceptance_criteria deserialization fails
        if (tool == "task_create" || tool == "task_update") && msg.contains("acceptance_criter") {
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

impl DjinnMcpServer {
    pub async fn dispatch_tool_with_worktree(
        &self,
        name: &str,
        args: Value,
        worktree_root: Option<std::path::PathBuf>,
    ) -> Result<Value, String> {
        match name {
            "memory_write" => map_json(
                name,
                self.memory_write_with_worktree(
                    Parameters(decode_args::<WriteParams>(name, args)?),
                    worktree_root,
                )
                .await,
            ),
            "memory_edit" => map_json(
                name,
                self.memory_edit_with_worktree(
                    Parameters(decode_args::<EditParams>(name, args)?),
                    worktree_root,
                )
                .await,
            ),
            "memory_delete" => map_json(
                name,
                self.memory_delete_with_worktree(
                    Parameters(decode_args::<DeleteParams>(name, args)?),
                    worktree_root,
                )
                .await,
            ),
            _ => self.dispatch_tool(name, args).await,
        }
    }

    pub async fn dispatch_tool(&self, name: &str, args: Value) -> Result<Value, String> {
        match name {
            "credential_set" => map_json(
                name,
                self.credential_set(Parameters(decode_args::<CredentialSetInput>(name, args)?))
                    .await,
            ),
            "credential_list" => map_json(name, self.credential_list().await),
            "credential_delete" => map_json(
                name,
                self.credential_delete(Parameters(decode_args::<CredentialDeleteInput>(
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
            "execution_start" => map_json(
                name,
                self.execution_start(Parameters(decode_args::<ExecutionStartParams>(name, args)?))
                    .await,
            ),
            "execution_pause" => map_json(
                name,
                self.execution_pause(Parameters(decode_args::<ExecutionPauseParams>(name, args)?))
                    .await,
            ),
            "execution_resume" => map_json(
                name,
                self.execution_resume(Parameters(decode_args::<ExecutionResumeParams>(
                    name, args,
                )?))
                .await,
            ),
            "execution_status" => map_json(
                name,
                self.execution_status(Parameters(decode_args::<ExecutionStatusParams>(
                    name, args,
                )?))
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
            "project_add" => map_json(
                name,
                self.project_add(Parameters(decode_args::<ProjectAddParams>(name, args)?))
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
            "project_settings_validate" => map_json(
                name,
                self.project_settings_validate(Parameters(decode_args::<
                    ProjectSettingsValidateParams,
                >(name, args)?))
                    .await,
            ),
            "propose_adr_list" => map_json(
                name,
                self.propose_adr_list(Parameters(decode_args::<ProposeAdrListParams>(name, args)?))
                    .await,
            ),
            "propose_adr_show" => map_json(
                name,
                self.propose_adr_show(Parameters(decode_args::<ProposeAdrShowParams>(name, args)?))
                    .await,
            ),
            "propose_adr_accept" => map_json(
                name,
                self.propose_adr_accept(Parameters(decode_args::<ProposeAdrAcceptParams>(
                    name, args,
                )?))
                .await,
            ),
            "propose_adr_reject" => map_json(
                name,
                self.propose_adr_reject(Parameters(decode_args::<ProposeAdrRejectParams>(
                    name, args,
                )?))
                .await,
            ),
            "model_health" => map_json(
                name,
                self.model_health(Parameters(decode_args::<ModelHealthInput>(name, args)?))
                    .await,
            ),
            "provider_catalog" => map_json(name, self.provider_catalog().await),
            "provider_connected" => map_json(name, self.provider_connected().await),
            "provider_models" => map_json(
                name,
                self.provider_models(Parameters(decode_args::<ProviderModelsInput>(name, args)?))
                    .await,
            ),
            "provider_models_connected" => map_json(name, self.provider_models_connected().await),
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
            "provider_add_custom" => map_json(
                name,
                self.provider_add_custom(Parameters(decode_args::<ProviderAddCustomInput>(
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
            "task_sync_enable" => map_json(
                name,
                self.task_sync_enable(Parameters(decode_args::<TaskSyncEnableParams>(name, args)?))
                    .await,
            ),
            "task_sync_disable" => map_json(
                name,
                self.task_sync_disable(Parameters(decode_args::<TaskSyncDisableParams>(
                    name, args,
                )?))
                .await,
            ),
            "task_sync_export" => map_json(
                name,
                self.task_sync_export(Parameters(decode_args::<TaskSyncExportParams>(name, args)?))
                    .await,
            ),
            "task_sync_import" => map_json(
                name,
                self.task_sync_import(Parameters(decode_args::<TaskSyncImportParams>(name, args)?))
                    .await,
            ),
            "task_sync_status" => map_json(
                name,
                self.task_sync_status(Parameters(decode_args::<TaskSyncStatusParams>(name, args)?))
                    .await,
            ),
            "system_ping" => map_json(name, self.system_ping().await),
            "system_logs" => map_json(
                name,
                self.system_logs(Parameters(decode_args::<SystemLogsInput>(name, args)?))
                    .await,
            ),
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
            "memory_reindex" => map_json(
                name,
                self.memory_reindex(Parameters(decode_args::<ReindexParams>(name, args)?))
                    .await,
            ),
            "memory_build_context" => map_json(
                name,
                self.memory_build_context(Parameters(decode_args::<BuildContextParams>(
                    name, args,
                )?))
                .await,
            ),
            "memory_write" => map_json(
                name,
                self.memory_write(Parameters(decode_args::<WriteParams>(name, args)?))
                    .await,
            ),
            "memory_edit" => map_json(
                name,
                self.memory_edit(Parameters(decode_args::<EditParams>(name, args)?))
                    .await,
            ),
            "memory_delete" => map_json(
                name,
                self.memory_delete(Parameters(decode_args::<DeleteParams>(name, args)?))
                    .await,
            ),
            "memory_move" => map_json(
                name,
                self.memory_move(Parameters(decode_args::<MoveParams>(name, args)?))
                    .await,
            ),
            "memory_associations" => map_json(
                name,
                self.memory_associations(Parameters(decode_args::<AssociationsParams>(
                    name, args,
                )?))
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
            _ => Err(format!("unknown MCP tool: '{name}'")),
        }
    }
}
