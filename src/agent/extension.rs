use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;
use tokio::time::{Duration, timeout};

use goose::agents::ExtensionConfig;
use goose::conversation::message::{Message, MessageContent};
use rmcp::model::{CallToolResult as RmcpCallToolResult, Content as RmcpContent, Tool as RmcpTool};
use rmcp::object;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use super::AgentType;
use super::sandbox;
use crate::db::repositories::note::NoteRepository;
use crate::db::repositories::session::SessionRepository;
use crate::db::repositories::task::TaskRepository;
use crate::models::task::Task;
use crate::server::AppState;

pub fn config(_agent_type: AgentType) -> ExtensionConfig {
    let mut tool_values = vec![
        serde_json::to_value(tool_task_show()).expect("serialize tool_task_show"),
        serde_json::to_value(tool_task_comment_add()).expect("serialize tool_task_comment_add"),
        serde_json::to_value(tool_memory_read()).expect("serialize tool_memory_read"),
        serde_json::to_value(tool_memory_search()).expect("serialize tool_memory_search"),
    ];

    let mut available_tools = vec![
        "task_show".to_string(),
        "task_comment_add".to_string(),
        "memory_read".to_string(),
        "memory_search".to_string(),
    ];

    // All agent types get task_update and shell tools.
    {
        tool_values
            .push(serde_json::to_value(tool_task_update()).expect("serialize tool_task_update"));
        tool_values.push(serde_json::to_value(tool_shell()).expect("serialize tool_shell"));
        available_tools.push("task_update".to_string());
        available_tools.push("shell".to_string());
    }

    let tools = serde_json::from_value(serde_json::Value::Array(tool_values))
        .expect("deserialize goose frontend tools");

    ExtensionConfig::Frontend {
        name: "djinn".to_string(),
        description: "Djinn task and memory tools".to_string(),
        tools,
        instructions: Some(
            "Use Djinn tools for task lifecycle and project memory. For shell calls, pass workdir as the active task worktree path.".to_string(),
        ),
        bundled: Some(true),
        available_tools,
    }
}

pub async fn handle_event(
    state: &AppState,
    agent: &goose::agents::Agent,
    event: &goose::agents::AgentEvent,
    worktree_path: &Path,
) {
    let goose::agents::AgentEvent::Message(msg) = event else {
        return;
    };

    handle_frontend_requests_in_message(state, agent, msg, worktree_path).await;
}

async fn handle_frontend_requests_in_message(
    state: &AppState,
    agent: &goose::agents::Agent,
    message: &Message,
    worktree_path: &Path,
) {
    for content in &message.content {
        let MessageContent::FrontendToolRequest(req) = content else {
            continue;
        };

        let payload = match &req.tool_call {
            Ok(tool_call) => dispatch_tool_call(state, tool_call, worktree_path).await,
            Err(err) => Err(format!("invalid frontend tool call: {err}")),
        };

        let (value, is_error) = match payload {
            Ok(value) => (value, false),
            Err(err) => (serde_json::json!({ "error": err }), true),
        };

        let ours = RmcpCallToolResult {
            content: vec![RmcpContent::text(value.to_string())],
            structured_content: None,
            is_error: Some(is_error),
            meta: None,
        };

        let result = Ok(serde_json::from_value(
            serde_json::to_value(ours).expect("serialize call_tool_result"),
        )
        .expect("deserialize goose call_tool_result"));

        agent.handle_tool_result(req.id.clone(), result).await;
    }
}

#[derive(Deserialize)]
struct IncomingToolCall {
    name: String,
    arguments: Option<serde_json::Map<String, serde_json::Value>>,
}

async fn dispatch_tool_call<T>(
    state: &AppState,
    tool_call: &T,
    worktree_path: &Path,
) -> Result<serde_json::Value, String>
where
    T: Serialize,
{
    let call: IncomingToolCall =
        from_value(serde_json::to_value(tool_call).map_err(|e| e.to_string())?)
            .map_err(|e| format!("invalid frontend tool payload: {e}"))?;

    match call.name.as_str() {
        "task_show" => call_task_show(state, &call.arguments).await,
        "task_create" => call_task_create(state, &call.arguments).await,
        "task_update" => call_task_update(state, &call.arguments).await,
        "task_comment_add" => call_task_comment_add(state, &call.arguments).await,
        "memory_read" => call_memory_read(state, &call.arguments).await,
        "memory_search" => call_memory_search(state, &call.arguments).await,
        "shell" => call_shell(&call.arguments, worktree_path).await,
        other => Err(format!("unknown djinn frontend tool: {other}")),
    }
}

#[derive(Deserialize)]
struct TaskShowParams {
    id: String,
}

#[derive(Deserialize)]
struct TaskUpdateParams {
    id: String,
    title: Option<String>,
    description: Option<String>,
    design: Option<String>,
    priority: Option<i64>,
    owner: Option<String>,
    labels_add: Option<Vec<String>>,
    labels_remove: Option<Vec<String>>,
    acceptance_criteria: Option<Vec<serde_json::Value>>,
    memory_refs_add: Option<Vec<String>>,
    memory_refs_remove: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct TaskCreateParams {
    epic_id: String,
    title: String,
    issue_type: Option<String>,
    description: Option<String>,
    design: Option<String>,
    priority: Option<i64>,
    owner: Option<String>,
}

#[derive(Deserialize)]
struct TaskCommentAddParams {
    id: String,
    body: String,
    actor_id: Option<String>,
    actor_role: Option<String>,
}

#[derive(Deserialize)]
struct MemoryReadParams {
    project: Option<String>,
    identifier: String,
}

#[derive(Deserialize)]
struct MemorySearchParams {
    project: Option<String>,
    query: String,
    folder: Option<String>,
    #[serde(rename = "type")]
    note_type: Option<String>,
    limit: Option<i64>,
}

#[derive(Deserialize)]
struct ShellParams {
    command: String,
    workdir: String,
    timeout_ms: Option<u64>,
}

async fn call_task_show(
    state: &AppState,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskShowParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db().clone(), state.events().clone());
    let session_repo = SessionRepository::new(state.db().clone(), state.events().clone());

    match repo.resolve(&p.id).await {
        Ok(Some(task)) => {
            let mut value = task_to_value(&task);
            if let Some(map) = value.as_object_mut() {
                let session_count = session_repo.count_for_task(&task.id).await.unwrap_or(0);
                let active_session = session_repo.active_for_task(&task.id).await.ok().flatten();
                map.insert(
                    "session_count".to_string(),
                    serde_json::json!(session_count),
                );
                map.insert(
                    "active_session".to_string(),
                    serde_json::json!(active_session),
                );
            }
            Ok(value)
        }
        Ok(None) => Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) })),
        Err(e) => Err(e.to_string()),
    }
}

async fn call_task_create(
    state: &AppState,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskCreateParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db().clone(), state.events().clone());

    let issue_type = p.issue_type.as_deref().unwrap_or("task");
    let description = p.description.as_deref().unwrap_or("");
    let design = p.design.as_deref().unwrap_or("");
    let priority = p.priority.unwrap_or(0);
    let owner = p.owner.as_deref().unwrap_or("");

    let task = repo
        .create(
            &p.epic_id,
            &p.title,
            description,
            design,
            issue_type,
            priority,
            owner,
        )
        .await
        .map_err(|e| e.to_string())?;

    Ok(task_to_value(&task))
}

async fn call_task_update(
    state: &AppState,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskUpdateParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db().clone(), state.events().clone());

    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };

    let title = p.title.as_deref().unwrap_or(&task.title);
    let description = p.description.as_deref().unwrap_or(&task.description);
    let design = p.design.as_deref().unwrap_or(&task.design);
    let priority = p.priority.unwrap_or(task.priority);
    let owner = p.owner.as_deref().unwrap_or(&task.owner);

    let labels_json = if p.labels_add.is_some() || p.labels_remove.is_some() {
        let mut labels: Vec<String> = serde_json::from_str(&task.labels).unwrap_or_default();
        if let Some(add) = p.labels_add {
            for label in add {
                if !labels.contains(&label) {
                    labels.push(label);
                }
            }
        }
        if let Some(remove) = p.labels_remove {
            labels.retain(|v| !remove.contains(v));
        }
        serde_json::to_string(&labels).unwrap_or_else(|_| "[]".to_string())
    } else {
        task.labels.clone()
    };

    let ac_json = p
        .acceptance_criteria
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".to_string()))
        .unwrap_or_else(|| task.acceptance_criteria.clone());

    let updated = repo
        .update(
            &task.id,
            title,
            description,
            design,
            priority,
            owner,
            &labels_json,
            &ac_json,
        )
        .await
        .map_err(|e| e.to_string())?;

    if p.memory_refs_add.is_some() || p.memory_refs_remove.is_some() {
        let mut refs: Vec<String> = serde_json::from_str(&updated.memory_refs).unwrap_or_default();
        if let Some(add) = p.memory_refs_add {
            for r in add {
                if !refs.contains(&r) {
                    refs.push(r);
                }
            }
        }
        if let Some(remove) = p.memory_refs_remove {
            refs.retain(|r| !remove.contains(r));
        }
        let refs_json = serde_json::to_string(&refs).unwrap_or_else(|_| "[]".to_string());
        let out = repo
            .update_memory_refs(&updated.id, &refs_json)
            .await
            .map_err(|e| e.to_string())?;
        return Ok(task_to_value(&out));
    }

    Ok(task_to_value(&updated))
}

async fn call_task_comment_add(
    state: &AppState,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskCommentAddParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db().clone(), state.events().clone());

    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };

    let payload = serde_json::json!({ "body": p.body }).to_string();
    let actor_id = p.actor_id.as_deref().unwrap_or("goose-agent");
    let actor_role = p.actor_role.as_deref().unwrap_or("system");

    let entry = repo
        .log_activity(Some(&task.id), actor_id, actor_role, "comment", &payload)
        .await
        .map_err(|e| e.to_string())?;

    Ok(serde_json::json!({
        "id": entry.id,
        "task_id": entry.task_id,
        "actor_id": entry.actor_id,
        "actor_role": entry.actor_role,
        "event_type": entry.event_type,
        "payload": serde_json::from_str::<serde_json::Value>(&entry.payload).unwrap_or(serde_json::json!({})),
        "created_at": entry.created_at,
    }))
}

async fn call_memory_read(
    state: &AppState,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: MemoryReadParams = parse_args(arguments)?;
    let project_path = resolve_project_path(p.project);
    let project_id = project_id_for_path(state, &project_path).await?;

    let repo = NoteRepository::new(state.db().clone(), state.events().clone());
    let note = resolve_note_by_identifier(&repo, &project_id, &p.identifier)
        .await
        .ok_or_else(|| format!("note not found: {}", p.identifier))?;

    let _ = repo.touch_accessed(&note.id).await;
    Ok(note_to_value(&note))
}

async fn call_memory_search(
    state: &AppState,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: MemorySearchParams = parse_args(arguments)?;
    let project_path = resolve_project_path(p.project);
    let project_id = project_id_for_path(state, &project_path).await?;

    let repo = NoteRepository::new(state.db().clone(), state.events().clone());
    let limit = p.limit.unwrap_or(10).clamp(1, 100) as usize;
    let results = repo
        .search(
            &project_id,
            &p.query,
            p.folder.as_deref(),
            p.note_type.as_deref(),
            limit,
        )
        .await
        .map_err(|e| e.to_string())?;

    let items: Vec<serde_json::Value> = results
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "id": r.id,
                "permalink": r.permalink,
                "title": r.title,
                "folder": r.folder,
                "note_type": r.note_type,
                "snippet": r.snippet,
            })
        })
        .collect();

    Ok(serde_json::json!({ "results": items }))
}

async fn call_shell(
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    worktree_path: &Path,
) -> Result<serde_json::Value, String> {
    let p: ShellParams = parse_args(arguments)?;
    let timeout_ms = p.timeout_ms.unwrap_or(120_000).max(1000);

    let workdir = resolve_path(
        &p.workdir,
        &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    );

    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/c").arg(&p.command);
        c
    } else {
        let mut c = Command::new("bash");
        c.arg("-lc").arg(&p.command);
        c
    };

    sandbox::SANDBOX
        .apply(worktree_path, &mut cmd)
        .map_err(|e| e.to_string())?;

    let output = timeout(
        Duration::from_millis(timeout_ms),
        cmd.current_dir(&workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    )
    .await
    .map_err(|_| format!("shell timed out after {} ms", timeout_ms))?
    .map_err(|e| format!("failed to run shell command: {e}"))?;

    let stdout = truncate_shell_output(&String::from_utf8_lossy(&output.stdout));
    let stderr = truncate_shell_output(&String::from_utf8_lossy(&output.stderr));

    Ok(serde_json::json!({
        "ok": output.status.success(),
        "exit_code": output.status.code(),
        "stdout": stdout,
        "stderr": stderr,
        "workdir": workdir,
    }))
}

fn resolve_path(raw: &str, base: &std::path::Path) -> PathBuf {
    use std::path::Component;
    let p = Path::new(raw);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn parse_args<T>(
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<T, String>
where
    T: for<'de> Deserialize<'de>,
{
    let args = arguments.clone().unwrap_or_default();
    serde_json::from_value(serde_json::Value::Object(args)).map_err(|e| e.to_string())
}

async fn project_id_for_path(state: &AppState, project_path: &str) -> Result<String, String> {
    state
        .db()
        .ensure_initialized()
        .await
        .map_err(|e| e.to_string())?;

    let normalized = project_path.trim_end_matches('/');

    let project_id = sqlx::query_scalar::<_, String>("SELECT id FROM projects WHERE path = ?1")
        .bind(normalized)
        .fetch_optional(state.db().pool())
        .await
        .map_err(|e| e.to_string())?;

    if let Some(project_id) = project_id {
        return Ok(project_id);
    }

    let all_projects = sqlx::query_as::<_, (String, String)>("SELECT id, path FROM projects")
        .fetch_all(state.db().pool())
        .await
        .map_err(|e| e.to_string())?;

    let mut best: Option<(String, usize)> = None;
    for (id, path) in all_projects {
        let root = path.trim_end_matches('/');
        let matches = normalized == root
            || normalized
                .strip_prefix(root)
                .map(|suffix| suffix.starts_with('/'))
                .unwrap_or(false);
        if matches {
            let len = root.len();
            if best
                .as_ref()
                .map(|(_, best_len)| len > *best_len)
                .unwrap_or(true)
            {
                best = Some((id, len));
            }
        }
    }

    best.map(|(id, _)| id)
        .ok_or_else(|| format!("project not found: {project_path}"))
}

fn resolve_project_path(project: Option<String>) -> String {
    match project {
        Some(path) => path,
        None => std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .display()
            .to_string(),
    }
}

async fn resolve_note_by_identifier(
    repo: &NoteRepository,
    project_id: &str,
    identifier: &str,
) -> Option<crate::models::note::Note> {
    if let Ok(Some(n)) = repo.get_by_permalink(project_id, identifier).await {
        return Some(n);
    }
    if let Ok(results) = repo.search(project_id, identifier, None, None, 1).await
        && let Some(r) = results.into_iter().next()
    {
        return repo.get(&r.id).await.ok().flatten();
    }
    None
}

fn note_to_value(note: &crate::models::note::Note) -> serde_json::Value {
    let tags: serde_json::Value = serde_json::from_str(&note.tags).unwrap_or(serde_json::json!([]));
    serde_json::json!({
        "id": note.id,
        "project_id": note.project_id,
        "permalink": note.permalink,
        "title": note.title,
        "file_path": note.file_path,
        "note_type": note.note_type,
        "folder": note.folder,
        "tags": tags,
        "content": note.content,
        "created_at": note.created_at,
        "updated_at": note.updated_at,
        "last_accessed": note.last_accessed,
    })
}

fn task_to_value(t: &Task) -> serde_json::Value {
    let labels: serde_json::Value =
        serde_json::from_str(&t.labels).unwrap_or(serde_json::json!([]));
    let ac: serde_json::Value =
        serde_json::from_str(&t.acceptance_criteria).unwrap_or(serde_json::json!([]));
    let memory_refs: serde_json::Value =
        serde_json::from_str(&t.memory_refs).unwrap_or(serde_json::json!([]));

    serde_json::json!({
        "id": t.id,
        "short_id": t.short_id,
        "epic_id": t.epic_id,
        "title": t.title,
        "description": t.description,
        "design": t.design,
        "issue_type": t.issue_type,
        "status": t.status,
        "priority": t.priority,
        "owner": t.owner,
        "labels": labels,
        "memory_refs": memory_refs,
        "acceptance_criteria": ac,
        "reopen_count": t.reopen_count,
        "continuation_count": t.continuation_count,
        "created_at": t.created_at,
        "updated_at": t.updated_at,
        "closed_at": t.closed_at,
        "close_reason": t.close_reason,
        "merge_commit_sha": t.merge_commit_sha,
    })
}

fn tool_task_show() -> RmcpTool {
    RmcpTool::new(
        "task_show".to_string(),
        "Show details of a work item including recent activity and blockers.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Task UUID or short ID"}
            }
        }),
    )
}

fn tool_task_update() -> RmcpTool {
    RmcpTool::new(
        "task_update".to_string(),
        "Update fields on a work item.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string"},
                "title": {"type": "string"},
                "description": {"type": "string"},
                "design": {"type": "string"},
                "priority": {"type": "integer"},
                "owner": {"type": "string"},
                "labels_add": {"type": "array", "items": {"type": "string"}},
                "labels_remove": {"type": "array", "items": {"type": "string"}},
                "acceptance_criteria": {"type": "array", "items": {}},
                "memory_refs_add": {"type": "array", "items": {"type": "string"}},
                "memory_refs_remove": {"type": "array", "items": {"type": "string"}}
            }
        }),
    )
}

fn tool_task_comment_add() -> RmcpTool {
    RmcpTool::new(
        "task_comment_add".to_string(),
        "Add a comment to a work item.".to_string(),
        object!({
            "type": "object",
            "required": ["id", "body"],
            "properties": {
                "id": {"type": "string"},
                "body": {"type": "string"},
                "actor_id": {"type": "string"},
                "actor_role": {"type": "string"}
            }
        }),
    )
}

fn tool_memory_read() -> RmcpTool {
    RmcpTool::new(
        "memory_read".to_string(),
        "Read a note by permalink or title.".to_string(),
        object!({
            "type": "object",
            "required": ["identifier"],
            "properties": {
                "project": {"type": "string", "description": "Absolute project path"},
                "identifier": {"type": "string"}
            }
        }),
    )
}

fn tool_memory_search() -> RmcpTool {
    RmcpTool::new(
        "memory_search".to_string(),
        "Search notes in project memory.".to_string(),
        object!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "project": {"type": "string", "description": "Absolute project path"},
                "query": {"type": "string"},
                "folder": {"type": "string"},
                "type": {"type": "string"},
                "limit": {"type": "integer"}
            }
        }),
    )
}

fn tool_shell() -> RmcpTool {
    RmcpTool::new(
        "shell".to_string(),
        "Execute shell commands in the task worktree.".to_string(),
        object!({
            "type": "object",
            "required": ["command", "workdir"],
            "properties": {
                "command": {"type": "string"},
                "workdir": {"type": "string", "description": "Absolute task worktree path"},
                "timeout_ms": {"type": "integer"}
            }
        }),
    )
}

/// Truncate shell output to prevent blowing the context window.
/// Hard cap at 50 KB — both line count and byte size are enforced.
fn truncate_shell_output(raw: &str) -> String {
    const MAX_LINES: usize = 2000;
    const MAX_BYTES: usize = 50_000;

    if raw.len() <= MAX_BYTES && raw.split('\n').count() <= MAX_LINES {
        return raw.to_string();
    }

    let total_lines = raw.split('\n').count();
    let total_bytes = raw.len();

    // Take last lines that fit within MAX_BYTES
    let mut preview_bytes = 0;
    let mut preview_lines: Vec<&str> = Vec::new();
    for line in raw.rsplit('\n') {
        let line_bytes = line.len() + 1; // +1 for newline
        if preview_bytes + line_bytes > MAX_BYTES && !preview_lines.is_empty() {
            break;
        }
        preview_bytes += line_bytes;
        preview_lines.push(line);
    }
    preview_lines.reverse();
    let preview = preview_lines.join("\n");

    let reason = format!(
        "Output truncated to {} KB ({} lines / {} bytes total).",
        MAX_BYTES / 1000,
        total_lines,
        total_bytes
    );

    format!(
        "{preview}\n\n[{reason} Use shell commands like `head`, `tail`, or `sed -n '100,200p'` to read sections.]"
    )
}

fn from_value<T>(value: serde_json::Value) -> Result<T, serde_json::Error>
where
    T: DeserializeOwned,
{
    serde_json::from_value(value)
}
