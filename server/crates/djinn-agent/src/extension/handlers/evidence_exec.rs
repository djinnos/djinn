//! Agent-local execution seam for frozen command evidence checks.
//!
//! This module is intentionally not a profile schema.  It only accepts the
//! small request shape below after an authenticated task session reaches the
//! local extension fallback.

use std::path::{Path, PathBuf};
use std::time::Duration;

use djinn_control_plane::tools::evidence_command::{
    ServerCommandObservation, record_command_observation,
};
use djinn_control_plane::tools::evidence_plan::{EvidencePlanIdentity, require_frozen_plan};
use djinn_db::EvidenceRepository;
use djinn_sandbox::{
    EVIDENCE_MAX_OUTPUT_BYTES, EVIDENCE_MAX_TIMEOUT, EvidenceRequest, EvidenceSandbox,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const MIN_TIMEOUT_MS: u64 = 1;
const MAX_EXCERPT_BYTES: usize = 16 * 1024;

/// Closed, caller-owned portion of an evidence execution request.  Execution
/// identity, executable selection, stdin, health, and provenance are all
/// deliberately absent and cannot be deserialized.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EvidenceExecRequest {
    check_id: String,
    argv: Vec<String>,
    cwd: Option<String>,
    timeout_ms: Option<u64>,
}

pub(super) async fn call_evidence_exec(
    state: &crate::context::AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    clone_root: &Path,
    session_task_id: Option<&str>,
    authenticated_session_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let request: EvidenceExecRequest = serde_json::from_value(serde_json::Value::Object(
        arguments
            .clone()
            .ok_or("evidence_exec requires an argument object")?,
    ))
    .map_err(|error| format!("invalid evidence_exec request: {error}"))?;
    let task_id = session_task_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or("evidence_exec requires an authenticated task session")?;
    let session_id = authenticated_session_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or("evidence_exec requires an authenticated active session id")?;
    if request.check_id.trim().is_empty() || request.argv.is_empty() {
        return Err("invalid evidence_exec request: check_id and argv must be nonempty".into());
    }
    let timeout_ms = request
        .timeout_ms
        .unwrap_or(EVIDENCE_MAX_TIMEOUT.as_millis() as u64);
    if !(MIN_TIMEOUT_MS..=EVIDENCE_MAX_TIMEOUT.as_millis() as u64).contains(&timeout_ms) {
        return Err("invalid evidence_exec request: timeout_ms is outside the server bound".into());
    }

    let clone_root = clone_root
        .canonicalize()
        .map_err(|error| format!("evidence_exec cannot canonicalize clone root: {error}"))?;
    let cwd = canonical_cwd(&clone_root, request.cwd.as_deref())?;
    let identity = evidence_identity(task_id, session_id, &clone_root).await?;
    let repository = EvidenceRepository::new(state.db.clone());

    // This preflight is before the sandbox boundary.  Rejected plan/check/method
    // requests therefore cannot start a descendant or append an invocation.
    let frozen = require_frozen_plan(&repository, &identity)
        .await
        .map_err(|error| error.to_string())?;
    let check_id = request.check_id.trim();
    match frozen
        .plan
        .checks
        .iter()
        .find(|check| check.check_id == check_id)
    {
        Some(check) if check.method == "command" => {}
        Some(check) => {
            return Err(format!(
                "evidence check '{check_id}' requires method '{}', not command",
                check.method
            ));
        }
        None => return Err(format!("unknown evidence check '{check_id}'")),
    }

    let timeout = Duration::from_millis(timeout_ms);
    let launch_started = tokio::time::Instant::now();
    let execution = EvidenceSandbox::new(clone_root.clone())
        .run(EvidenceRequest {
            argv: request.argv.clone(),
            cwd: Some(cwd.clone()),
            timeout,
            output_limit: EVIDENCE_MAX_OUTPUT_BYTES,
        })
        .await;
    let elapsed_millis = i64::try_from(launch_started.elapsed().as_millis()).unwrap_or(i64::MAX);
    let observation =
        observation_from_execution(request.argv, cwd, timeout_ms, elapsed_millis, execution);
    let invocation = record_command_observation(&repository, &identity, check_id, observation)
        .await
        .map_err(|error| error.to_string())?;

    Ok(serde_json::json!({
        "invocation_id": invocation.id,
        "execution": {
            "check_id": invocation.check_id,
            "argv": invocation.argv,
            "cwd": invocation.canonical_cwd,
            "launch_state": invocation.launch_state,
            "process_state": invocation.process_state,
            "exit_code": invocation.exit_code,
            "signal": invocation.signal,
            "runner_failure": invocation.runner_failure,
            "elapsed_millis": invocation.elapsed_millis,
            "timeout_millis": invocation.timeout_millis,
            "timed_out": invocation.timed_out,
            "stdout_digest": invocation.stdout_digest,
            "stdout_excerpt": invocation.stdout_excerpt,
            "stdout_truncated": invocation.stdout_truncated,
            "stderr_digest": invocation.stderr_digest,
            "stderr_excerpt": invocation.stderr_excerpt,
            "stderr_truncated": invocation.stderr_truncated,
        }
    }))
}

fn canonical_cwd(clone_root: &Path, requested: Option<&str>) -> Result<PathBuf, String> {
    let path = match requested {
        None => clone_root.to_path_buf(),
        Some(value) => {
            let relative = Path::new(value);
            if relative.is_absolute() {
                return Err(
                    "invalid evidence_exec request: cwd must be relative to the clone".into(),
                );
            }
            clone_root.join(relative)
        }
    };
    let canonical = path
        .canonicalize()
        .map_err(|_| "invalid evidence_exec request: cwd is not an in-clone directory")?;
    if !canonical.is_dir() || !canonical.starts_with(clone_root) {
        return Err("invalid evidence_exec request: cwd escapes the clone".into());
    }
    Ok(canonical)
}

async fn evidence_identity(
    task_id: &str,
    session_id: &str,
    clone_root: &Path,
) -> Result<EvidencePlanIdentity, String> {
    // All repository access goes through djinn-git. The agent does not own
    // raw git capability, even when deriving server-owned evidence identity.
    let commit = djinn_git::head_commit_sha(clone_root)
        .await
        .map_err(|error| format!("evidence_exec cannot resolve checked-out commit: {error}"))?;
    let mut fingerprint = Sha256::new();
    fingerprint.update(clone_root.as_os_str().as_encoded_bytes());
    fingerprint.update(b"\0");
    fingerprint.update(commit.as_bytes());
    Ok(EvidencePlanIdentity {
        spike_task_id: task_id.to_owned(),
        session_id: session_id.to_owned(),
        captured_commit_sha: commit,
        worktree_fingerprint: hex::encode(fingerprint.finalize()),
    })
}

fn observation_from_execution(
    argv: Vec<String>,
    cwd: PathBuf,
    timeout_ms: u64,
    elapsed_millis: i64,
    execution: Result<djinn_sandbox::ChatShellResult, djinn_sandbox::EvidenceError>,
) -> ServerCommandObservation {
    match execution {
        Ok(result) => {
            let timed_out = result.timed_out;
            let process_state = if timed_out {
                "timed_out"
            } else if result.exit_code.is_some() {
                "exited"
            } else {
                "signaled"
            };
            ServerCommandObservation {
                argv,
                canonical_cwd: cwd.display().to_string(),
                launch_state: "launched".into(),
                process_state: process_state.into(),
                launched_at: None,
                finished_at: None,
                exit_code: result.exit_code,
                signal: None,
                runner_failure: None,
                elapsed_millis: Some(elapsed_millis),
                timeout_millis: Some(i64::try_from(timeout_ms).unwrap_or(i64::MAX)),
                timed_out,
                stdout_digest: Some(digest(&result.stdout)),
                stdout_excerpt: Some(excerpt(&result.stdout)),
                stdout_truncated: result.truncated,
                stderr_digest: Some(digest(&result.stderr)),
                stderr_excerpt: Some(excerpt(&result.stderr)),
                stderr_truncated: result.truncated,
            }
        }
        Err(error) => ServerCommandObservation {
            argv,
            canonical_cwd: cwd.display().to_string(),
            launch_state: "failed_to_launch".into(),
            process_state: "runner_failed".into(),
            launched_at: None,
            finished_at: None,
            exit_code: None,
            signal: None,
            runner_failure: Some(error.to_string()),
            elapsed_millis: Some(elapsed_millis),
            timeout_millis: Some(i64::try_from(timeout_ms).unwrap_or(i64::MAX)),
            timed_out: false,
            stdout_digest: Some(digest(&[])),
            stdout_excerpt: Some(String::new()),
            stdout_truncated: false,
            stderr_digest: Some(digest(&[])),
            stderr_excerpt: Some(String::new()),
            stderr_truncated: false,
        },
    }
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn excerpt(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_EXCERPT_BYTES)]).into_owned()
}
