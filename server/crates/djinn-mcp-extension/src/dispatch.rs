//! Tool dispatch and handler orchestration.
//!
//! This module will house the dispatch logic currently in
//! `djinn-agent::extension::handlers` (including `dispatch_tool_call`)
//! once handler bodies are extracted in a later wave. Dispatch will
//! operate over the [`context::ExtensionContext`] trait rather than the
//! concrete `AgentContext`.
//!
//! [`context::ExtensionContext`]: crate::context
