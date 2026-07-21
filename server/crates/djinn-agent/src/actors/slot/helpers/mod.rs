use crate::context::AgentContext;
use djinn_core::models::Task;
use djinn_orchestration_types::slot::MergeConflictMetadata;
use djinn_provider::repos::CredentialRepository;

mod feedback;
pub mod provider_resolution;

#[allow(unused_imports)]
pub(crate) use djinn_slot::helpers::{
    KnowledgePackConfig, NotePackDisposition, NotePackOutcome, PackedKnowledgeNotes,
    derive_task_scope_paths, format_knowledge_notes, is_role_auto_code_context_enabled,
    pack_knowledge_notes, pack_ranked_knowledge_notes,
};
#[allow(unused_imports)]
pub(crate) use feedback::{
    COMBINED_BRIEF_SECTION_FLOOR_CHARS, COMBINED_BRIEF_TOTAL_CHARS, budget_combined_sections,
    conflict_context_for_dispatch, default_target_branch, extract_worker_context,
    format_attempt_history, format_command_details, initial_user_message_for_task, load_task,
    parse_conflict_metadata, pr_review_feedback_context, raw_ci_feedback_in_cycle, recent_feedback,
    runtime_env_diagnostics, runtime_fs_diagnostics,
};
pub use provider_resolution::{
    OAuthAuthMethodWire, OAuthCapabilitiesWire, OAuthConfigWire, OAuthFormatFamilyWire,
    ProviderCredential, auth_method_for_provider, capabilities_for_provider, default_base_url,
    format_family_for_provider, load_provider_credential, parse_model_id,
    refresh_oauth_credential_after_401,
};
#[allow(unused_imports)]
pub(crate) use provider_resolution::{
    build_provider_from_resolved, build_restamp_target, build_telemetry_meta_with_attribution,
    resolved_needs_base_url,
};

/// Agent-compatible wrapper around the canonical djinn-slot code graph context helper.
pub(crate) async fn build_role_code_graph_context(
    role_name: &str,
    task: &Task,
    app_state: &AgentContext,
    project_path: &str,
    task_paths: &[String],
) -> Option<String> {
    crate::with_slot_context!(app_state, |slot_ctx| {
        djinn_slot::helpers::build_role_code_graph_context(
            role_name,
            task,
            slot_ctx,
            project_path,
            task_paths,
        )
    })
}

/// Agent-compatible wrapper around the canonical djinn-slot reviewer diff helper.
pub(crate) async fn build_reviewer_diff_context(
    role_name: &str,
    task: &Task,
    app_state: &AgentContext,
    project_path: &str,
    from_sha: Option<&str>,
    to_sha: Option<&str>,
) -> Option<String> {
    crate::with_slot_context!(app_state, |slot_ctx| {
        djinn_slot::helpers::build_reviewer_diff_context(
            role_name,
            task,
            slot_ctx,
            project_path,
            from_sha,
            to_sha,
        )
    })
}
