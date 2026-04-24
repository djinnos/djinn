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

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use rmcp::model::Tool as RmcpTool;
use rmcp::object;

use crate::context::AgentContext;
#[cfg(target_os = "linux")]
use crate::sandbox::chat_shell::{ChatShellError, ChatShellRequest, ChatShellSandbox};

// ─── Chat-specific tool schemas ──────────────────────────────────────────

/// Chat-facing `shell`.  Adds a required `project` argument; worker/architect
/// schema (`extension::tool_defs::tool_shell`) keeps the task-worktree
/// signature unchanged.
///
/// `argv` is an array; `/bin/sh -c` is deliberately not available.  The
/// first token must be on the
/// [`crate::sandbox::chat_shell`] command allowlist.
fn tool_chat_shell() -> RmcpTool {
    RmcpTool::new(
        "shell".to_string(),
        "Execute a read-only shell command against the named project's \
         ephemeral clone. `argv` is an array of strings: the first element \
         is matched against a fixed allowlist (cat, ls, find, grep, rg, head, \
         tail, wc, sort, uniq, tr, awk, sed, jq, file, stat, du, tree, git, \
         env, echo, printf, pwd, basename, dirname, realpath, readlink, \
         test, xargs, date). No shell interpreter is available. Commands \
         run from the clone root."
            .to_string(),
        object!({
            "type": "object",
            "required": ["project", "argv"],
            "properties": {
                "project": {
                    "type": "string",
                    "description": "Project slug (owner/repo) or UUID. Required on every call — chat sessions are not pinned to a project."
                },
                "argv": {
                    "type": "array",
                    "description": "Argv vector. First element is the command; remaining elements are arguments.",
                    "items": { "type": "string" },
                    "minItems": 1
                }
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

/// Curated allowlist of server-wide `DjinnMcpServer` tools the chat may
/// call. Shipping the full `DjinnMcpServer::all_tool_schemas()` to the
/// model leaks every admin/write surface (credential_set,
/// project_environment_config_set, task_update, settings_set, …) —
/// also trips OpenAI's strict schema validator on a few tools whose
/// `object` parameters lack a `properties` field.
///
/// Entries here are strictly read-oriented from chat's perspective, plus
/// the one ADR-050-parity write (`epic_create`) and the GitHub file
/// fetch. Admin, runtime, and provider tools are not on this list.
const CHAT_ALLOWED_MCP_TOOLS: &[&str] = &[
    // Read-only memory
    "memory_read",
    "memory_search",
    "memory_list",
    "memory_build_context",
    "memory_health",
    "memory_broken_links",
    "memory_orphans",
    "memory_associations",
    "memory_catalog",
    "memory_recent",
    "memory_graph",
    "memory_diff",
    "memory_history",
    "memory_task_refs",
    "memory_extracted_audit",
    // Read-only tasks + epics
    "task_show",
    "task_list",
    "task_activity_list",
    "task_blocked_list",
    "task_blockers_list",
    "task_memory_refs",
    "task_ready",
    "task_count",
    "task_timeline",
    "epic_show",
    "epic_list",
    "epic_tasks",
    "epic_count",
    // ADR-050 §2 parity: the single knowledge-base write chat retains.
    "epic_create",
    // GitHub read
    "github_fetch_file",
];

/// Returns `true` if this server-wide MCP tool is on the chat allowlist.
/// Used both to filter the schemas sent to the model and to gate dispatch
/// when the model asks for an unallowed tool.
pub fn is_chat_allowed_mcp_tool(name: &str) -> bool {
    CHAT_ALLOWED_MCP_TOOLS.contains(&name)
}

/// Union predicate: `true` iff `name` is valid in chat at all, regardless
/// of which dispatch tier handles it. The two backing lists
/// ([`CHAT_EXTENSION_TOOLS`] and [`CHAT_ALLOWED_MCP_TOOLS`]) partition
/// chat's surface by routing target, but callers that only care "is this
/// tool exposed to chat?" should reach for this helper rather than OR-ing
/// the two predicates by hand.
pub fn is_chat_allowed_tool(name: &str) -> bool {
    is_chat_extension_tool(name) || is_chat_allowed_mcp_tool(name)
}

/// Filter an `all_tool_schemas()` list down to the chat-allowed subset.
/// Unknown-shaped entries (missing a string `name`) are dropped
/// defensively.
pub fn filter_chat_allowed_mcp_schemas(
    schemas: Vec<serde_json::Value>,
) -> Vec<serde_json::Value> {
    schemas
        .into_iter()
        .filter(|schema| {
            schema
                .get("name")
                .and_then(|v| v.as_str())
                .map(is_chat_allowed_mcp_tool)
                .unwrap_or(false)
        })
        .collect()
}

// ─── Chat-specific handlers ──────────────────────────────────────────────

/// Parse + validate the `argv` tool argument.  Shared by every platform.
fn parse_chat_shell_argv(
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<Vec<String>, String> {
    let argv: Vec<String> = match arguments.as_ref().and_then(|m| m.get("argv")) {
        Some(serde_json::Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for v in items {
                let Some(s) = v.as_str() else {
                    return Err("shell: every `argv` element must be a string".to_owned());
                };
                out.push(s.to_owned());
            }
            out
        }
        Some(_) => {
            return Err("shell: `argv` must be an array of strings".to_owned());
        }
        None => return Err("shell: missing required `argv` array".to_owned()),
    };
    if argv.is_empty() {
        return Err("shell: `argv` must not be empty".to_owned());
    }
    Ok(argv)
}

/// Chat-facing `shell`.  Runs every invocation inside a
/// [`ChatShellSandbox`] (landlock + netns + pid-ns + seccomp + rlimits +
/// env scrub + argv allowlist + stdout cap).  No shell interpreter is
/// reachable; callers pass `argv` as an array and the first element is
/// matched against the sandbox's command allowlist.
#[cfg(target_os = "linux")]
pub async fn call_chat_shell(
    clone_path: &Path,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let argv = parse_chat_shell_argv(arguments)?;

    let sandbox = ChatShellSandbox::new(clone_path.to_path_buf());
    let request = ChatShellRequest {
        argv,
        cwd: None,
        stdin: None,
    };

    let result = sandbox.run(request).await.map_err(|err| match err {
        ChatShellError::DisallowedCommand(name) => format!("disallowed command: {name}"),
        ChatShellError::InvalidArgv => "shell: invalid argv".to_owned(),
        ChatShellError::CwdOutsideClone => {
            "shell: cwd escaped clone root".to_owned()
        }
        ChatShellError::SpawnFailed(e) => format!("shell: spawn failed: {e}"),
        ChatShellError::Interrupted => "shell: interrupted".to_owned(),
    })?;

    let stdout = String::from_utf8_lossy(&result.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&result.stderr).into_owned();
    Ok(serde_json::json!({
        "ok": result.exit_code == Some(0),
        "exit_code": result.exit_code,
        "stdout": stdout,
        "stderr": stderr,
        "truncated": result.truncated,
        "timed_out": result.timed_out,
        "elapsed_ms": result.elapsed.as_millis() as u64,
    }))
}

/// Non-Linux stub: the chat shell sandbox requires Linux-only primitives
/// (Landlock, namespaces, seccomp).  On other platforms we refuse every
/// invocation instead of silently falling through to an unsandboxed
/// bash.
#[cfg(not(target_os = "linux"))]
pub async fn call_chat_shell(
    _clone_path: &Path,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    // Still validate argv so the schema error path is exercised on every
    // platform.
    let _argv = parse_chat_shell_argv(arguments)?;
    Err("shell: chat sandbox is only available on Linux".to_owned())
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

/// A resolved project — produced by the server-side `ProjectResolver`
/// and handed into [`dispatch_chat_tool`] per tool call.
#[derive(Debug, Clone)]
pub struct ChatResolvedProject {
    pub id: String,
    pub clone_path: PathBuf,
}

/// Async callback that turns a `project` tool-arg into a resolved
/// `(id, clone_path)` tuple.  Implemented on the server side by the
/// `ProjectResolver` + `WorkspaceStore` combo; kept as a boxed future
/// here so `djinn-agent` stays free of a `djinn-server` dependency.
///
/// Takes the project ref (slug or UUID) as the only argument — there
/// is no per-user / per-session identity threaded through: projects
/// are globally accessible to any authenticated caller under the
/// one-org-per-deployment model, and the `WorkspaceStore` clone is
/// shared across every chat session.
pub type ChatProjectResolveFn<'a> = &'a (dyn Fn(
    String,
) -> Pin<
    Box<dyn Future<Output = Result<ChatResolvedProject, String>> + Send + 'a>,
> + Send
              + Sync
              + 'a);

/// Dispatch a chat extension tool call.
///
/// For tools that need a project (shell, read, code_graph,
/// pr_review_context), we extract `args["project"]` (a slug or UUID),
/// call `resolve` to turn it into a `ChatResolvedProject`, and pass the
/// resulting `(project_id, clone_path)` into the handler.  For tools
/// that don't need a project (github_search with no `project`,
/// project_list), the resolver is skipped.
///
/// A missing `project` argument on a project-required tool produces a
/// tool-level error — never a panic and never a session-level failure.
pub async fn dispatch_chat_tool<'a>(
    state: &AgentContext,
    name: &str,
    args: serde_json::Value,
    resolve: ChatProjectResolveFn<'a>,
) -> Result<serde_json::Value, String> {
    use crate::extension::handlers;

    let arguments = args.as_object().cloned();

    // Pluck out the `project` arg once; tools that don't need it ignore
    // the result.
    let project_arg = arguments
        .as_ref()
        .and_then(|m| m.get("project"))
        .and_then(|v| v.as_str())
        .map(ToOwned::to_owned);

    // Helper to resolve lazily — only tools that need a project pay the cost.
    async fn require_project<'a>(
        name: &str,
        project_arg: &Option<String>,
        resolve: ChatProjectResolveFn<'a>,
    ) -> Result<ChatResolvedProject, String> {
        let Some(project) = project_arg.as_ref() else {
            return Err(format!(
                "tool '{name}' requires a 'project' argument (slug or UUID)"
            ));
        };
        if project.trim().is_empty() {
            return Err(format!(
                "tool '{name}' requires a non-empty 'project' argument"
            ));
        }
        resolve(project.clone()).await
    }

    match name {
        "shell" => {
            let resolved = require_project(name, &project_arg, resolve).await?;
            call_chat_shell(&resolved.clone_path, &arguments).await
        }
        "read" => {
            let resolved = require_project(name, &project_arg, resolve).await?;
            call_chat_read(state, &resolved.clone_path, &arguments).await
        }
        "code_graph" => {
            let resolved = require_project(name, &project_arg, resolve).await?;
            let clone_path_str = resolved.clone_path.to_string_lossy().into_owned();
            handlers::call_code_graph(state, &arguments, &resolved.id, &clone_path_str).await
        }
        "pr_review_context" => {
            // `pr_review_context` takes `project` already; route through
            // the MCP server so chat and architect share one implementation.
            // We do NOT acquire an ephemeral clone here — the meta-tool
            // reads from Dolt + the bare mirror directly.
            let server = djinn_control_plane::server::DjinnMcpServer::new(state.to_mcp_state());
            let args_value = arguments
                .map(serde_json::Value::Object)
                .unwrap_or(serde_json::Value::Object(Default::default()));
            server.dispatch_tool("pr_review_context", args_value).await
        }
        "github_search" => {
            // `project` is optional on github_search (cross-repo search is
            // a legitimate use); only resolve when supplied.
            let resolved = if project_arg.is_some() {
                Some(require_project(name, &project_arg, resolve).await?)
            } else {
                None
            };
            handlers::call_github_search(state, &arguments, resolved.as_ref().map(|r| r.id.as_str())).await
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

    /// Admin / write / runtime / provider / credential / settings tools
    /// must never appear on the chat allowlist. Regression guard for the
    /// OpenAI 400 on `project_environment_config_set` and the broader
    /// "chat was being handed the entire admin surface" bug.
    #[test]
    fn chat_allowlist_excludes_admin_and_write_tools() {
        let forbidden: &[&str] = &[
            "project_environment_config_set",
            "project_environment_config_reset",
            "project_environment_config_get",
            "project_config_set",
            "project_config_get",
            "project_graph_exclusions_set",
            "project_graph_exclusions_get",
            "project_add_from_github",
            "project_remove",
            "credential_set",
            "credential_delete",
            "credential_list",
            "provider_oauth_start",
            "provider_remove",
            "provider_validate",
            "settings_set",
            "settings_reset",
            "settings_get",
            "memory_write",
            "memory_edit",
            "memory_delete",
            "memory_move",
            "memory_reindex",
            "memory_confirm",
            "task_create",
            "task_update",
            "task_transition",
            "task_claim",
            "task_comment_add",
            "epic_update",
            "epic_close",
            "epic_reopen",
            "epic_delete",
            "agent_create",
            "agent_update",
            "agent_show",
            "agent_list",
            "agent_metrics",
            "board_reconcile",
            "propose_adr_accept",
            "propose_adr_reject",
            "execution_kill_task",
            "retrigger_image_build",
            "github_app_installations",
            "github_app_install_url",
            "github_list_repos",
            "session_list",
            "session_show",
            "session_messages",
            "session_active",
            "session_for_task",
        ];
        for tool in forbidden {
            assert!(
                !is_chat_allowed_mcp_tool(tool),
                "chat allowlist regression: `{tool}` is admin/write and must \
                 never appear in CHAT_ALLOWED_MCP_TOOLS"
            );
        }
    }

    /// `is_chat_allowed_tool` unions the two routing-tier lists. Disjoint
    /// by construction — a tool is either dispatched in-process or via
    /// the MCP router, never both. This test asserts that invariant and
    /// the union shape.
    #[test]
    fn is_chat_allowed_tool_unions_the_two_tiers() {
        // Extension tools are in.
        assert!(is_chat_allowed_tool("shell"));
        assert!(is_chat_allowed_tool("project_list"));
        // MCP tools are in.
        assert!(is_chat_allowed_tool("memory_read"));
        assert!(is_chat_allowed_tool("epic_create"));
        // Admin/write tools are out.
        assert!(!is_chat_allowed_tool("credential_set"));
        assert!(!is_chat_allowed_tool("project_environment_config_set"));
        assert!(!is_chat_allowed_tool("task_update"));
        // Totally unknown names are out.
        assert!(!is_chat_allowed_tool("not_a_real_tool"));

        // Disjointness: no tool is both an extension and an MCP tool.
        for name in CHAT_EXTENSION_TOOLS {
            assert!(
                !is_chat_allowed_mcp_tool(name),
                "tool `{name}` is both an extension tool AND on the MCP \
                 allowlist — the two tiers must be disjoint"
            );
        }
        for name in CHAT_ALLOWED_MCP_TOOLS {
            assert!(
                !is_chat_extension_tool(name),
                "tool `{name}` is both on the MCP allowlist AND an extension \
                 tool — the two tiers must be disjoint"
            );
        }
    }

    /// The filter drops entries not on the allowlist and keeps allowlisted
    /// entries untouched.
    #[test]
    fn filter_drops_disallowed_schemas() {
        let schemas = vec![
            serde_json::json!({"name": "memory_read", "description": "x"}),
            serde_json::json!({"name": "project_environment_config_set", "description": "admin"}),
            serde_json::json!({"name": "epic_create", "description": "allowed write"}),
            serde_json::json!({"name": "credential_set", "description": "secrets"}),
            serde_json::json!({"description": "no name field"}),
        ];
        let filtered = filter_chat_allowed_mcp_schemas(schemas);
        let names: BTreeSet<String> = filtered
            .iter()
            .filter_map(|v| v.get("name").and_then(|n| n.as_str()).map(ToString::to_string))
            .collect();
        let expected: BTreeSet<String> = ["memory_read", "epic_create"]
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(names, expected);
    }
}
