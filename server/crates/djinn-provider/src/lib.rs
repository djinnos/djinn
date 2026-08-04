pub mod bootstrap;
pub mod catalog;
pub mod completion;
pub mod embeddings;
pub mod error_classify;
pub mod github_api;
pub mod github_app;
pub mod github_server;
pub mod http_util;
pub mod message;
pub mod model_turn_admission;
pub mod oauth;
pub mod prompts;
pub mod provider;
pub mod rate_limit;
pub mod repos;

pub use completion::{
    CompletionRequest, CompletionResponse, complete, resolve_memory_provider,
    resolve_memory_provider_config, resolve_memory_provider_config_for_user,
    resolve_memory_provider_config_for_user_db, resolve_memory_provider_for_user,
};

pub use error_classify::{
    is_context_length_error, is_orphaned_tool_call_error, is_orphaned_tool_call_error_str,
};
pub use model_turn_admission::{
    MAX_OUTPUT_RESERVATION_UNITS_V1, ProviderAbortCapabilityV1, ProviderAdmissionPolicyV1,
    ProviderApiKeyNormalizerV1, ProviderAttemptAbortHandleV1, ProviderAttemptAbortResultV1,
    ProviderAttemptCapabilitiesV1, ProviderAttemptLossV1, ProviderAttemptPlanV1,
    ProviderAttemptRouteCoverageV1, ProviderAttemptScopeV1, ProviderAttemptTerminalV1,
    ProviderAttemptUncoveredReasonV1, ProviderAvailableCapacityV1, ProviderCredentialRecordScopeV1,
    ProviderDiscoveryOwnershipV1, ProviderHiddenRetryCapabilityV1, ProviderNormalizedObservationV1,
    ProviderObservationDiagnosticsV1, ProviderObservationIgnoreReasonV1, ProviderOutcomeV1,
    ProviderOutputReservationSourceV1, ProviderReceiptTimeV1, ProviderTokenEmissionV1,
    ProviderUsageObservationV1, plan_provider_attempt_v1,
};
pub use prompts::{MEMORY_L0_ABSTRACT, MEMORY_L1_OVERVIEW};
