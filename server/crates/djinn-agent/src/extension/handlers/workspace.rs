use super::*;

pub(crate) async fn call_shell(
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    worktree_path: &Path,
) -> Result<serde_json::Value, String> {
    let p: ShellParams = parse_args(arguments)?;
    let timeout_ms = p.timeout_ms.unwrap_or(120_000).max(1000);

    let mut cmd = if cfg!(windows) {
        let mut c = std::process::Command::new("cmd");
        c.arg("/c").arg(&p.command);
        c
    } else {
        let mut c = std::process::Command::new("bash");
        c.arg("-lc").arg(&p.command);
        c
    };

    sandbox::SANDBOX
        .apply(worktree_path, &mut cmd)
        .map_err(|e| e.to_string())?;

    cmd.current_dir(worktree_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::process::isolate_process_group(&mut cmd);
    let output = crate::process::output_with_kill(cmd, Duration::from_millis(timeout_ms))
        .await
        .map_err(|e| format!("failed to run shell command: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    Ok(serde_json::json!({
        "ok": output.status.success(),
        "exit_code": output.status.code(),
        "stdout": stdout,
        "stderr": stderr,
        "workdir": worktree_path,
    }))
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
    let mut all_lines: Vec<String> = Vec::new();
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
        let n = reader
            .read_until(b'\n', &mut buf)
            .await
            .map_err(|e| format!("read failed: {e}"))?;
        if n == 0 {
            break; // EOF — no more lines
        }
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
        all_lines.push(line);

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
    for (i, line) in all_lines[start..end].iter().enumerate() {
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

    state
        .file_time
        .read(&worktree_path.display().to_string(), &path)
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
            let diag_xml = format_diagnostics_xml(state.lsp.diagnostics(worktree_path).await);

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

            let (new_content, match_note) =
                fuzzy_replace(&content, &p.old_text, &p.new_text, &path)?;
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
            let diag_xml = format_diagnostics_xml(state.lsp.diagnostics(worktree_path).await);

            let mut result = serde_json::json!({
                "ok": true,
                "path": path.display().to_string(),
                "diagnostics": diag_xml,
            });
            if let Some(note) = match_note {
                result["match_note"] = serde_json::Value::String(note);
            }
            let result = match (project_id, touched_rel.as_deref()) {
                (Some(pid), Some(rel)) => {
                    enrich_with_related_files(result, state, pid, &[rel.to_string()]).await
                }
                _ => result,
            };
            let touched: Vec<String> = touched_rel.iter().cloned().collect();
            let result =
                maybe_append_pitfall_hint(result, state, worktree_path, project_id, &touched).await;
            Ok(result)
        })
        .await
}

pub(crate) async fn call_apply_patch(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    worktree_path: &Path,
    project_id: Option<&str>,
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

    let diag_xml = format_diagnostics_xml(state.lsp.diagnostics(worktree_path).await);

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

/// F2: best-effort just-in-time pitfall hint. On the FIRST write/edit/
/// apply_patch of a session (gated by `DJINN_JIT_PITFALLS=1`, default OFF),
/// run a scoped pitfall/pattern search over the touched paths and append the
/// top-2 as a transient `jit_pitfalls` field on the response JSON. A miss,
/// an error, or a non-first modification leaves `response` untouched. When the
/// gate is OFF this is a single cheap env read with zero further cost.
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
