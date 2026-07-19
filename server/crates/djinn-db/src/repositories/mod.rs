pub mod admission_handoff;
pub mod admission_journal;
pub mod agent;
pub mod audit_sampler;
pub mod chat_interruption_notice;
pub mod code_chunk;
pub mod commit_file_changes;
pub mod coordinator_incarnation;
pub mod dispatch_pause;
pub mod dispatch_state;
pub mod doctor_finding;
pub mod epic;
pub mod events;
pub mod extension_load_diagnostic;
#[cfg(test)]
pub mod extension_load_diagnostic_tests;
pub mod git_settings;
pub mod image;
pub mod init;
pub mod legacy_settings_import;
pub mod liveness;
pub mod llm_call_attempt;
pub mod models;
pub mod note;
pub mod oauth;
pub mod org_ai_policy;
pub mod org_config;
pub mod project;
pub mod project_live_state_migration;
pub mod project_workspace_coverage;
pub mod project_workspace_graph;
pub mod proposal;
pub mod repo_graph_cache;
pub mod repo_graph_generation;
pub mod retrieval_trace;
#[cfg(test)]
pub mod retrieval_trace_tests;
pub mod scip_indexer_timing;
pub mod service;
pub mod session;
pub mod session_auth;
pub mod session_compaction_boundary;
pub mod session_message;
pub mod settings;
pub mod task;
pub mod task_arbitration;
pub mod task_attempt;
#[cfg(test)]
pub mod task_attempt_tests;
pub mod task_run;
pub mod task_run_outcome;
pub mod test_support;
pub mod tool_call_evaluator;
#[cfg(test)]
pub mod tool_call_evaluator_tests;
pub mod tool_call_export;
pub mod tool_call_metrics;
pub mod usage_analytics;
pub mod user;
pub mod user_settings;
pub mod verify_run;
pub mod warm_base_activity;

/// Render `count` Postgres positional placeholders starting at `$start`.
///
/// `pg_placeholders(3, 1)` → `"$1, $2, $3"`; `pg_placeholders(2, 6)` →
/// `"$6, $7"`. Used by the dynamic `IN (...)` builders whose bind count is
/// only known at runtime. Postgres rejects MySQL-style `?` placeholders
/// (`syntax error at or near ","`), so every such builder must emit `$N`
/// numbered to match its bind order — including the offset for any fixed
/// params bound before the IN list.
pub(crate) fn pg_placeholders(count: usize, start: usize) -> String {
    (start..start + count)
        .map(|n| format!("${n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod placeholder_tests {
    use super::pg_placeholders;

    #[test]
    fn pg_placeholders_numbers_from_start() {
        assert_eq!(pg_placeholders(3, 1), "$1, $2, $3");
        assert_eq!(pg_placeholders(2, 6), "$6, $7");
        assert_eq!(pg_placeholders(1, 1), "$1");
        assert_eq!(pg_placeholders(0, 1), "");
    }
}
