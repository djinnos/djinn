// djinn:allow-oversize
//
// The worktree tool handlers: `read`, `write`, `edit`, `apply_patch`,
// `code_search`, `shell`. They share the FileTime read-coverage bookkeeping and
// the GateGuard edit checks, so they are kept together deliberately. Splitting
// them is a behaviour-bearing refactor of production dispatch and is out of
// scope for the read-coverage fix that pushed this file over MAX_BYTES.

use super::gate_guard::{gate_guard_edit_check, gate_guard_shell_check};
use super::shell_exec::{effective_shell_timeout_ms, finish_shell};
use super::size_nudge::maybe_append_size_nudge;
use super::workspace_helpers::{
    cargo_check_denied, classify_cargo_command, emit_edit_match_telemetry,
};
use super::*;
use djinn_core::clock::{Clock, SystemClock};
use djinn_runtime::RoleResourceClass;
use djinn_telemetry::cargo_invocation;

pub(crate) async fn call_shell(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    worktree_path: &Path,
    session_role: Option<&str>,
    cancel: &super::super::ToolCancellation,
) -> Result<serde_json::Value, String> {
    let p: ShellParams = parse_args(arguments)?;

    // The session's resource class, from the ONE classifier
    // (`djinn_runtime::RoleResourceClass`) that `djinn-k8s` pod sizing and
    // `djinn-coordinator` dispatch admission already share. A missing or
    // unrecognized role fails safe to build-capable, exactly as it does there.
    //
    // This replaced a second, contradictory local table
    // (`matches!(session_role, Some("worker") | Some("reviewer"))`) that
    // disagreed with `spec.rs` in both directions: it classed Reviewer with
    // Worker, and its comment asserted "planner/architect don't run cargo"
    // while `spec.rs` classes Architect as build-capable.
    let class = RoleResourceClass::for_role_name(session_role.unwrap_or_default());
    // Publish it for the recorders that are deliberately role-blind (the
    // invocation-lease shadow counter). A task-run pod hosts one role, so this
    // is a constant after the first shell call and cannot race meaningfully.
    djinn_telemetry::role_class::observe(class.as_str());

    // Steer `cargo check`/`cargo build` to clippy (warm cache) for EVERY role,
    // not a hand-picked subset. The steer is a property of the pod's cache
    // shape, not of the role: the cache is clippy-warmed, so a `cargo check`
    // from any session cold-builds the workspace. Keying it on the class would
    // reintroduce the same false premise this PR removes — that a Light role
    // never compiles. Measured 2026-07-25, 5.5% of light task-run sessions did,
    // and commit 1719ef8c3 recorded a reviewer burning ~12 minutes on exactly
    // this cold `cargo check`. `class` is carried for telemetry only.
    if let Some(msg) = cargo_check_denied(&p.command) {
        return Err(msg.to_string());
    }

    // GateGuard shell policy: classify destructive commands for workers, plus
    // the advisory build-drift soft gate. Runs after cargo steering and before
    // any subprocess execution. Non-worker roles pass through unconditionally.
    gate_guard_shell_check(state, session_role, worktree_path, &p.command).await?;

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
                // The injected root is exactly the owner cache
                // (`.task-runtime/read-sources`), never the project root.
                let dest = owner_root.join(&pid);
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

    // Cargo classification is observation-only.
    //
    // Two real execution paths, and BOTH must work in production (task grkq):
    //
    //   * `Some(launch)` — the cgroup-launcher sidecar came up and the worker
    //     completed its handshake, so the command is launched by the broker into
    //     a delegated cgroup leaf and is eligible for a CPU lease lift.
    //   * `None` — no launcher (it is not rendered by default, the pod predates
    //     it, this is a local/non-pod run, or the handshake failed closed). The
    //     command runs in-process at whatever quota the pod itself has, i.e.
    //     unleased. This arm used to be `#[cfg(test)]`-only, with production
    //     returning "broker-backed shell launch context is not configured" —
    //     which turned any launcher problem into EVERY shell command failing,
    //     while the surrounding doc comments claimed a fallback existed. It is
    //     now a genuine production path; degrading to unleased execution is the
    //     documented contract.
    let runner_result = if let Some(launch) = state.shell_launch.as_ref() {
        launch
            .runner()
            .output(
                cmd,
                launch.invocation(Duration::from_millis(timeout_ms)),
                child_token,
            )
            .await
            .map(|output| output.process)
            .map_err(|error| {
                crate::process::ProcessRunError::Started(std::io::Error::other(format!(
                    "lease invocation failed: {error:?}"
                )))
            })
    } else {
        // Put the child in its own process group BEFORE spawning. The in-process
        // runner's timeout/cancellation cleanup signals `-pgid`, so without this
        // the child shares the worker's group and the whole TERM/grace/KILL
        // sequence targets a process group that does not exist — a timed-out or
        // cancelled command would simply keep running.
        crate::process::isolate_process_group(&mut cmd);
        // The launcher-free path must not inherit the worker's ambient
        // environment. The broker validates a complete `CommandSpec`; mirror
        // that boundary here before the direct `spawn`.
        crate::environment::clear_and_admit_child_environment(&mut cmd)
            .map_err(|error| error.to_string())?;
        crate::process::output_with_kill_cancellable(
            cmd,
            Duration::from_millis(timeout_ms),
            child_token,
        )
        .await
    };

    // Structurally finish exactly one cargo observation from the single
    // terminal value. Exactly-once is structural: one call site after the
    // single runner return — no Drop guard, no recordings in individual
    // timeout/cancellation branches.
    finish_shell(
        classification,
        class.as_str(),
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

/// Headroom reserved inside [`crate::output_stash::MAX_TOOL_RESULT_CHARS`] for
/// everything in a read result that is *not* the numbered listing: the JSON
/// envelope's keys, the path value, and the trailing truncation notes appended
/// to `content` after the budgeted listing has been built.
const READ_RESULT_RESERVE_CHARS: usize = 1_024;

/// Number of characters needed to encode `s` as the body of a JSON string.
///
/// A read result is handed to the model only after
/// `output_stash::render_tool_result` serializes it as JSON, so the budget that
/// actually matters is the *escaped* size — a numbered listing spends two
/// characters on every `\n` and `\t` it contains.
fn json_escaped_len(s: &str) -> usize {
    s.chars()
        .map(|c| match c {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{08}' | '\u{0c}' => 2,
            c if (c as u32) < 0x20 => 6,
            c => c.len_utf8(),
        })
        .sum()
}

/// Escaped-character budget available to a read result's `content` field before
/// the downstream tool-result clamp would fire.
///
/// Derived from the single shared clamp constant so the two can never drift.
/// This does **not** widen the clamp: it makes the read handler stop at a line
/// boundary the clamp will accept, so the window the model receives is the
/// window the handler recorded coverage for.
pub(crate) fn read_content_budget() -> usize {
    crate::output_stash::MAX_TOOL_RESULT_CHARS.saturating_sub(READ_RESULT_RESERVE_CHARS)
}

/// Hard ceiling on the number of lines one `read` will return, and the default
/// when the caller does not ask for a window.
///
/// Named because it is a contract, not an implementation detail: `size_nudge`
/// derives "how many reads does this file cost?" from this exact value, so a
/// literal here and a literal there would be a drift bug waiting to happen.
pub(crate) const READ_MAX_LINES: usize = 2000;

/// The extension tool name whose results record read coverage.
const READ_TOOL_NAME: &str = "read";

/// Recover the resolved file path from an already-rendered `read` result.
///
/// `call_read` returns a JSON object whose `path` is
/// `resolved_path.display().to_string()` — the exact key `FileTime` records
/// under — and `render_tool_result` hands that object to the model as pretty
/// JSON. Parsing it back is therefore an identity lookup on the payload the
/// model was going to receive, not a heuristic on prose.
///
/// Returns `None` when the text is not the read handler's own envelope (e.g. a
/// result some other layer already replaced), in which case there is nothing to
/// downgrade.
fn externalized_read_path(rendered: &str) -> Option<std::path::PathBuf> {
    serde_json::from_str::<serde_json::Value>(rendered)
        .ok()?
        .get("path")?
        .as_str()
        .map(std::path::PathBuf::from)
}

/// Downgrade the read-coverage record behind a tool result that the per-turn
/// inline-character budget re-externalized after the handler recorded it.
///
/// `call_read` records coverage for the window it emits, sized against
/// `output_stash::MAX_TOOL_RESULT_CHARS`. That is the last clamp *inside* the
/// handler, but not the last clamp overall: `djinn-slot`'s turn budget runs
/// after every tool in the turn has been dispatched and can replace a rendered
/// result with a stash stub. Nothing of a numbered listing survives that stub
/// (its preview is a line-aware head/tail split and the listing is one giant
/// JSON line), so the record must stop claiming coverage the model never got.
///
/// Anything that is not a `read` result carries no coverage and is ignored.
pub(crate) async fn downgrade_externalized_read_coverage(
    state: &AgentContext,
    tool_name: &str,
    rendered: &str,
    worktree_path: &Path,
) {
    if tool_name != READ_TOOL_NAME {
        return;
    }
    let Some(path) = externalized_read_path(rendered) else {
        tracing::warn!(
            worktree = %worktree_path.display(),
            "read result externalized by the turn budget carried no resolvable path; \
             read coverage could not be downgraded"
        );
        return;
    };
    // Cross-repo (mirror-backed) reads never record coverage, and a file read
    // twice in one turn keeps only the later record; both land here as a
    // no-op or as a conservative downgrade of the surviving record.
    let downgraded = state
        .file_time
        .mark_read_unobserved(&worktree_path.display().to_string(), &path)
        .await;
    if downgraded {
        tracing::info!(
            path = %path.display(),
            "read coverage downgraded: the turn budget externalized this result \
             after the read handler recorded it"
        );
    }
}

/// Render `lines` as a numbered listing beginning at absolute index `start`,
/// stopping early once the listing would no longer survive the tool-result
/// clamp. Returns the listing and the number of lines actually emitted.
///
/// At least one line is always emitted so a single over-long line still yields
/// a usable (if over-budget) result rather than an empty window.
fn numbered_lines_within_budget<'a, I>(lines: I, start: usize, budget: usize) -> (String, usize)
where
    I: IntoIterator<Item = &'a str>,
{
    let mut numbered = String::new();
    let mut spent = 0usize;
    let mut emitted = 0usize;
    for (i, line) in lines.into_iter().enumerate() {
        let mut l = line.to_string();
        if l.chars().count() > 2000 {
            l = l.chars().take(2000).collect();
        }
        let rendered = format!("{:>6}\t{}\n", start + i + 1, l);
        let cost = json_escaped_len(&rendered);
        if emitted > 0 && spent + cost > budget {
            break;
        }
        spent += cost;
        numbered.push_str(&rendered);
        emitted += 1;
    }
    (numbered, emitted)
}

/// Format an in-memory file body as a numbered, paginated window matching the
/// shape `call_read` returns from the worktree path. Used for mirror-backed
/// cross-repo reads (the whole blob is already in memory from `git show`).
fn numbered_window(content: &str, offset: usize, limit: usize, path: &str) -> serde_json::Value {
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    let start = offset.min(total);
    let end = start.saturating_add(limit).min(total);
    let (numbered, emitted) = numbered_lines_within_budget(
        lines[start..end].iter().copied(),
        start,
        read_content_budget(),
    );
    let emitted_end = start + emitted;
    serde_json::json!({
        "path": path,
        "offset": start,
        "limit": limit,
        "total_lines": total,
        "has_more": emitted_end < total,
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
                let limit = p.limit.unwrap_or(READ_MAX_LINES).min(READ_MAX_LINES);
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
    let limit = p.limit.unwrap_or(READ_MAX_LINES).min(READ_MAX_LINES);

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

    // Emit only as much of the window as will survive the downstream
    // tool-result clamp (`output_stash::MAX_TOOL_RESULT_CHARS`). Anything past
    // that point never reaches the model, so it must not be counted as read.
    let (mut numbered, emitted) = numbered_lines_within_budget(
        all_lines[start..end].iter().map(|(line, _)| line.as_str()),
        start,
        read_content_budget(),
    );
    let emitted_end = start + emitted;
    let clamped_by_result_budget = emitted_end < end;

    if truncated_by_budget {
        numbered.push_str(&format!(
            "\n[file too large: truncated at {} MiB; remaining content not shown]\n",
            MAX_READ_BYTES / (1024 * 1024)
        ));
    }
    if clamped_by_result_budget {
        numbered.push_str(&format!(
            "\n[tool-result budget reached after line {emitted_end}; the rest of this window was \
             NOT shown. Continue with read(file_path, offset={emitted_end}, limit={limit}).]\n"
        ));
    }

    // `has_more` is true if there's content past the scanned window, if the
    // requested window didn't reach the end of what we scanned, or if the
    // tool-result budget cut the window short.
    let has_more = has_more_beyond_window || emitted_end < total_scanned;

    // Compute read coverage metadata from what the model actually receives.
    // A read is full-file coverage only when the worker received all content
    // from the start (offset 0), there are no remaining pages, and no
    // byte-budget truncation occurred. `has_more` now accounts for the
    // tool-result clamp as well, so a listing the clamp would gut can never be
    // recorded as `Full`.
    let coverage = if offset == 0 && !has_more && !truncated_by_budget {
        crate::file_time::ReadCoverage::Full
    } else {
        // Record the byte range of the window actually returned.
        let cov_start = if start < emitted_end {
            line_byte_offsets[start]
        } else {
            // Empty window: point at EOF offset.
            scanned_bytes as u64
        };
        let cov_end = if start < emitted_end {
            Some(all_lines[emitted_end - 1].1)
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
            // Authorship-time size advisory. Runs after the bytes are on disk
            // and cannot affect them; see size_nudge's module docs for why it
            // is here and not in gate_guard.
            let response = maybe_append_size_nudge(
                response,
                worktree_path,
                std::slice::from_ref(&path),
            )
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
                    // Authorship-time size advisory; see `call_write`.
                    let result =
                        maybe_append_size_nudge(result, worktree_path, std::slice::from_ref(&path))
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

                // GateGuard: enforce the worker read-coverage gate before
                // mutation, against the span the patch actually rewrites.
                //
                // A `Delete` destroys the whole file, so whole-file coverage
                // is the honest requirement there. An `Update` only rewrites
                // its located chunks; declaring `0..usize::MAX` for those made
                // the check unsatisfiable for every file too large to read
                // into a single `ReadCoverage::Full` record, which is the
                // `apply_patch` deadlock workers were escaping via `shell`
                // heredocs. Unlocatable chunks fall back to the old
                // conservative span — `apply_patch` rejects them moments later
                // for the same reason.
                let span = match op {
                    crate::patch::FileOp::Update { chunks, .. } => {
                        match tokio::fs::read_to_string(&resolved).await {
                            Ok(content) => crate::patch::update_span(&content, chunks, raw_path)
                                .unwrap_or(0..usize::MAX),
                            Err(_) => 0..usize::MAX,
                        }
                    }
                    _ => 0..usize::MAX,
                };
                gate_guard_edit_check(state, session_role, &worktree_key, &resolved, span).await?;
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
    // Authorship-time size advisory; see `call_write`. A patch can touch many
    // files, so the nudge picks the single worst rather than stacking one
    // advisory per path. Deletes are excluded — nothing was authored.
    let touched_abs: Vec<std::path::PathBuf> = results
        .iter()
        .filter(|(_, action)| *action != "deleted")
        .map(|(file_path, _)| file_path.clone())
        .collect();
    let response = maybe_append_size_nudge(response, worktree_path, &touched_abs).await;
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
