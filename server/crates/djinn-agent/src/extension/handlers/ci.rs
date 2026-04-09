use super::*;

/// Fetch a GitHub Actions job log, optionally filtered to a specific step.
///
/// The raw log is cleaned (timestamps stripped, group markers removed) and
/// returned as-is. When the result exceeds the tool-result size limit, the
/// reply-loop automatically stashes the full output and the worker can
/// paginate with `output_view` / `output_grep`.
pub(super) async fn call_ci_job_log(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    session_task_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let p: CiJobLogParams = parse_args(arguments)?;

    let task_id = session_task_id.ok_or("ci_job_log requires a task context (session_task_id)")?;

    // Find the CI failure activity entry that contains the owner/repo context.
    // The PR poller stores this alongside the body when logging CI failures.
    let task_repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());

    let (owner, repo) = {
        let entries = task_repo
            .list_activity(task_id)
            .await
            .map_err(|e| format!("failed to list activity: {e}"))?;

        let mut found = None;
        for entry in entries.iter().rev() {
            if entry.event_type != "comment" || entry.actor_role != "verification" {
                continue;
            }
            if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&entry.payload)
                && let Some(ci_jobs) = payload.get("ci_jobs").and_then(|v| v.as_array())
            {
                let has_job = ci_jobs
                    .iter()
                    .any(|j| j.get("job_id").and_then(|v| v.as_u64()) == Some(p.job_id));
                if has_job {
                    let o = payload
                        .get("owner")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let r = payload
                        .get("repo")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if !o.is_empty() && !r.is_empty() {
                        found = Some((o, r));
                        break;
                    }
                }
            }
        }
        found.ok_or_else(|| {
            format!(
                "Could not find CI job metadata for job_id={} in task {} activity.  \
                 This tool can only fetch logs for jobs recorded in the task activity log.",
                p.job_id, task_id
            )
        })?
    };

    let cred_repo = Arc::new(CredentialRepository::new(
        state.db.clone(),
        state.event_bus.clone(),
    ));
    let gh_client = GitHubApiClient::new(cred_repo);

    let raw_log = gh_client
        .get_job_logs(&owner, &repo, p.job_id)
        .await
        .map_err(|e| format!("failed to fetch job log: {e}"))?;

    let cleaned = clean_actions_log(&raw_log);

    // Optionally filter to just the requested step.
    let output = if let Some(ref step_name) = p.step {
        extract_step_log(&cleaned, step_name).unwrap_or_else(|| {
            format!(
                "Step '{}' not found in the job log. Returning full cleaned log.\n\n{}",
                step_name, cleaned
            )
        })
    } else {
        cleaned
    };

    Ok(serde_json::Value::String(output))
}

/// Strip GitHub Actions noise from a raw job log.
///
/// Removes ISO-8601 timestamp prefixes and `##[group]`/`##[endgroup]`
/// markers while preserving `##[error]` and `##[warning]` content.
fn clean_actions_log(raw_log: &str) -> String {
    raw_log
        .lines()
        .map(|line| {
            // Strip leading ISO-8601 timestamp prefix (29 chars like "2026-03-24T17:10:50.0448487Z ")
            line.get(..29)
                .filter(|prefix| {
                    prefix.len() >= 20
                        && prefix.as_bytes().first() == Some(&b'2')
                        && prefix.contains('T')
                        && prefix.ends_with(' ')
                })
                .map(|_| &line[29..])
                .unwrap_or(line)
        })
        .filter(|line| !line.starts_with("##[endgroup]"))
        .map(|line| {
            line.strip_prefix("##[group]")
                .or_else(|| line.strip_prefix("##[error]"))
                .or_else(|| line.strip_prefix("##[warning]"))
                .unwrap_or(line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Extract the log section for a specific step name.
///
/// GitHub Actions logs use `##[group]Run <step>` / `##[endgroup]` to delimit
/// steps. After cleaning (which strips `##[group]` prefixes), the step
/// boundaries become plain text lines starting with `Run ...` or the step
/// name itself. We look for the step name in these boundary lines and return
/// everything between the start and the next boundary (or end of log).
fn extract_step_log(cleaned_log: &str, step_name: &str) -> Option<String> {
    let lines: Vec<&str> = cleaned_log.lines().collect();
    let step_lower = step_name.to_lowercase();

    // Find the start of the target step section.
    // After cleaning, step headers look like:
    //   "Run cd server && cargo test ..." or just the step name
    // We search for lines that contain the step name (case-insensitive).
    let mut start_idx = None;
    let mut end_idx = lines.len();

    // Track step boundaries — lines that look like GitHub Actions step headers.
    // These typically start with "Run " after group marker removal, or match
    // known step patterns. We use a heuristic: if a line exactly matches one
    // of the step names from the job, it's a boundary.
    //
    // Simpler approach: scan for the step name, then collect until the next
    // recognizable boundary or end of log.
    for (i, line) in lines.iter().enumerate() {
        if line.to_lowercase().contains(&step_lower) && start_idx.is_none() {
            start_idx = Some(i);
        }
    }

    let start = start_idx?;

    // Look for the next step boundary after our start.
    // Step boundaries in cleaned logs are hard to detect generically.
    // Use a practical heuristic: "Post Run " lines mark cleanup steps,
    // and "Complete job" marks the end.
    for (i, line) in lines.iter().enumerate().skip(start + 1) {
        let trimmed = line.trim();
        if trimmed.starts_with("Post Run ") || trimmed == "Complete job" {
            end_idx = i;
            break;
        }
    }

    let section: Vec<&str> = lines[start..end_idx].to_vec();
    if section.is_empty() {
        None
    } else {
        Some(section.join("\n"))
    }
}
