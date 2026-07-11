#!/usr/bin/env perl
use strict;
use warnings;
local $/;
my $file = $ARGV[0] or die "usage: patch.pl <file>\n";
open my $fh, '<', $file or die "Cannot open $file: $!\n";
my $content = <$fh>;
close $fh;

# Step 1: Replace make_context to delegate to make_context_with_task
my $old_mc = <<'ENDOLD';
/// Returns (context, project_path, task_id, session_id, cancel).
async fn make_context() -> (
    crate::host::SlotContext,
    String,
    String,
    String,
    CancellationToken,
) {
    let cancel = CancellationToken::new();
    let db = test_helpers::create_test_db();
    let ctx = test_helpers::agent_context_from_db(db.clone(), cancel.clone());
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;
    // Create a real session row so session_messages FK constraint is satisfied.
    let session_repo = SessionRepository::new(db.clone(), ctx.event_bus.clone());
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &project.id,
            task_id: Some(&task.id),
            model: "test/mock-model",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .expect("create session");
    let project_path = djinn_core::paths::project_dir(&project.github_owner, &project.github_repo)
        .to_string_lossy()
        .into_owned();
    (ctx, project_path, task.id, session.id, cancel)
}
ENDOLD

my $new_mc = <<'ENDNEW';
/// Returns (context, project_path, task_id, session_id, cancel).
async fn make_context() -> (
    crate::host::SlotContext,
    String,
    String,
    String,
    CancellationToken,
) {
    let (ctx, project_path, task_id, session_id, cancel, _task) =
        make_context_with_task().await;
    (ctx, project_path, task_id, session_id, cancel)
}

/// Extended `make_context` that also returns the `Task` model so callers can
/// pass it to `render_prompt_for_role` for realistic prompt rendering.
async fn make_context_with_task() -> (
    crate::host::SlotContext,
    String,
    String,
    String,
    CancellationToken,
    djinn_core::models::Task,
) {
    let cancel = CancellationToken::new();
    let db = test_helpers::create_test_db();
    let ctx = test_helpers::agent_context_from_db(db.clone(), cancel.clone());
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;
    // Create a real session row so session_messages FK constraint is satisfied.
    let session_repo = SessionRepository::new(db.clone(), ctx.event_bus.clone());
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &project.id,
            task_id: Some(&task.id),
            model: "test/mock-model",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .expect("create session");
    let project_path = djinn_core::paths::project_dir(&project.github_owner, &project.github_repo)
        .to_string_lossy()
        .into_owned();
    (ctx, project_path, task.id, session.id, cancel, task)
}
ENDNEW

my $pos = index($content, $old_mc);
die "FATAL: old make_context not found" unless $pos >= 0;
substr($content, $pos, length($old_mc)) = $new_mc;

# Step 2: Add new_with_worker_prompt after new()
my $old_new = <<'ENDOLD2';
    async fn new() -> Self {
        let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
        let mut conv = Conversation::new();
        conv.push(Message::system("You are a worker."));
        conv.push(Message::user("Do the task."));
        Self {
            slot_ctx,
            project_path,
            task_id,
            session_id,
            cancel,
            conv,
        }
    }
ENDOLD2

my $new_new = <<'ENDNEW2';
    async fn new() -> Self {
        let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
        let mut conv = Conversation::new();
        conv.push(Message::system("You are a worker."));
        conv.push(Message::user("Do the task."));
        Self {
            slot_ctx,
            project_path,
            task_id,
            session_id,
            cancel,
            conv,
        }
    }

    /// Build a harness with the **real** post-wzz6 worker prompt surface:
    /// the actual rendered role prompt with `format_tools_section` applied to
    /// the real canonical tool schemas.  This is the harness used by the
    /// provider-tool preservation regression tests so that a regression in
    /// prompt rendering or canonical tool schema generation would be caught.
    async fn new_with_worker_prompt() -> Self {
        let (slot_ctx, project_path, task_id, session_id, cancel, task) =
            make_context_with_task().await;
        let tool_schemas_fn = djinn_mcp_extension::tool_defs::tool_schemas_worker;
        let role_config = djinn_roles::config::config_for(djinn_roles::AgentType::Worker);
        let task_ctx = djinn_roles::prompts::TaskContext {
            project_path: project_path.clone(),
            workspace_path: "/tmp".to_string(),
            diff: None,
            commits: None,
            start_commit: None,
            end_commit: None,
            conflict_files: None,
            merge_base_branch: None,
            merge_target_branch: None,
            merge_failure_context: None,
            setup_commands: None,
            activity: None,
            worker_summary: None,
            worker_concerns: None,
            epic_context: None,
            knowledge_context: None,
            code_graph_context: None,
            reviewer_diff_context: None,
            ci_blocking_directive: None,
            worker_resume_note: None,
        };
        let system_prompt =
            djinn_roles::prompts::render_prompt_for_role(role_config, tool_schemas_fn, &task, &task_ctx);
        let mut conv = Conversation::new();
        conv.push(Message::system(system_prompt));
        conv.push(Message::user(format!(
            "Implement task {}: {}",
            task.short_id, task.title
        )));
        Self {
            slot_ctx,
            project_path,
            task_id,
            session_id,
            cancel,
            conv,
        }
    }
ENDNEW2

$pos = index($content, $old_new);
die "FATAL: old new() not found" unless $pos >= 0;
substr($content, $pos, length($old_new)) = $new_new;

# Step 3: Replace the entire wzz6_provider_tool_schemas function
my $old_wzz6 = <<'ENDOLD3';
/// Build a set of provider-declared tool schemas that represent the
/// post-wzz6 tool surface: shortened canonical descriptions (≤350 chars)
/// but each tool still carries `name`, `description`, and `parameters`
/// (the OpenAI-format equivalent of `inputSchema`).
///
/// Tool-runtime metadata fields (`readOnly`, `concurrent_safe`, etc.) are
/// preserved unchanged — only prompt-side description text was shortened.
fn wzz6_provider_tool_schemas() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "shell",
                "description": "Execute a shell command in the workspace workdir and return stdout/stderr/exit_code.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "Shell command to execute"
                        },
                        "timeout_ms": {
                            "type": "integer",
                            "description": "Timeout in milliseconds"
                        }
                    },
                    "required": ["command"]
                }
            },
            "readOnly": false,
            "destructive": false,
            "idempotent": false,
            "openWorld": false,
            "concurrent_safe": false
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "read",
                "description": "Read a file from the workspace by path. Rejects binary files.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "file_path": {
                            "type": "string",
                            "description": "Path to the file"
                        }
                    },
                    "required": ["file_path"]
                }
            },
            "readOnly": true,
            "destructive": false,
            "idempotent": true,
            "openWorld": false,
            "concurrent_safe": true
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "submit_work",
                "description": "Signal the worker has finished implementing the task and provide a summary.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "task_id": { "type": "string" },
                        "summary": { "type": "string" },
                        "commit_title": { "type": "string" }
                    },
                    "required": ["task_id", "commit_title", "summary"]
                }
            },
            "readOnly": false,
            "destructive": false,
            "idempotent": false,
            "openWorld": false,
            "concurrent_safe": false
        }),
    ]
}
ENDOLD3

my $new_wzz6 = <<'ENDNEW3';
/// Fetch the real canonical worker tool schemas from `djinn-mcp-extension`.
///
/// These are the same schemas that the production reply loop passes to
/// `.stream(...)` — sourced from `tool_schemas_worker()` via the
/// `djinn-roles` tool-schema registry.  Using the real schemas (rather
/// than hand-written facsimiles) ensures the regression tests catch any
/// change to the canonical schema surface (e.g. dropping `description`
/// or renaming `inputSchema`).
///
/// The schemas use the native format with top-level `name`, `description`,
/// and `inputSchema` keys — matching the wire format seen by
/// `RecordingProvider::stream()`.
fn real_worker_tool_schemas() -> Vec<serde_json::Value> {
    djinn_mcp_extension::tool_defs::tool_schemas_worker()
}
ENDNEW3

$pos = index($content, $old_wzz6);
die "FATAL: old wzz6 fn not found" unless $pos >= 0;
substr($content, $pos, length($old_wzz6)) = $new_wzz6;

# Step 4: Replace usages
$content =~ s/wzz6_provider_tool_schemas\(\)/real_worker_tool_schemas()/g;

# Step 5: Update test 1 assertions to match native schema format (name/description/inputSchema at top level)
# Replace the assertion block in provider_tool_schemas_preserve_name_description_and_input_schema
my $old_assertions = <<'ENDOLD4';
    // Every captured tools array must carry every schema field unchanged.
    for (turn_idx, captured) in captures.iter().enumerate() {
        assert_eq!(
            captured.len(),
            schemas.len(),
            "turn {turn_idx}: captured tools count must match input"
        );
        for (i, tool) in captured.iter().enumerate() {
            // Each tool must be an object with "type" and "function".
            assert_eq!(
                tool.get("type").and_then(|v| v.as_str()),
                Some("function"),
                "turn {turn_idx}, tool {i}: must have type=function"
            );
            let function = tool
                .get("function")
                .unwrap_or_else(|| panic!("turn {turn_idx}, tool {i}: missing function key"));
            // `name` must be present and non-empty.
            let name = function
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| panic!("turn {turn_idx}, tool {i}: function.name missing"));
            assert!(
                !name.is_empty(),
                "turn {turn_idx}, tool {i}: function.name must not be empty"
            );
            // `description` must be present and non-empty.
            let desc = function
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| {
                    panic!("turn {turn_idx}, tool {i}: function.description missing")
                });
            assert!(
                !desc.is_empty(),
                "turn {turn_idx}, tool {i}: function.description must not be empty"
            );
            // `parameters` (= inputSchema) must be present and be an object.
            let params = function.get("parameters").unwrap_or_else(|| {
                panic!("turn {turn_idx}, tool {i}: function.parameters (=inputSchema) missing")
            });
            assert!(
                params.is_object(),
                "turn {turn_idx}, tool {i}: function.parameters must be an object"
            );
        }
    }
}
ENDOLD4

my $new_assertions = <<'ENDNEW4';
    // Every captured tools array must carry every schema field unchanged.
    // Real schemas use native format: top-level `name`, `description`,
    // `inputSchema` (not wrapped in `{"type":"function","function":{…}}`).
    for (turn_idx, captured) in captures.iter().enumerate() {
        assert_eq!(
            captured.len(),
            schemas.len(),
            "turn {turn_idx}: captured tools count must match input"
        );
        for (i, tool) in captured.iter().enumerate() {
            // `name` must be present and non-empty at the top level.
            let name = tool
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| panic!("turn {turn_idx}, tool {i}: name missing"));
            assert!(
                !name.is_empty(),
                "turn {turn_idx}, tool {i}: name must not be empty"
            );
            // `description` must be present and non-empty at the top level.
            let desc = tool
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| {
                    panic!("turn {turn_idx}, tool {i}: description missing")
                });
            assert!(
                !desc.is_empty(),
                "turn {turn_idx}, tool {i}: description must not be empty"
            );
            // `inputSchema` must be present and be an object.
            let input_schema = tool.get("inputSchema").unwrap_or_else(|| {
                panic!("turn {turn_idx}, tool {i}: inputSchema missing")
            });
            assert!(
                input_schema.is_object(),
                "turn {turn_idx}, tool {i}: inputSchema must be an object"
            );
        }
    }
}
ENDNEW4

$pos = index($content, $old_assertions);
die "FATAL: old assertions block not found" unless $pos >= 0;
substr($content, $pos, length($old_assertions)) = $new_assertions;

# Step 6: Update test 3 dispatch_semantics_unchanged assertions
# The third test checks schema.get("function") for description - update to top-level
my $old_desc_check = <<'ENDOLD5';
    // Description text is still substantive after shortening — not empty
    // and not replaced with signature-only text.
    for schema in &schemas {
        let function = schema.get("function").expect("function key");
        let desc = function
            .get("description")
            .and_then(|v| v.as_str())
            .expect("description present");
        let name = function
            .get("name")
            .and_then(|v| v.as_str())
            .expect("name present");
        assert!(
            desc.len() > 10,
            "description for {name} should be substantive after shortening, got: {desc:?}"
        );
        // Description should not accidentally be the parameter signature
        // (which would mean prompt-side deduplication leaked into provider schemas).
        assert!(
            !desc.starts_with('(') && !desc.contains("required:"),
            "description for {name} should not be a parameter signature: {desc:?}"
        );
    }
ENDOLD5

my $new_desc_check = <<'ENDNEW5';
    // Description text is still substantive after shortening — not empty
    // and not replaced with signature-only text.
    // Real schemas use native format: description at top level (not nested
    // under "function").
    for schema in &schemas {
        let desc = schema
            .get("description")
            .and_then(|v| v.as_str())
            .expect("description present at top level");
        let name = schema
            .get("name")
            .and_then(|v| v.as_str())
            .expect("name present at top level");
        assert!(
            desc.len() > 10,
            "description for {name} should be substantive after shortening, got: {desc:?}"
        );
        // Description should not accidentally be the parameter signature
        // (which would mean prompt-side deduplication leaked into provider schemas).
        assert!(
            !desc.starts_with('(') && !desc.contains("required:"),
            "description for {name} should not be a parameter signature: {desc:?}"
        );
    }
ENDNEW5

$pos = index($content, $old_desc_check);
die "FATAL: old desc check not found" unless $pos >= 0;
substr($content, $pos, length($old_desc_check)) = $new_desc_check;

# Step 7: Update test 2 schema assertion block at end
my $old_test2_schema = <<'ENDOLD6';
    // Schema preservation: captured tools must carry full schemas.
    let captures = provider.captured_tools();
    for (turn_idx, captured) in captures.iter().enumerate() {
        for (i, tool) in captured.iter().enumerate() {
            let function = tool
                .get("function")
                .unwrap_or_else(|| panic!("turn {turn_idx}, tool {i}: missing function"));
            assert!(
                function.get("name").is_some(),
                "turn {turn_idx}, tool {i}: name preserved"
            );
            assert!(
                function.get("description").is_some(),
                "turn {turn_idx}, tool {i}: description preserved"
            );
            assert!(
                function.get("parameters").is_some(),
                "turn {turn_idx}, tool {i}: inputSchema/parameters preserved"
            );
        }
    }
}
ENDOLD6

my $new_test2_schema = <<'ENDNEW6';
    // Schema preservation: captured tools must carry full schemas.
    // Real schemas use native format: top-level `name`, `description`,
    // `inputSchema` (not nested under `function`).
    let captures = provider.captured_tools();
    for (turn_idx, captured) in captures.iter().enumerate() {
        for (i, tool) in captured.iter().enumerate() {
            assert!(
                tool.get("name").is_some(),
                "turn {turn_idx}, tool {i}: name preserved"
            );
            assert!(
                tool.get("description").is_some(),
                "turn {turn_idx}, tool {i}: description preserved"
            );
            assert!(
                tool.get("inputSchema").is_some(),
                "turn {turn_idx}, tool {i}: inputSchema preserved"
            );
        }
    }
}
ENDNEW6

$pos = index($content, $old_test2_schema);
die "FATAL: old test2 schema block not found" unless $pos >= 0;
substr($content, $pos, length($old_test2_schema)) = $new_test2_schema;

# Step 8: Update the three tests to use new_with_worker_prompt()
# Test 1
$content =~ s/let mut h = ReplyLoopHarness::new\(\)\.await;\n\s*let \(result, output, _ti, _to, _cr, _cw\) = h\.run\(&provider, &schemas\)\.await;/let mut h = ReplyLoopHarness::new_with_worker_prompt().await;\n    let (result, output, _ti, _to, _cr, _cw) = h.run(\&provider, \&schemas).await;/;

# Write result
open my $out, '>', $file or die "Cannot write $file: $!\n";
print $out $content;
close $out;

print "Patching complete.\n";
