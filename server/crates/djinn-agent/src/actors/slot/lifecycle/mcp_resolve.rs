//! Per-session MCP server + skills resolution for the task lifecycle.
//!
//! This is a pure code-motion extraction from `run_task_lifecycle` (task #14
//! preparatory work). It reads the MCP default + global-skill fields of
//! `environment_config` from Dolt (verification config already moved there —
//! see `crate::verification::environment`), resolves the effective MCP
//! servers and skills (role-level list merged with project defaults),
//! connects to the resolved MCP servers (best-effort; unreachable servers are
//! logged and skipped), and loads skill markdown files from the worktree.
//!
//! No failure modes here propagate errors: missing environment config, unknown
//! server names, unreachable endpoints, and missing skill files are all
//! non-fatal and emit a `tracing::warn!` on the existing log path. There is
//! therefore no error type — the helper always returns the populated struct.

use std::path::Path;

use crate::context::AgentContext;
use crate::mcp_client::McpToolRegistry;
use crate::roles::AgentRole;
use crate::skills::ResolvedSkill;
use crate::verification::environment::environment_config_for_path;
use crate::verification::settings::{effective_mcp_server_names, effective_skill_names};

/// Resolved MCP + skills bundle for the upcoming session.
///
/// `effective_mcp_servers` / `effective_skills` are the pre-resolve *name*
/// lists used for downstream telemetry (the reply-loop context records them
/// for session-log provenance); `mcp_registry` / `resolved_skills` are the
/// fully-hydrated forms used for tool dispatch / prompt building.
///
/// Setup / verification-rule fields previously came in on a
/// `DjinnSettings` handle returned here; they were moved to Dolt's
/// `projects.environment_config.verification` as part of the P8 cut-over.
/// Downstream callers fetch that block directly via
/// [`crate::verification::environment::verification_for_project_id`].
pub(crate) struct McpAndSkills {
    pub effective_mcp_servers: Vec<String>,
    pub effective_skills: Vec<String>,
    pub mcp_registry: Option<McpToolRegistry>,
    pub resolved_skills: Vec<ResolvedSkill>,
}

/// Fetch project environment config, resolve the effective MCP server + skill
/// lists for the current role, connect to the resolved MCP servers, and load
/// the skill markdown files.
///
/// Behaviour:
///   - Fetching the `environment_config` from Dolt is best-effort; any
///     failure (missing row, parse error, pre-reseed column) is logged on the
///     call path and defaulted to an empty config.
///   - Empty `effective_mcp_servers` short-circuits both the registry load
///     and `connect_and_discover` so default-role sessions don't touch the
///     MCP machinery at all.
///   - The `mcp_registry_override` test seam bypasses `connect_and_discover`.
///   - The two `tracing::info!` "resolved role MCP servers" / "resolved role
///     skills" log lines are preserved.
pub(crate) async fn resolve_mcp_and_skills(
    worktree_path: &Path,
    runtime_role: &dyn AgentRole,
    task_short_id: &str,
    role_mcp_servers: Option<&[String]>,
    role_skills: &[String],
    #[cfg(test)] mcp_registry_override: Option<McpToolRegistry>,
    app_state: &AgentContext,
) -> McpAndSkills {
    let env_cfg = environment_config_for_path(&app_state.db, worktree_path).await;

    let effective_mcp_servers = effective_mcp_server_names(
        &env_cfg.agent_mcp_defaults,
        runtime_role.config().name,
        role_mcp_servers,
    );
    let effective_skills = effective_skill_names(&env_cfg.global_skills, role_skills);

    // ── Resolve role-level MCP servers ────────────────────────────────────────
    // Load the project MCP server registry from standard discovery files
    // (`mcp.json`, `.cursor/mcp.json`, `.opencode/mcp.json`). Unknown names
    // are logged as warnings and skipped — they never block the session from
    // starting.
    //
    // Default roles have empty mcp_servers, so this block is a no-op for them.
    let resolved_mcp_servers = if !effective_mcp_servers.is_empty() {
        let registry = crate::verification::settings::load_mcp_server_registry(worktree_path);
        let resolved = crate::verification::settings::resolve_mcp_servers(
            task_short_id,
            runtime_role.config().name,
            &effective_mcp_servers,
            &registry,
        );
        tracing::info!(
            task_id = %task_short_id,
            role = %runtime_role.config().name,
            requested_count = effective_mcp_servers.len(),
            resolved_count = resolved.len(),
            "Lifecycle: resolved role MCP servers"
        );
        resolved
            .into_iter()
            .map(|(name, cfg)| (name, cfg.clone()))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    // Connect to resolved MCP servers and discover their tool definitions.
    // Unreachable or misconfigured servers are logged and skipped (non-fatal).
    let mcp_registry = {
        #[cfg(test)]
        {
            if let Some(registry) = mcp_registry_override {
                Some(registry)
            } else if !resolved_mcp_servers.is_empty() {
                crate::mcp_client::connect_and_discover(
                    task_short_id,
                    runtime_role.config().name,
                    &resolved_mcp_servers,
                    app_state,
                )
                .await
            } else {
                None
            }
        }
        #[cfg(not(test))]
        {
            if !resolved_mcp_servers.is_empty() {
                crate::mcp_client::connect_and_discover(
                    task_short_id,
                    runtime_role.config().name,
                    &resolved_mcp_servers,
                    app_state,
                )
                .await
            } else {
                None
            }
        }
    };

    // ── Load and resolve skills from worktree .djinn/skills/ ─────────────────
    // Skills are markdown files with YAML frontmatter. Missing skills are logged
    // as warnings and skipped — they never block the session from starting.
    let resolved_skills = if !effective_skills.is_empty() {
        let loaded = crate::skills::load_skills(worktree_path, &effective_skills);
        tracing::info!(
            task_id = %task_short_id,
            role = %runtime_role.config().name,
            requested_count = effective_skills.len(),
            resolved_count = loaded.len(),
            "Lifecycle: resolved role skills"
        );
        loaded
    } else {
        Vec::new()
    };

    McpAndSkills {
        effective_mcp_servers,
        effective_skills,
        mcp_registry,
        resolved_skills,
    }
}
