//! Chat-specific tool surface.
//!
//! Exposes a subset of the agent extension tools for use in the Djinn chat
//! interface.  The chat runs read-only against an ephemeral clone of the
//! selected project — no write/edit/apply_patch, and (per the
//! chat-user-global refactor) no `lsp`.
//!
//! Historically chat and architect shared a tool surface by contract
//! (ADR-050 §2 parity).  That parity intentionally breaks for `lsp` under
//! the chat-user-global refactor: chat is single-turn, read-only, and runs
//! against an ephemeral clone that should not host a stateful
//! rust-analyzer process.  The architect keeps `lsp` because it runs in a
//! long-lived patrol context with a warm canonical index tree.
//!
//! ## Chat vs. agent schemas
//!
//! Chat tools also diverge from the agent-facing variants in
//! [`crate::extension::tool_defs`] in that every project-scoped chat tool
//! carries an explicit `project` argument (slug or UUID).  The chat
//! session is no longer pinned to a single project, so each tool call
//! must name its target.  Worker/architect schemas are unchanged — their
//! project context comes from the pinned task worktree, not a tool arg.

use std::path::Path;

use rmcp::model::Tool as RmcpTool;
use rmcp::object;

use crate::context::AgentContext;

// ─── Chat-specific tool schemas ──────────────────────────────────────────

/// Chat-facing `shell`.  Adds a required `project` argument; worker/architect
/// schema (`extension::tool_defs::tool_shell`) keeps the task-worktree
/// signature unchanged.
fn tool_chat_shell() -> RmcpTool {
    RmcpTool::new(
        "shell".to_string(),
        "Execute a read-only shell command against the named project's \
         ephemeral clone. Commands run from the clone root."
            .to_string(),
        object!({
            "type": "object",
            "required": ["project", "command"],
            "properties": {
                "project": {
                    "type": "string",
                    "description": "Project slug (owner/repo) or UUID. Required on every call — chat sessions are not pinned to a project."
                },
                "command": {"type": "string", "description": "Shell command to execute"},
                "timeout_ms": {"type": "integer", "description": "Timeout in milliseconds (default 120000)"}
            }
        }),
    )
}

/// Chat-facing `read`.  Adds a required `project` argument; worker/architect
/// schema keeps the task-worktree signature unchanged.
fn tool_chat_read() -> RmcpTool {
    RmcpTool::new(
        "read".to_string(),
        "Read a file from the named project's ephemeral clone. Returns line-numbered, paginated content. Rejects binary files.".to_string(),
        object!({
            "type": "object",
            "required": ["project", "file_path"],
            "properties": {
                "project": {
                    "type": "string",
                    "description": "Project slug (owner/repo) or UUID. Required on every call — chat sessions are not pinned to a project."
                },
                "file_path": { "type": "string" },
                "offset": { "type": "integer", "minimum": 0 },
                "limit": { "type": "integer", "minimum": 1 }
            }
        }),
    )
}

/// Chat-only `project_list`.  Unpinned chat sessions need discoverability
/// of the projects the caller can target.  Returns a minimal
/// `{projects: [{id, slug, owner, repo}]}` shape.
fn tool_project_list() -> RmcpTool {
    RmcpTool::new(
        "project_list".to_string(),
        "List the projects available to the current user. Returns id, slug (owner/repo), owner, and repo for each. Use the slug or id as the `project` argument on per-project tools.".to_string(),
        object!({
            "type": "object",
            "properties": {},
            "required": []
        }),
    )
}

fn serialize_chat_tool(tool: RmcpTool, concurrent_safe: bool) -> serde_json::Value {
    crate::extension::shared_schemas::serialize_tool_schema(tool, concurrent_safe)
}

/// Tool schemas for the chat interface: read-only codebase tools plus
/// `project_list` for discoverability.
///
/// Diverges from the architect surface on `lsp` (chat drops it; architect
/// keeps it).  See module-level docs + ADR-050 amendment.
pub fn chat_extension_tool_schemas() -> Vec<serde_json::Value> {
    use crate::extension::tool_defs::{
        tool_code_graph, tool_github_search, tool_pr_review_context,
    };
    vec![
        serialize_chat_tool(tool_chat_shell(), false),
        serialize_chat_tool(tool_chat_read(), true),
        serialize_chat_tool(tool_code_graph(), true),
        serialize_chat_tool(tool_pr_review_context(), true),
        serialize_chat_tool(tool_github_search(), true),
        serialize_chat_tool(tool_project_list(), true),
    ]
}

/// Names of tools that [`dispatch_chat_tool`] can handle.
const CHAT_EXTENSION_TOOLS: &[&str] = &[
    "shell",
    "read",
    "code_graph",
    "pr_review_context",
    "github_search",
    "project_list",
];

/// Returns `true` if this tool name is handled by the chat extension dispatch.
pub fn is_chat_extension_tool(name: &str) -> bool {
    CHAT_EXTENSION_TOOLS.contains(&name)
}

// ─── Chat-specific handlers ──────────────────────────────────────────────

/// Chat-facing `shell`.  Thin wrapper over
/// [`crate::extension::handlers::call_shell`].
///
/// TODO(commit 6): wrap the spawn in [`djinn_agent::sandbox::chat_shell::ChatShellSandbox::run`]
/// once the chat handler supplies the sandbox instance.  For now the call
/// runs unsandboxed; commit 6 flips it.
pub async fn call_chat_shell(
    clone_path: &Path,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    crate::extension::handlers::call_shell(arguments, clone_path).await
}

/// Chat-facing `read`.  Thin wrapper over
/// [`crate::extension::handlers::call_read`].
pub async fn call_chat_read(
    state: &AgentContext,
    clone_path: &Path,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    crate::extension::handlers::call_read(state, arguments, clone_path).await
}

/// Chat-only `project_list`.  Returns `{projects: [{id, slug, owner, repo}]}`
/// drawn from [`djinn_db::ProjectRepository::list`].
pub async fn call_project_list(state: &AgentContext) -> Result<serde_json::Value, String> {
    let repo = djinn_db::ProjectRepository::new(state.db.clone(), state.event_bus.clone());
    let projects = repo.list().await.map_err(|e| e.to_string())?;
    let rows: Vec<serde_json::Value> = projects
        .into_iter()
        .map(|p| {
            let slug = format!("{}/{}", p.github_owner, p.github_repo);
            serde_json::json!({
                "id": p.id,
                "slug": slug,
                "owner": p.github_owner,
                "repo": p.github_repo,
            })
        })
        .collect();
    Ok(serde_json::json!({ "projects": rows }))
}

// ─── Dispatcher ──────────────────────────────────────────────────────────

/// Dispatch a chat extension tool call.
///
/// `project_root` is the clone path for the project named in
/// `args["project"]`.  Commit 6 will wire a real `ChatProjectResolver` into
/// the chat handler that resolves the tool-arg project per call against
/// the session's `user_id`, authz-checks it, and produces the
/// `(project_id, clone_path)` tuple passed here.
///
/// `project_id` is the resolved UUID — forwarded to tools that need it
/// (e.g. `code_graph`, `github_search`).
pub async fn dispatch_chat_tool(
    state: &AgentContext,
    name: &str,
    args: serde_json::Value,
    project_root: &Path,
    project_id: &str,
) -> Result<serde_json::Value, String> {
    use crate::extension::handlers;

    let arguments = args.as_object().cloned();
    // ADR-050 Chunk C: respect `working_root` if the caller installed one on
    // the AgentContext (e.g. the chat first-use hook resolves the canonical
    // index-tree path).  Falls back to the supplied `project_root`.
    let effective_root = state.working_root_for(project_root);
    let effective_root_str = effective_root.to_string_lossy().into_owned();
    match name {
        "shell" => call_chat_shell(&effective_root, &arguments).await,
        "read" => call_chat_read(state, &effective_root, &arguments).await,
        "code_graph" => {
            handlers::call_code_graph(state, &arguments, project_id, &effective_root_str).await
        }
        "pr_review_context" => {
            // Route through the MCP server so chat and architect share one
            // implementation — the meta-tool lives in djinn-control-plane.
            let server =
                djinn_control_plane::server::DjinnMcpServer::new(state.to_mcp_state());
            let args_value = arguments
                .map(serde_json::Value::Object)
                .unwrap_or(serde_json::Value::Object(Default::default()));
            server.dispatch_tool("pr_review_context", args_value).await
        }
        "github_search" => {
            handlers::call_github_search(state, &arguments, Some(project_id)).await
        }
        "project_list" => call_project_list(state).await,
        _ => Err(format!("unknown chat extension tool: {name}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension::tool_defs::tool_schemas_architect;
    use std::collections::BTreeSet;

    fn schema_names(schemas: &[serde_json::Value]) -> BTreeSet<String> {
        schemas
            .iter()
            .filter_map(|v| {
                v.get("name")
                    .and_then(|n| n.as_str())
                    .map(ToString::to_string)
            })
            .collect()
    }

    /// Chat's post-refactor tool surface (chat-user-global §5).  Pinned
    /// explicitly so accidental additions/removals are caught.
    ///
    /// Diverges from the architect surface on `lsp` (dropped here; kept
    /// there) — see ADR-050 §2 amendment.
    #[test]
    fn chat_surface_is_exactly_the_expected_set() {
        let expected: BTreeSet<String> = [
            "shell",
            "read",
            "code_graph",
            "pr_review_context",
            "github_search",
            "project_list",
        ]
        .iter()
        .map(ToString::to_string)
        .collect();

        let chat = schema_names(&chat_extension_tool_schemas());
        assert_eq!(
            chat, expected,
            "chat tool surface drift — update chat_extension_tool_schemas() \
             in chat_tools.rs and ADR-050 §2 together"
        );
    }

    /// Architect surface retains every read/analysis tool the chat used to
    /// share (minus `lsp`, which chat drops under the chat-user-global
    /// refactor — see ADR-050 §2 amendment).  Pinned to the read/analysis
    /// subset; the architect's full surface is covered by
    /// `architect_tool_names.snap` and `architect_tool_schemas.snap`.
    #[test]
    fn architect_surface_retains_read_and_analysis_tools() {
        let required_architect: &[&str] = &[
            "shell",
            "read",
            "lsp",
            "code_graph",
            "pr_review_context",
            "github_search",
        ];

        let architect = schema_names(&tool_schemas_architect());
        for tool in required_architect {
            assert!(
                architect.contains(*tool),
                "architect tool surface regression: missing `{tool}`. \
                 The architect retains the full read/analysis subset even \
                 though chat has dropped `lsp` per ADR-050 §2 amendment."
            );
        }
    }

    /// Chat and architect surfaces intentionally diverge on `lsp` under
    /// the chat-user-global refactor (ADR-050 §2 amendment): architect
    /// keeps it, chat drops it.  This test guards that divergence — if
    /// someone re-adds `lsp` to chat it must also be added to the
    /// expected-set above and the ADR must be re-amended.
    #[test]
    fn chat_drops_lsp_but_architect_keeps_it() {
        let chat = schema_names(&chat_extension_tool_schemas());
        let architect = schema_names(&tool_schemas_architect());
        assert!(
            !chat.contains("lsp"),
            "chat surface must not include `lsp` (ADR-050 §2 amendment — \
             chat-user-global drops lsp)"
        );
        assert!(
            architect.contains("lsp"),
            "architect surface must retain `lsp` (ADR-050 §2 amendment — \
             divergence is intentional)"
        );
    }
}
