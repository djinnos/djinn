use super::gate_guard::{gate_guard_edit_check, gate_guard_shell_check};
use super::workspace_helpers::{
    cargo_check_denied, classify_cargo_command, emit_edit_match_telemetry,
};
use super::*;
use djinn_core::clock::{Clock, SystemClock};
use djinn_telemetry::cargo_invocation::{self, EXIT_CANCELLED, EXIT_FAIL, EXIT_OK};

/// Default interactive-shell timeout (ms) when the caller passes no `timeout_ms`.
/// Overridable via `DJINN_SHELL_TIMEOUT_MS`. Raised well above the old 120s:
/// cold native builds routinely exceed two minutes, and a too-short ceiling
/// SIGKILLed compiles mid-flight, leaving the model to retry from cold — the
/// same guillotine the command runner already fixed.
fn default_shell_timeout_ms() -> u64 {
    std::env::var("DJINN_SHELL_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v >= 1000)
        .unwrap_or(600_000)
}

/// Minimum timeout (ms) enforced for slow native build/test commands, even when
/// the caller requests a smaller value. A cold `cargo`/`clippy`/`nextest`/`go`/
/// `pnpm` compile legitimately runs many minutes; flooring stops a low guess
/// from killing a build that is still making progress. Overridable via
/// `DJINN_SHELL_BUILD_TIMEOUT_MS`. Only ever RAISES the effective timeout.
fn build_command_floor_ms() -> u64 {
    std::env::var("DJINN_SHELL_BUILD_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v >= 1000)
        .unwrap_or(1_800_000)
}

/// Heuristic: does this command invoke a slow native build/test toolchain whose
/// cold compile can exceed the default timeout? Matches on substrings so the
/// common `cd server && cargo …` shape is covered. False positives are benign:
/// a non-build command that happens to contain the needle finishes fast anyway,
/// so the only effect is a higher (unused) ceiling.
fn is_build_command(command: &str) -> bool {
    const NEEDLES: [&str; 8] = [
        "cargo ", "nextest", "go build", "go test", "pnpm ", "npm run", "make ", "bazel ",
    ];
    NEEDLES.iter().any(|needle| command.contains(needle))
}

/// Resolve the effective shell timeout: the caller's `timeout_ms` (or the
/// default), clamped to a sane minimum, then floored up for build/test commands.
fn effective_shell_timeout_ms(requested: Option<u64>, command: &str) -> u64 {
    let base = requested.unwrap_or_else(default_shell_timeout_ms).max(1000);
    if is_build_command(command) {
        base.max(build_command_floor_ms())
    } else {
        base
    }
}

/// Structurally record exactly one cargo invocation observation from a single
/// runner terminal result.
///
/// This is the private testable seam between the process runner and the
/// telemetry contract. Exactly-once is structural: there is exactly one call
/// site in [`call_shell`], placed after the single runner return. No Drop
/// guard, no recordings in individual timeout/cancellation branches.
///
/// Mapping:
/// - `classification == None` (non-cargo command): no observation.
/// - [`crate::process::ProcessRunError::Spawn`] (child never started): no observation.
/// - Successful exit ([`crate::process::ProcessTermination::Exited`] + success): `EXIT_OK`.
/// - Nonzero exit, timeout, or post-start runner error: `EXIT_FAIL`.
/// - Handled cancellation: `EXIT_CANCELLED`.
fn finish_shell(
    classification: Option<&'static str>,
    started: std::time::Instant,
    result: &Result<crate::process::ProcessOutput, crate::process::ProcessRunError>,
    clock: &dyn Clock,
    recorder: impl Fn(&'static str, &'static str, std::time::Duration),
) {
    let Some(kind) = classification else {
        return;
    };
    let exit: &'static str = match result {
        // Spawn error: child never started — no observation.
        Err(crate::process::ProcessRunError::Spawn(_)) => return,
        // Post-start runner error (wait/reap/join): child started.
        Err(crate::process::ProcessRunError::Started(_)) => EXIT_FAIL,
        Ok(po) => match po.termination {
            // Handled cancellation: child started and was cleaned up.
            crate::process::ProcessTermination::Cancelled => EXIT_CANCELLED,
            // Timeout: child was killed by the deadline — always fail.
            crate::process::ProcessTermination::TimedOut => EXIT_FAIL,
            crate::process::ProcessTermination::Exited if po.output.status.success() => EXIT_OK,
            crate::process::ProcessTermination::Exited => EXIT_FAIL,
        },
    };
    let ended = clock.now_instant();
    let elapsed = ended.saturating_duration_since(started);
    recorder(kind, exit, elapsed);
}

pub(crate) async fn call_shell(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    worktree_path: &Path,
    session_role: Option<&str>,
    cancel: &super::super::ToolCancellation,
) -> Result<serde_json::Value, String> {
    let p: ShellParams = parse_args(arguments)?;

    // Worker AND reviewer roles: steer `cargo check`/`cargo build` to clippy
    // (warm cache). Other roles (planner/architect) don't run cargo.
    if matches!(session_role, Some("worker") | Some("reviewer"))
        && let Some(msg) = cargo_check_denied(&p.command)
    {
        return Err(msg.to_string());
    }

    // GateGuard shell policy: classify destructive commands for workers.
    // Runs after cargo steering and before any subprocess execution.
    // Non-worker roles pass through unconditionally.
    let session_id = worktree_path.display().to_string();
    gate_guard_shell_check(state, session_role, &session_id, &p.command).await?;

    let timeout_ms = effective_shell_timeout_ms(p.timeout_ms, &p.command);

    // Cross-repo shell is authorized by immutable task-run data injected into
    // AgentContext, never by process environment. Workers consume only an
    // already-mounted owner cache; migration and clone lifecycle are host-only.
    let run_dir: std::path::PathBuf = if let Some(proj) =
        p.project.as_deref().filter(|s| !s.is_empty())
    {
        let repo = ProjectRepository::new(state.db.clone(), state.event_bus.clone());
        match repo.resolve(proj).await.map_err(|e| e.to_string())? {
            Some(pid) if state.default_project_id.as_deref() != Some(pid.as_str()) => {
                let authorization = &state.read_source_authorization;
                if !authorization
                    .read_source_project_ids
                    .iter()
                    .any(|authorized_id| authorized_id == &pid)
                {
                    return Err(format!(
                        "cross-project shell denied: project {pid} is not an authorized read source"
                    ));
                }
                if authorization.owner_project_id.as_deref() != state.default_project_id.as_deref()
                {
                    return Err("cross-project shell denied: immutable owner ID is unavailable or mismatched".into());
                }
                let owner_root = authorization.owner_cache_root.as_deref().ok_or_else(|| {
                    "cross-project shell denied: authorized owner cache is not mounted".to_string()
                })?;
                let dest = owner_root
                    .join(".task-runtime")
                    .join("read-sources")
                    .join(&pid);
                if !dest.is_dir() {
                    return Err(format!(
                        "cross-project shell denied: authorized read-source cache is not mounted: {}",
                        dest.display()
                    ));
                }
                dest
            }
            _ => worktree_path.to_path_buf(),
        }
    } else {
        worktree_path.to_path_buf()
    };

    let mut cmd = if cfg!(windows) {
        let mut c = std::process::Command::new("cmd");
        c.arg("/c").arg(&p.command);
        c
    } else {
        let mut c = std::process::Command::new("bash");
        c.arg("-lc").arg(&p.command);
        c
    };

    let sandbox_scope = if run_dir == worktree_path {
        sandbox::SandboxScope::Worktree(worktree_path)
    } else {
        sandbox::SandboxScope::ReadSource {
            root: &run_dir,
            cwd: &run_dir,
        }
    };
    sandbox::SANDBOX
        .apply(sandbox_scope, &mut cmd)
        .map_err(|e| e.to_string())?;

    cmd.current_dir(&run_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::process::isolate_process_group(&mut cmd);

    // Agent-private cancellation: the child token is cancelled as soon as
    // either the session or global token fires, but the shell future is NOT
    // dropped — the cancellable runner owns the child process and runs the
    // process-group TERM/grace/KILL/reap cleanup before returning its handled
    // terminal result. We await that future to completion so the terminal
    // observation is preserved rather than lost to a dropped future.
    let child_token = tokio_util::sync::CancellationToken::new();
    let forward_token = child_token.clone();
    let session = cancel.session.clone();
    let global = cancel.global.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = session.cancelled() => forward_token.cancel(),
            _ = global.cancelled() => forward_token.cancel(),
        }
    });

    // Classify for cargo invocation telemetry. `None` means non-cargo: no
    // observation is recorded and the rest of the function is unchanged.
    let classification = classify_cargo_command(&p.command);

    // Capture monotonic start immediately before entering the runner.
    let clock = SystemClock::new();
    let started = clock.now_instant();

    let runner_result = crate::process::output_with_kill_cancellable(
        cmd,
        Duration::from_millis(timeout_ms),
        child_token,
    )
    .await;

    // Structurally finish exactly one cargo observation from the single
    // terminal value. Exactly-once is structural: one call site after the
    // single runner return — no Drop guard, no recordings in individual
    // timeout/cancellation branches.
    finish_shell(
        classification,
        started,
        &runner_result,
        &clock,
        cargo_invocation::record_seconds,
    );

    let process_output =
        runner_result.map_err(|e| format!("failed to run shell command: {}", e.into_io_error()))?;

    let output = process_output.output;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    Ok(serde_json::json!({
        "ok": output.status.success(),
        "exit_code": output.status.code(),
        "stdout": stdout,
        "stderr": stderr,
        "workdir": run_dir.display().to_string(),
    }))
}

/// Format an in-memory file body as a numbered, paginated window matching the
/// shape `call_read` returns from the worktree path. Used for mirror-backed
/// cross-repo reads (the whole blob is already in memory from `git show`).
fn numbered_window(content: &str, offset: usize, limit: usize, path: &str) -> serde_json::Value {
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    let start = offset.min(total);
    let end = start.saturating_add(limit).min(total);
    let mut numbered = String::new();
    for (i, line) in lines[start..end].iter().enumerate() {
        let line_no = start + i + 1;
        let mut l = (*line).to_string();
        if l.chars().count() > 2000 {
            l = l.chars().take(2000).collect();
        }
        numbered.push_str(&format!("{:>6}\t{}\n", line_no, l));
    }
    serde_json::json!({
        "path": path,
        "offset": start,
        "limit": limit,
        "total_lines": total,
        "has_more": end < total,
        "content": numbered,
    })
}

/// Hard byte budget for a single `call_read` scan. We never read more than
/// this many bytes off disk for one read, even when the requested window
/// would imply scanning further — a multi-GB file must not OOM the worker.
/// Matches the order of magnitude of the other tool-output truncation
/// budgets in this crate (see `output_stash::MAX_TOTAL_BYTES`, 5 MB).
const MAX_READ_BYTES: usize = 8 * 1024 * 1024; // 8 MiB

pub(crate) async fn call_read(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    worktree_path: &Path,
) -> Result<serde_json::Value, String> {
    use tokio::io::AsyncBufReadExt;

    let p: ReadParams = parse_args(arguments)?;

    // Cross-repo read: when `project` names a DIFFERENT registered project than
    // the task's own, serve read-only from that repo's bare mirror (no clone).
    // The task's own project keeps reading the live worktree (your branch).
    if let Some(proj) = p.project.as_deref().filter(|s| !s.is_empty()) {
        let repo = ProjectRepository::new(state.db.clone(), state.event_bus.clone());
        let resolved = repo.resolve(proj).await.map_err(|e| e.to_string())?;
        match resolved {
            Some(pid) if state.default_project_id.as_deref() != Some(pid.as_str()) => {
                let git_ref = repo
                    .get_default_branch(&pid)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "HEAD".to_string());
                let content = crate::repo_access::read_file(&pid, &git_ref, &p.file_path).await?;
                let offset = p.offset.unwrap_or(0);
                let limit = p.limit.unwrap_or(2000).min(2000);
                return Ok(numbered_window(&content, offset, limit, &p.file_path));
            }
            // Same as the task project (or unresolvable → fall through to the
            // worktree path so the existing not-found suggestions still apply).
            _ => {}
        }
    }

    let path = resolve_path(&p.file_path, worktree_path);

    let file = tokio::fs::File::open(&path).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            let parent = path.parent().unwrap_or(worktree_path);
            let suggestions = std::fs::read_dir(parent)
                .ok()
                .into_iter()
                .flat_map(|it| it.filter_map(Result::ok))
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|name| !name.is_empty())
                .take(10)
                .collect::<Vec<_>>();
            if suggestions.is_empty() {
                format!("file not found: {}", path.display())
            } else {
                format!(
                    "file not found: {}. similar filenames: {}",
                    path.display(),
                    suggestions.join(", ")
                )
            }
        } else {
            format!("read failed: {e}")
        }
    })?;

    let offset = p.offset.unwrap_or(0);
    let limit = p.limit.unwrap_or(2000).min(2000);

    // We only need the window [offset, offset+limit). Stream line by line and
    // stop once we've collected one line past the window — enough to know
    // whether there is `has_more` content without reading the rest of a huge
    // file. A hard byte budget caps the scan so a pathologically large file
    // (or a binary blob with no newlines) can never balloon memory.
    let want_lines = offset.saturating_add(limit);
    // Read one extra line beyond the window so `has_more` is exact in the
    // common (within-budget) case.
    let scan_target = want_lines.saturating_add(1);

    let mut reader = tokio::io::BufReader::new(file);
    // Each entry is (line_text, byte_offset_after_line). We track the byte
    // offset where each line starts separately in `line_byte_offsets` so we
    // can compute accurate `ReadCoverage::Range` metadata after slicing.
    let mut all_lines: Vec<(String, u64)> = Vec::new();
    let mut line_byte_offsets: Vec<u64> = Vec::new();
    let mut scanned_bytes: usize = 0;
    let mut has_more_beyond_window = false;
    let mut truncated_by_budget = false;

    loop {
        // Stop reading the moment we have the window plus a single lookahead
        // line: we never need to materialize the rest of the file.
        if all_lines.len() >= scan_target {
            has_more_beyond_window = true;
            break;
        }

        let mut buf: Vec<u8> = Vec::new();
        let line_start = scanned_bytes as u64;
        let n = reader
            .read_until(b'\n', &mut buf)
            .await
            .map_err(|e| format!("read failed: {e}"))?;
        if n == 0 {
            break; // EOF — no more lines
        }
        let line_byte_len = n;
        scanned_bytes = scanned_bytes.saturating_add(n);

        // Binary detection from the streamed chunk: a NUL byte means this is
        // not a text file. Bail before dumping bytes into the response.
        if buf.contains(&0) {
            return Err(format!("refusing to read binary file: {}", path.display()));
        }

        // Decode lossily and strip the trailing newline(s), mirroring the old
        // `str::lines()` behaviour.
        let mut line = String::from_utf8_lossy(&buf).into_owned();
        while line.ends_with('\n') || line.ends_with('\r') {
            line.pop();
        }
        if line.chars().count() > 2000 {
            line = line.chars().take(2000).collect::<String>();
        }
        line_byte_offsets.push(line_start);
        all_lines.push((line, line_start + line_byte_len as u64));

        // Byte budget: stop scanning once we've consumed the cap. If there's
        // still content on disk, surface it as truncation rather than
        // silently dropping the tail.
        if scanned_bytes >= MAX_READ_BYTES {
            use tokio::io::AsyncReadExt;
            let mut probe = [0u8; 1];
            let read = reader
                .read(&mut probe)
                .await
                .map_err(|e| format!("read failed: {e}"))?;
            if read > 0 {
                truncated_by_budget = true;
                has_more_beyond_window = true;
            }
            break;
        }
    }

    let total_scanned = all_lines.len();
    let start = offset.min(total_scanned);
    let end = start.saturating_add(limit).min(total_scanned);

    let mut numbered = String::new();
    for (i, (line, _byte_end)) in all_lines[start..end].iter().enumerate() {
        let line_no = start + i + 1;
        numbered.push_str(&format!("{:>6}\t{}\n", line_no, line));
    }
    if truncated_by_budget {
        numbered.push_str(&format!(
            "\n[file too large: truncated at {} MiB; remaining content not shown]\n",
            MAX_READ_BYTES / (1024 * 1024)
        ));
    }

    // `has_more` is true if there's content past the scanned window, or if the
    // requested window didn't reach the end of what we scanned.
    let has_more = has_more_beyond_window || end < total_scanned;

    // Compute read coverage metadata from the actual arguments and result.
    // A read is full-file coverage only when the worker received all content
    // from the start (offset 0), there are no remaining pages, and no
    // byte-budget truncation occurred.
    let coverage = if offset == 0 && !has_more && !truncated_by_budget {
        crate::file_time::ReadCoverage::Full
    } else {
        // Record the byte range of the window actually returned.
        let cov_start = if start < end {
            line_byte_offsets[start]
        } else {
            // Empty window: point at EOF offset.
            scanned_bytes as u64
        };
        let cov_end = if start < end {
            Some(all_lines[end - 1].1)
        } else {
            None
        };
        crate::file_time::ReadCoverage::Range {
            start: cov_start,
            end: cov_end,
        }
    };

    state
        .file_time
        .read_with_coverage(
            &worktree_path.display().to_string(),
            &path,
            coverage,
            truncated_by_budget,
        )
        .await?;

    Ok(serde_json::json!({
        "path": path.display().to_string(),
        "offset": start,
        "limit": limit,
        "total_lines": total_scanned,
        "has_more": has_more,
        "content": numbered,
    }))
}

pub(crate) async fn call_write(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    worktree_path: &Path,
    project_id: Option<&str>,
    #[allow(unused_variables)] session_task_id: Option<&str>,
    session_role: Option<&str>,
) -> Result<serde_json::Value, String> {
    let p: WriteParams = parse_args(arguments)?;
    let path = resolve_path(&p.path, worktree_path);

    // Ensure path is within worktree
    ensure_path_within_worktree(&path, worktree_path)?;

    let touched_rel = relative_to_worktree(&path, worktree_path);

    state
        .file_time
        .with_lock(&path, async {
            if path.exists() {
                state
                    .file_time
                    .assert(&worktree_path.display().to_string(), &path)
                    .await
                    .map_err(|e| match e.as_str() {
                        _ if e.starts_with(
                            "file must be read before modification in this session:",
                        ) =>
                        {
                            format!(
                                "You must read the file {} before overwriting it. Use the read tool first",
                                path.display()
                            )
                        }
                        _ if e.starts_with(
                            "file was modified since last read in this session:",
                        ) =>
                        {
                            format!(
                                "File {} has been modified since last read. Please read it again.",
                                path.display()
                            )
                        }
                        _ => e,
                    })?;

                    // GateGuard: enforce worker read-coverage gate before
                    // mutation. call_write overwrites the entire file, so the
                    // mutation span is 0..file_size.
                    let file_size = tokio::fs::metadata(&path)
                        .await
                        .map(|m| m.len() as usize)
                        .unwrap_or(0);
                    gate_guard_edit_check(
                        state,
                        session_role,
                        &worktree_path.display().to_string(),
                        &path,
                        0..file_size,
                    )
                    .await?;
                }

                if let Some(parent) = path.parent() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|e| format!("create dirs failed: {e}"))?;
                }
                tokio::fs::write(&path, &p.content)
                .await
                .map_err(|e| format!("write failed: {e}"))?;

            // Invalidate the read record: the model's in-context view of this
            // file is now stale relative to disk. A subsequent edit must
            // re-read first (forced via the "modified since last read" /
            // "must be read before modification" guard in `assert`), so it
            // patches against current content rather than its stale view.
            state
                .file_time
                .invalidate(&worktree_path.display().to_string(), &path)
                .await;

            state.lsp.touch_file(worktree_path, &path, true).await;
            let diag_xml = state.lsp.diagnostics_xml(worktree_path).await;

            let response = serde_json::json!({
                "ok": true,
                "path": path.display().to_string(),
                "bytes": p.content.len(),
                "diagnostics": diag_xml,
            });
            let response = match (project_id, touched_rel.as_deref()) {
                (Some(pid), Some(rel)) => {
                    enrich_with_related_files(response, state, pid, &[rel.to_string()]).await
                }
                _ => response,
            };
            let touched: Vec<String> = touched_rel.iter().cloned().collect();
            let response =
                maybe_append_pitfall_hint(response, state, worktree_path, project_id, &touched)
                    .await;
            Ok(response)
        })
        .await
}

pub(crate) async fn call_edit(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    worktree_path: &Path,
    project_id: Option<&str>,
    session_task_id: Option<&str>,
    session_role: Option<&str>,
) -> Result<serde_json::Value, String> {
    let p: EditParams = parse_args(arguments)?;
    let path = resolve_path(&p.path, worktree_path);

    // Ensure path is within worktree
    ensure_path_within_worktree(&path, worktree_path)?;

    let touched_rel = relative_to_worktree(&path, worktree_path);

    state
        .file_time
        .with_lock(&path, async {
            state
                .file_time
                .assert(&worktree_path.display().to_string(), &path)
                .await
                .map_err(|e| match e.as_str() {
                    _ if e
                        .starts_with("file must be read before modification in this session:") =>
                    {
                        format!(
                            "You must read the file {} before editing it. Use the read tool first",
                            path.display()
                        )
                    }
                    _ if e.starts_with("file was modified since last read in this session:") => {
                        format!(
                            "File {} has been modified since last read. Please read it again.",
                            path.display()
                        )
                    }
                    _ => e,
                })?;

            let content = tokio::fs::read_to_string(&path)
                .await
                .map_err(|e| format!("read failed: {e}"))?;

            let metadata = find_match(&content, &p.old_text);

            // Extract bounded-cardinality path extension for telemetry.
            let path_ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_string();

            // Compute matched_bytes for telemetry (only meaningful for Success).
            let matched_bytes = metadata.byte_range.map(|br| br.end - br.start);

            match metadata.outcome {
                MatchOutcome::Success => {
                    // Structured edit_match metadata.
                    let byte_range = metadata.byte_range.expect("success has byte range");
                    let matched_bytes_val = byte_range.end - byte_range.start;
                    let edit_match = serde_json::json!({
                        "strategy": metadata.strategy.as_str(),
                        "matched_byte_range": [byte_range.start, byte_range.end],
                        "matched_line_range": metadata.line_range.map(|lr| [lr.start, lr.end]),
                        "old_bytes": p.old_text.len(),
                        "new_bytes": p.new_text.len(),
                        "matched_bytes": matched_bytes_val,
                        "reindented": metadata.reindented,
                        "unicode_splice": metadata.unicode_splice.map(|s| match s {
                            UnicodeSpliceStatus::Clean => "clean",
                            UnicodeSpliceStatus::Adjusted => "adjusted",
                        }),
                        "note": match_note_for(metadata.strategy),
                    });

                    // Emit telemetry BEFORE GateGuard check so match-outcome
                    // telemetry is always recorded for successful candidates.
                    emit_edit_match_telemetry(
                        &metadata,
                        session_task_id,
                        session_task_id,
                        session_role,
                        &path_ext,
                        p.old_text.len(),
                        p.new_text.len(),
                        matched_bytes,
                    );

                    // GateGuard: enforce worker read-coverage gate before mutation.
                    gate_guard_edit_check(
                        state,
                        session_role,
                        &worktree_path.display().to_string(),
                        &path,
                        byte_range.start..byte_range.end,
                    )
                    .await?;

                    let new_content = apply_match(&content, &p.new_text, &metadata);
                    tokio::fs::write(&path, &new_content)
                        .await
                        .map_err(|e| format!("write failed: {e}"))?;

                    // Invalidate the read record so a subsequent edit must re-read
                    // first — see the matching note in `call_write`. Prevents the
                    // apply_patch/edit "context mismatch" loop where the model patches
                    // against its pre-edit view of the file.
                    state
                        .file_time
                        .invalidate(&worktree_path.display().to_string(), &path)
                        .await;

                    state.lsp.touch_file(worktree_path, &path, true).await;
                    let diag_xml = state.lsp.diagnostics_xml(worktree_path).await;

                    let match_note = match_note_for(metadata.strategy);
                    let mut result = serde_json::json!({
                        "ok": true,
                        "path": path.display().to_string(),
                        "diagnostics": diag_xml,
                    });
                    if let Some(note) = match_note {
                        result["match_note"] = serde_json::Value::String(note);
                    }

                    result["edit_match"] = edit_match;

                    let result = match (project_id, touched_rel.as_deref()) {
                        (Some(pid), Some(rel)) => {
                            enrich_with_related_files(result, state, pid, &[rel.to_string()]).await
                        }
                        _ => result,
                    };
                    let touched: Vec<String> = touched_rel.iter().cloned().collect();
                    let result = maybe_append_pitfall_hint(
                        result,
                        state,
                        worktree_path,
                        project_id,
                        &touched,
                    )
                    .await;
                    Ok(result)
                }
                MatchOutcome::Ambiguous => {
                    // Emit telemetry before returning error.
                    emit_edit_match_telemetry(
                        &metadata,
                        session_task_id,
                        session_task_id,
                        session_role,
                        &path_ext,
                        p.old_text.len(),
                        p.new_text.len(),
                        matched_bytes,
                    );

                    // Structured details for ambiguity.
                    let details = serde_json::json!({
                        "edit_match": {
                            "strategy": metadata.strategy.as_str(),
                            "outcome": "ambiguous",
                            "candidate_count": metadata.candidate_count,
                        }
                    });
                    Err(format!(
                        "old_text appears {} times in file (must be unique): {} {}",
                        metadata.candidate_count,
                        path.display(),
                        details,
                    ))
                }
                MatchOutcome::NoMatch => {
                    // Emit telemetry before returning error.
                    emit_edit_match_telemetry(
                        &metadata,
                        session_task_id,
                        session_task_id,
                        session_role,
                        &path_ext,
                        p.old_text.len(),
                        p.new_text.len(),
                        matched_bytes,
                    );

                    let details = serde_json::json!({
                        "edit_match": {
                            "strategy": metadata.strategy.as_str(),
                            "outcome": "no_match",
                            "nearest_miss": metadata.nearest_miss,
                        }
                    });
                    Err(format!(
                        "old_text not found in file: {} {}",
                        path.display(),
                        details,
                    ))
                }
                MatchOutcome::GuardRejected => {
                    // Emit telemetry before returning error.
                    emit_edit_match_telemetry(
                        &metadata,
                        session_task_id,
                        session_task_id,
                        session_role,
                        &path_ext,
                        p.old_text.len(),
                        p.new_text.len(),
                        matched_bytes,
                    );

                    let details = serde_json::json!({
                        "edit_match": {
                            "strategy": metadata.strategy.as_str(),
                            "outcome": "guard_rejected",
                            "guard_reason": metadata.guard_rejected_reason,
                        }
                    });
                    Err(format!(
                        "old_text match rejected by safety guard{}: {} {}",
                        metadata
                            .guard_rejected_reason
                            .map(|r| format!(" ({r})"))
                            .unwrap_or_default(),
                        path.display(),
                        details,
                    ))
                }
            }
        })
        .await
}

pub(crate) async fn call_apply_patch(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    worktree_path: &Path,
    project_id: Option<&str>,
    #[allow(unused_variables)] session_task_id: Option<&str>,
    session_role: Option<&str>,
) -> Result<serde_json::Value, String> {
    let p: ApplyPatchParams = parse_args(arguments)?;

    // Parse the custom patch format
    let parsed = crate::patch::parse_patch(&p.patch)?;

    let worktree_key = worktree_path.display().to_string();

    // Validate all paths are within worktree and assert FileTime for updates/deletes
    for op in &parsed.operations {
        let raw_path = op.path();
        let resolved = resolve_path(raw_path, worktree_path);
        ensure_path_within_worktree(&resolved, worktree_path)?;

        match op {
            crate::patch::FileOp::Update { .. } | crate::patch::FileOp::Delete { .. } => {
                state
                    .file_time
                    .assert(&worktree_key, &resolved)
                    .await
                    .map_err(|e| {
                        if e.starts_with("file must be read before modification in this session:") {
                            format!(
                                "You must read the file {} before editing it. \
                                 Use the read tool first",
                                resolved.display()
                            )
                        } else if e
                            .starts_with("file was modified since last read in this session:")
                        {
                            format!(
                                "File {} has been modified since last read. \
                                 Please read it again.",
                                resolved.display()
                            )
                        } else {
                            e
                        }
                    })?;

                // GateGuard: enforce worker read-coverage gate before
                // mutation. Conservative: require full-file coverage for
                // update/delete since exact touched spans are not proven
                // from the patch parser.
                gate_guard_edit_check(state, session_role, &worktree_key, &resolved, 0..usize::MAX)
                    .await?;
            }
            crate::patch::FileOp::Add { .. } => {
                // New files don't need FileTime assertion
            }
        }
    }

    // Apply all patch operations
    let results = crate::patch::apply_patch(&parsed, worktree_path).await?;

    // Update FileTime and notify LSP for each affected file
    let mut affected = Vec::new();
    for (file_path, action) in &results {
        if *action != "deleted" {
            // Invalidate rather than re-record: the patch changed this file,
            // so the model's in-context view is now stale. Forcing a re-read
            // before the next patch fixes the apply_patch "context mismatch"
            // loop (model keeps patching against its pre-patch view). See the
            // note in `call_write`.
            state.file_time.invalidate(&worktree_key, file_path).await;
            state.lsp.touch_file(worktree_path, file_path, true).await;
        }
        affected.push(serde_json::json!({
            "path": file_path.display().to_string(),
            "action": action,
        }));
    }

    let diag_xml = state.lsp.diagnostics_xml(worktree_path).await;

    let response = serde_json::json!({
        "ok": true,
        "files": affected,
        "diagnostics": diag_xml,
    });

    // Compute the union of related files across every touched path —
    // non-deleted only (deletes can't meaningfully nudge related edits).
    let touched_rel: Vec<String> = results
        .iter()
        .filter(|(_, action)| *action != "deleted")
        .filter_map(|(file_path, _)| relative_to_worktree(file_path, worktree_path))
        .collect();

    let response = match project_id {
        Some(pid) if !touched_rel.is_empty() => {
            enrich_with_related_files(response, state, pid, &touched_rel).await
        }
        _ => response,
    };
    let response =
        maybe_append_pitfall_hint(response, state, worktree_path, project_id, &touched_rel).await;
    Ok(response)
}

/// Cross-repo code search via `git grep` against bare mirrors (zero clones).
/// `project` scopes to one repo; omitted (or `"*"`) fans out across ALL
/// registered projects — the org-wide "who calls this?" case.
pub(crate) async fn call_code_search(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: CodeSearchParams = parse_args(arguments)?;
    if p.query.trim().is_empty() {
        return Err("code_search requires a non-empty query".to_string());
    }
    let max = p.max_results.unwrap_or(100).min(500);
    let ignore_case = p.ignore_case.unwrap_or(false);
    let repo = ProjectRepository::new(state.db.clone(), state.event_bus.clone());

    // Resolve the target project set.
    let all = p
        .project
        .as_deref()
        .map(|s| s.trim())
        .is_none_or(|s| s.is_empty() || s == "*");
    let projects: Vec<(String, String)> = if all {
        repo.list()
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|proj| {
                (
                    proj.id,
                    format!("{}/{}", proj.github_owner, proj.github_repo),
                )
            })
            .collect()
    } else {
        let proj = p.project.as_deref().unwrap();
        match repo.resolve(proj).await.map_err(|e| e.to_string())? {
            Some(id) => {
                let slug = repo
                    .get(&id)
                    .await
                    .ok()
                    .flatten()
                    .map(|x| format!("{}/{}", x.github_owner, x.github_repo))
                    .unwrap_or_else(|| id.clone());
                vec![(id, slug)]
            }
            None => return Err(format!("project not found: {proj}")),
        }
    };

    let mut groups = Vec::new();
    let mut skipped = Vec::new();
    let mut total = 0usize;
    for (pid, slug) in projects {
        if !crate::repo_access::mirror_exists(&pid) {
            skipped.push(slug);
            continue;
        }
        let git_ref = repo
            .get_default_branch(&pid)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| "HEAD".to_string());
        match crate::repo_access::grep(
            &pid,
            &git_ref,
            &p.query,
            p.path.as_deref(),
            ignore_case,
            max,
        )
        .await
        {
            Ok(hits) if hits.is_empty() => {}
            Ok(hits) => {
                total += hits.len();
                groups.push(serde_json::json!({ "project": slug, "matches": hits }));
            }
            Err(e) => {
                tracing::warn!(project = %slug, error = %e, "code_search: grep failed; skipping");
                skipped.push(slug);
            }
        }
    }

    Ok(serde_json::json!({
        "total_matches": total,
        "results": groups,
        "skipped": skipped,
        "truncated_per_project_at": max,
    }))
}

/// F2: best-effort just-in-time pitfall hint. On the FIRST write/edit/
/// apply_patch of a session (when `DJINN_JIT_PITFALLS_ROLLOUT` explicitly
/// selects `enabled`/`cohort`/`staging`, or the legacy migration
/// `DJINN_JIT_PITFALLS=1` opt-in is present), run a scoped pitfall/pattern
/// search over the touched paths and append the top-2 as a transient
/// `jit_pitfalls` field on the response JSON. A miss, an error, or a non-first
/// modification leaves `response` untouched. Default-off and explicit
/// kill-switch decisions record distinct telemetry outcomes but do no DB search
/// and append no hint.
async fn maybe_append_pitfall_hint(
    response: serde_json::Value,
    state: &AgentContext,
    worktree_path: &Path,
    project_id: Option<&str>,
    touched_paths: &[String],
) -> serde_json::Value {
    let session_id = worktree_path.display().to_string();
    match super::jit_pitfalls::maybe_pitfall_hint(state, &session_id, project_id, touched_paths)
        .await
    {
        Some(block) => {
            let mut response = response;
            if let Some(obj) = response.as_object_mut() {
                obj.insert("jit_pitfalls".to_string(), serde_json::Value::String(block));
            }
            response
        }
        None => response,
    }
}

/// Resolve the path to a repo-relative form for coupling lookup. Paths
/// outside the worktree (e.g. absolute paths not under `worktree_path`)
/// return `None` — the nudge is best-effort, a miss just drops the
/// enrichment.
fn relative_to_worktree(path: &Path, worktree_path: &Path) -> Option<String> {
    path.strip_prefix(worktree_path)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Append a `related_files` array to the write response JSON, populated
/// by the file-keyed coupling query with project-level exclusions
/// applied. Design notes:
///
/// * **Thresholds.** Returns the top 5 peers with `co_edits >= 2`. A
///   single co-edit is a random commit, not coupling — keeping the
///   single-shot peers in would dilute the signal on day-one projects.
/// * **Multi-file writes** (apply_patch): takes the union of peers
///   across every touched path, dedups, picks the 5 highest-count
///   entries (higher co_edits wins on tie).
/// * **Error swallowing.** Every step is best-effort. On any failure
///   (DB blip, no coupling data yet, project_id resolves to nothing) we
///   log at warn level and return the response unchanged — the write
///   itself has already succeeded and the user should never see a
///   coupling error masking that.
/// * **Reads NOT nudged.** Reads are 10–50× more frequent than writes;
///   flooding read responses with coupling noise dilutes the signal.
async fn enrich_with_related_files(
    mut response: serde_json::Value,
    state: &AgentContext,
    project_id: &str,
    touched_paths: &[String],
) -> serde_json::Value {
    use djinn_control_plane::tools::graph_exclusions::load_project_exclusion_matcher;
    use djinn_db::CommitFileChangeRepository;

    if touched_paths.is_empty() {
        return response;
    }

    let matcher = load_project_exclusion_matcher(&state.db, &state.event_bus, project_id).await;
    let repo = CommitFileChangeRepository::new(state.db.clone());

    // (file_path -> co_edits) — union across touched paths, keeping the
    // highest observed co_edits count per path.
    use std::collections::HashMap;
    let mut merged: HashMap<String, i64> = HashMap::new();
    let touched_set: std::collections::HashSet<&str> =
        touched_paths.iter().map(|s| s.as_str()).collect();

    for touched in touched_paths {
        match repo.top_coupled(project_id, touched, 50).await {
            Ok(rows) => {
                for row in rows {
                    // Skip the files we just touched — "related to
                    // itself" is noise.
                    if touched_set.contains(row.file_path.as_str()) {
                        continue;
                    }
                    if matcher.excludes_path(&row.file_path) {
                        continue;
                    }
                    if row.co_edit_count < 2 {
                        continue;
                    }
                    let entry = merged.entry(row.file_path).or_insert(0);
                    if row.co_edit_count > *entry {
                        *entry = row.co_edit_count;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    project_id = %project_id,
                    touched = %touched,
                    error = %e,
                    "enrich_with_related_files: coupling query failed; skipping",
                );
            }
        }
    }

    if merged.is_empty() {
        return response;
    }
    let mut related: Vec<(String, i64)> = merged.into_iter().collect();
    // Higher co_edits wins on tie (brief §C.4); stable by path for
    // deterministic output in tests.
    related.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    related.truncate(5);

    let value = serde_json::Value::Array(
        related
            .into_iter()
            .map(|(path, co_edits)| {
                serde_json::json!({
                    "path": path,
                    "co_edits": co_edits,
                })
            })
            .collect(),
    );
    if let Some(obj) = response.as_object_mut() {
        obj.insert("related_files".to_string(), value);
    }
    response
}

#[cfg(all(test, unix))]
#[path = "workspace_cargo_outcome_tests.rs"]
mod cargo_outcome_tests;

#[cfg(test)]
mod timeout_tests {
    use super::{effective_shell_timeout_ms, is_build_command};

    #[test]
    fn build_commands_are_detected() {
        assert!(is_build_command("cd server && cargo check -p djinn-db"));
        assert!(is_build_command("cargo clippy --all-features"));
        assert!(is_build_command("cargo nextest run"));
        assert!(is_build_command("go test ./..."));
        assert!(is_build_command("pnpm install"));
        assert!(is_build_command("make build"));
        assert!(!is_build_command("ls -la"));
        assert!(!is_build_command("git status"));
        assert!(!is_build_command("grep -r foo src"));
    }

    #[test]
    fn build_commands_are_floored_above_a_small_request() {
        // A 120s request for a cold compile must be raised to the build floor,
        // not honored verbatim (that was the SIGKILL-mid-build bug).
        let got = effective_shell_timeout_ms(Some(120_000), "cargo build");
        assert!(got >= 1_800_000, "build floor not applied: {got}");
    }

    #[test]
    fn non_build_commands_keep_the_requested_timeout() {
        assert_eq!(effective_shell_timeout_ms(Some(5_000), "echo hi"), 5_000);
    }

    #[test]
    fn requests_are_clamped_to_a_sane_minimum() {
        assert_eq!(effective_shell_timeout_ms(Some(10), "echo hi"), 1000);
    }

    #[test]
    fn a_large_explicit_build_timeout_is_preserved() {
        let got = effective_shell_timeout_ms(Some(3_600_000), "cargo test");
        assert_eq!(got, 3_600_000);
    }
}
