//! Internal-only bounded GitHub Actions artifact operations.
//!
//! This module deliberately has no MCP parameter type or dispatch entry.  The
//! public tool is wired by a later task; keeping this boundary lets that task
//! reuse the repository-bound transport and result shapes without exposing a
//! partially implemented capability.

use std::io::{Cursor, Read};
use std::time::Duration;

use djinn_provider::github_api::{ActionsArtifact, GitHubApiClient};
use serde::Serialize;

use super::ci::ResolvedWorkflowRun;

const OP_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ENTRIES: usize = 256;
const MAX_ENTRY_BYTES: u64 = 2 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Serialize)]
pub(crate) struct ArtifactListReport {
    pub run_id: u64,
    pub lane: &'static str,
    pub artifacts: Vec<ArtifactSummary>,
    pub truncated: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct ArtifactSummary {
    pub name: String,
    pub size_bytes: u64,
    pub expired: bool,
    pub expires_at: Option<String>,
    pub artifact_id: u64,
}

/// List exactly one provider page, retaining the provider's order.  The
/// timeout covers the whole operation rather than only the HTTP request.
pub(crate) async fn list_artifacts(
    client: &GitHubApiClient,
    owner: &str,
    repo: &str,
    resolved: ResolvedWorkflowRun,
) -> Result<ArtifactListReport, String> {
    tokio::time::timeout(OP_TIMEOUT, async {
        let page = client
            .list_run_artifacts(owner, repo, resolved.run_id)
            .await
            .map_err(|error| {
                format!(
                    "failed to list artifacts for run {}: {error}",
                    resolved.run_id
                )
            })?;
        Ok(ArtifactListReport {
            run_id: resolved.run_id,
            lane: resolved.lane.label(),
            artifacts: page.artifacts.into_iter().map(summary).collect(),
            truncated: page.truncated,
        })
    })
    .await
    .map_err(|_| "ci_artifact list exceeded its 30-second deadline".to_string())?
}

/// Fetch one exact artifact from the bounded first page.  No report is built
/// until download and rendering both succeed, so a timeout cannot leak a
/// partial fetch response.
pub(crate) async fn fetch_artifact(
    client: &GitHubApiClient,
    owner: &str,
    repo: &str,
    resolved: ResolvedWorkflowRun,
    name: &str,
) -> Result<String, String> {
    tokio::time::timeout(OP_TIMEOUT, async {
        let page = client
            .list_run_artifacts(owner, repo, resolved.run_id)
            .await
            .map_err(|error| format!("failed to list artifacts for run {}: {error}", resolved.run_id))?;
        let artifact = page.artifacts.iter().find(|artifact| artifact.name == name).ok_or_else(|| {
            let suffix = if page.truncated {
                " The first artifact page was truncated, so a matching artifact may exist on a later page."
            } else {
                ""
            };
            format!("artifact `{name}` was not found in run {}.{suffix}", resolved.run_id)
        })?;
        if artifact.expired {
            return Err(format!(
                "artifact `{name}` in run {} has expired and can no longer be downloaded",
                resolved.run_id
            ));
        }
        let download = client
            .download_artifact(owner, repo, resolved.run_id, artifact.id)
            .await
            .map_err(|error| format!("failed to download artifact `{name}` from run {}: {error}", resolved.run_id))?;
        render_zip(&download.bytes)
    })
    .await
    .map_err(|_| "ci_artifact fetch exceeded its 30-second deadline; no artifact report was returned".to_string())?
}

fn summary(artifact: ActionsArtifact) -> ArtifactSummary {
    ArtifactSummary {
        name: artifact.name,
        size_bytes: artifact.size_in_bytes,
        expired: artifact.expired,
        expires_at: artifact.expires_at,
        artifact_id: artifact.id,
    }
}

fn render_zip(bytes: &[u8]) -> Result<String, String> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|error| format!("artifact is not a readable ZIP archive: {error}"))?;
    if archive.len() > MAX_ENTRIES {
        return Err(format!("artifact ZIP has more than {MAX_ENTRIES} entries"));
    }
    let mut total = 0_u64;
    let mut report = String::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|error| error.to_string())?;
        let path = entry.name().to_owned();
        if path.contains('\0') || path.starts_with('/') || path.contains("..") || path.contains(':')
        {
            return Err(format!("artifact ZIP contains unsafe path `{path}`"));
        }
        if entry.size() > MAX_ENTRY_BYTES {
            return Err(format!("artifact entry `{path}` exceeds the 2 MiB limit"));
        }
        total = total.saturating_add(entry.size());
        if total > MAX_TOTAL_BYTES {
            return Err("artifact ZIP exceeds the 16 MiB decompressed limit".to_string());
        }
        if entry.is_dir() {
            continue;
        }
        let mut body = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut body)
            .map_err(|error| error.to_string())?;
        if body.contains(&0) || std::str::from_utf8(&body).is_err() {
            report.push_str(&format!(
                "## {path}\n[body omitted: binary or invalid UTF-8]\n\n"
            ));
        } else {
            report.push_str(&format!(
                "## {path}\n{}\n\n",
                String::from_utf8_lossy(&body)
            ));
        }
    }
    Ok(report)
}
