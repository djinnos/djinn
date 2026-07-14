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
pub use prompts::{MEMORY_L0_ABSTRACT, MEMORY_L1_OVERVIEW};
