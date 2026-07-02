use crate::context::AgentContext;
use djinn_core::models::Task;
use djinn_orchestration_types::slot::MergeConflictMetadata;
use djinn_provider::repos::CredentialRepository;

mod feedback;
pub mod provider_resolution;

// Re-export context-free code-context helpers directly from the canonical
// djinn-slot implementation. The remaining items are agent-specific helpers
// that cannot be shared without leaking host-only types.
#[allow(unused_imports)]
pub(crate) use djinn_slot::helpers::{
    derive_task_scope_paths, format_knowledge_notes, is_role_auto_code_context_enabled,
};
#[allow(unused_imports)]
// Thin adapter layer: context-free functions re-exported from djinn_slot,
// context-dependent async functions wrapped with AgentContext → SlotContext.
pub(crate) use feedback::{
    COMBINED_BRIEF_SECTION_FLOOR_CHARS, COMBINED_BRIEF_TOTAL_CHARS, budget_combined_sections,
    conflict_context_for_dispatch, default_target_branch, extract_worker_context,
    format_command_details, initial_user_message_for_task, load_task, parse_conflict_metadata,
    pr_review_feedback_context, raw_ci_feedback_in_cycle, recent_feedback, runtime_env_diagnostics,
    runtime_fs_diagnostics,
};
pub use provider_resolution::{
    OAuthAuthMethodWire, OAuthCapabilitiesWire, OAuthConfigWire, OAuthFormatFamilyWire,
    ProviderCredential, auth_method_for_provider, capabilities_for_provider, default_base_url,
    format_family_for_provider, load_provider_credential, parse_model_id,
    refresh_oauth_credential_after_401,
};
#[allow(unused_imports)]
pub(crate) use provider_resolution::{
    build_provider_from_resolved, build_telemetry_meta_with_attribution, resolved_needs_base_url,
};
