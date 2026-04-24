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

pub(crate) async fn call_read(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    worktree_path: &Path,
) -> Result<serde_json::Value, String> {
    let p: ReadParams = parse_args(arguments)?;
    let path = resolve_path(&p.file_path, worktree_path);

    let bytes = tokio::fs::read(&path).await.map_err(|e| {
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

    if bytes.contains(&0) {
        return Err(format!("refusing to read binary file: {}", path.display()));
    }

    let text = String::from_utf8(bytes)
        .map_err(|_| format!("refusing to read binary file: {}", path.display()))?;
    let all_lines: Vec<String> = text
        .lines()
        .map(|line| {
            if line.chars().count() > 2000 {
                line.chars().take(2000).collect::<String>()
            } else {
                line.to_string()
            }
        })
        .collect();

    let offset = p.offset.unwrap_or(0);
    let limit = p.limit.unwrap_or(2000).min(2000);
    let start = offset.min(all_lines.len());
    let end = start.saturating_add(limit).min(all_lines.len());

    let mut numbered = String::new();
    for (i, line) in all_lines[start..end].iter().enumerate() {
        let line_no = start + i + 1;
        numbered.push_str(&format!("{:>6}\t{}\n", line_no, line));
    }

    state
        .file_time
        .read(&worktree_path.display().to_string(), &path)
        .await?;

    Ok(serde_json::json!({
        "path": path.display().to_string(),
        "offset": start,
        "limit": limit,
        "total_lines": all_lines.len(),
        "has_more": end < all_lines.len(),
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

            state
                .file_time
                .read(&worktree_path.display().to_string(), &path)
                .await?;

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
            Ok(response)
        })
        .await
}

pub(super) async fn call_edit(
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

            state
                .file_time
                .read(&worktree_path.display().to_string(), &path)
                .await?;

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
            Ok(result)
        })
        .await
}

pub(super) async fn call_apply_patch(
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
            state.file_time.read(&worktree_key, file_path).await?;
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
    Ok(response)
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

    let matcher =
        load_project_exclusion_matcher(&state.db, &state.event_bus, project_id).await;
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
