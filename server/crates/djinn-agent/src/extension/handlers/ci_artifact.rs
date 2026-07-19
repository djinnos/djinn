//! Internal-only bounded GitHub Actions artifact operations.
use super::ci::{WorkflowRunResolutionRequest, resolve_workflow_run};
use djinn_provider::github_api::{ActionsArtifact, GitHubApiClient};
use serde::Serialize;
use std::io::{Cursor, Read};
use std::time::Duration;
const OP_TIMEOUT: Duration = Duration::from_secs(30);
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
/// Resolution and listing share a single cancellable operation deadline.
pub(crate) async fn list_artifacts(
    client: &GitHubApiClient,
    owner: &str,
    repo: &str,
    request: WorkflowRunResolutionRequest,
) -> Result<ArtifactListReport, String> {
    tokio::time::timeout(OP_TIMEOUT, async {
        let resolved = resolve_workflow_run(client, owner, repo, request).await?;
        let page = client
            .list_run_artifacts(owner, repo, resolved.run_id)
            .await
            .map_err(|e| format!("failed to list artifacts for run {}: {e}", resolved.run_id))?;
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
/// Resolution, list, download, and in-memory rendering share one deadline.
pub(crate) async fn fetch_artifact(
    client: &GitHubApiClient,
    owner: &str,
    repo: &str,
    request: WorkflowRunResolutionRequest,
    name: &str,
) -> Result<String, String> {
    tokio::time::timeout(OP_TIMEOUT, async {
        let resolved = resolve_workflow_run(client, owner, repo, request).await?;
        let page = client.list_run_artifacts(owner, repo, resolved.run_id).await.map_err(|e| format!("failed to list artifacts for run {}: {e}", resolved.run_id))?;
        let artifact = page.artifacts.iter().find(|a| a.name == name).ok_or_else(|| format!("artifact `{name}` was not found in run {}.{}", resolved.run_id, if page.truncated { " The first artifact page was truncated, so a matching artifact may exist on a later page." } else { "" }))?;
        if artifact.expired { return Err(format!("artifact `{name}` in run {} has expired and can no longer be downloaded", resolved.run_id)); }
        let download = client.download_artifact(owner, repo, resolved.run_id, artifact.id).await.map_err(|e| format!("failed to download artifact `{name}` from run {}: {e}", resolved.run_id))?;
        render_ci_artifact_zip(&download.bytes)
    }).await.map_err(|_| "ci_artifact fetch exceeded its 30-second deadline; no artifact report was returned".to_string())?
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
const MAX_ENTRIES: usize = 256;
const MAX_ENTRY_BYTES: usize = 2 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;
const BINARY_INSPECTION_BYTES: usize = 8 * 1024;
const MAX_LINES_PER_FILE: usize = 1_000;
const MAX_RENDERED_FILE_BYTES: usize = 64 * 1024;
const MAX_REPORT_BYTES: usize = 2 * 1024 * 1024;

/// Render a ZIP artifact wholly in memory, enforcing limits while reading.
pub(crate) fn render_ci_artifact_zip(bytes: &[u8]) -> Result<String, String> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|error| format!("invalid ZIP artifact: {error}"))?;
    if archive.len() > MAX_ENTRIES {
        return Err(format!("ZIP artifact has more than {MAX_ENTRIES} entries"));
    }
    let mut total = 0usize;
    let mut entries = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        let mut file = archive
            .by_index(index)
            .map_err(|e| format!("failed to read ZIP entry {index}: {e}"))?;
        let path = normalize_path(file.name())?;
        if file.is_symlink() {
            return Err(format!("ZIP artifact contains symlink entry: {path}"));
        }
        let mut body = Vec::new();
        let mut chunk = [0_u8; 8192];
        loop {
            let read = file
                .read(&mut chunk)
                .map_err(|e| format!("failed to decompress ZIP entry {path}: {e}"))?;
            if read == 0 {
                break;
            }
            if body.len().saturating_add(read) > MAX_ENTRY_BYTES {
                return Err(format!("ZIP entry exceeds {MAX_ENTRY_BYTES} bytes: {path}"));
            }
            total = total.saturating_add(read);
            if total > MAX_TOTAL_BYTES {
                return Err(format!(
                    "ZIP artifact exceeds {MAX_TOTAL_BYTES} decompressed bytes"
                ));
            }
            body.extend_from_slice(&chunk[..read]);
        }
        // `ZipFile::is_dir` is inferred from the member name, so consume a
        // directory-like member before rendering it as metadata. Otherwise a
        // member named `directory/` could bypass decompression limits.
        if file.is_dir() {
            entries.push(RenderEntry::Skipped(path, "directory"));
            continue;
        }
        let inspected = &body[..body.len().min(BINARY_INSPECTION_BYTES)];
        entries.push(if body.contains(&0) {
            RenderEntry::Skipped(path, "NUL byte content")
        } else if is_binary(inspected) {
            RenderEntry::Skipped(path, "binary content")
        } else if std::str::from_utf8(&body).is_err() {
            RenderEntry::Skipped(path, "invalid UTF-8 content")
        } else {
            RenderEntry::Text(path, String::from_utf8(body).expect("validated UTF-8"))
        });
    }
    Ok(render_entries(entries))
}

enum RenderEntry {
    Text(String, String),
    Skipped(String, &'static str),
}

fn normalize_path(name: &str) -> Result<String, String> {
    if name.is_empty() || name.contains('\0') || name.starts_with('/') || name.starts_with('\\') {
        return Err(format!("ZIP artifact contains unsafe path: {name:?}"));
    }
    let mut parts = Vec::new();
    for part in name.split(['/', '\\']) {
        if part.is_empty() || part == "." {
            continue;
        }
        let raw = part.as_bytes();
        if part == ".." || (raw.len() >= 2 && raw[0].is_ascii_alphabetic() && raw[1] == b':') {
            return Err(format!("ZIP artifact contains unsafe path: {name:?}"));
        }
        parts.push(part);
    }
    if parts.is_empty() {
        return Err(format!("ZIP artifact contains unsafe path: {name:?}"));
    }
    Ok(parts.join("/"))
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .any(|b| matches!(*b, 1..=8 | 11..=12 | 14..=31))
}

fn render_entries(entries: Vec<RenderEntry>) -> String {
    let rendered: Vec<(String, String)> = entries
        .into_iter()
        .map(|entry| match entry {
            RenderEntry::Text(path, text) => (path, render_file_text(&text)),
            RenderEntry::Skipped(path, reason) => (path, format!("[skipped: {reason}]\n")),
        })
        .collect();
    let paths: Vec<&str> = rendered.iter().map(|(path, _)| path.as_str()).collect();
    let mut prefix_bytes = Vec::with_capacity(rendered.len() + 1);
    prefix_bytes.push(0);
    for (path, body) in &rendered {
        prefix_bytes.push(
            prefix_bytes.last().copied().unwrap_or_default()
                + format!("== {path} ==\n").len()
                + body.len(),
        );
    }

    // Select the longest archive-order prefix whose bodies and complete
    // omission footer fit together. This reserves disclosure space rather than
    // allowing accepted bodies to consume the entire report budget.
    let selected = (0..=rendered.len())
        .rev()
        .find(|&count| {
            let footer_bytes = if count == rendered.len() {
                0
            } else {
                omission_footer(&paths[count..]).len()
            };
            prefix_bytes[count].saturating_add(footer_bytes) <= MAX_REPORT_BYTES
        })
        .unwrap_or(0);

    let mut report = String::new();
    for (path, body) in rendered.iter().take(selected) {
        report.push_str(&format!("== {path} ==\n"));
        report.push_str(body);
    }
    if selected < paths.len() {
        push_bounded(&mut report, &omission_footer(&paths[selected..]));
    }
    report
}

fn omission_footer(paths: &[impl AsRef<str>]) -> String {
    if paths.is_empty() {
        return String::new();
    }
    let mut footer = String::from("\n[report limit reached; omitted paths:]\n");
    for path in paths {
        footer.push_str("- ");
        footer.push_str(path.as_ref());
        footer.push('\n');
    }
    footer
}

fn render_file_text(text: &str) -> String {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let omitted_lines = lines.len().saturating_sub(MAX_LINES_PER_FILE);
    let content = lines[lines.len().saturating_sub(MAX_LINES_PER_FILE)..].concat();
    let mut output = if omitted_lines == 0 {
        String::new()
    } else {
        format!("[omitted {omitted_lines} leading lines]\n")
    };
    if output.len() + content.len() <= MAX_RENDERED_FILE_BYTES {
        output.push_str(&content);
        return output;
    }
    let marker = "[omitted leading bytes]\n";
    let budget = MAX_RENDERED_FILE_BYTES.saturating_sub(output.len() + marker.len());
    let mut start = content.len().saturating_sub(budget);
    while start < content.len() && !content.is_char_boundary(start) {
        start += 1;
    }
    output.push_str(marker);
    output.push_str(&content[start..]);
    output
}

fn push_bounded(target: &mut String, value: &str) {
    let mut end = value
        .len()
        .min(MAX_REPORT_BYTES.saturating_sub(target.len()));
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    target.push_str(&value[..end]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn zip(entries: Vec<(&str, Vec<u8>)>) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (n, b) in entries {
            w.start_file(n, SimpleFileOptions::default()).unwrap();
            w.write_all(&b).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    #[test]
    fn order_and_binary_metadata() {
        let report = render_ci_artifact_zip(&zip(vec![
            ("a", b"one\n".to_vec()),
            ("b", vec![0, b'x']),
            ("c", b"three\n".to_vec()),
        ]))
        .unwrap();
        assert!(report.find("== a").unwrap() < report.find("== b").unwrap());
        assert!(report.contains("[skipped: NUL byte content]"));
        assert!(!report.contains("== b ==\nx"));
    }
    #[test]
    fn unsafe_paths_fail_whole_archive_before_a_partial_report() {
        for name in [
            "../bad",
            "/bad",
            "\\bad",
            "C:/bad",
            "safe/../../bad",
            "bad\0name",
        ] {
            let error = render_ci_artifact_zip(&zip(vec![
                ("first", b"first body".to_vec()),
                (name, b"x".to_vec()),
            ]))
            .unwrap_err();
            assert!(error.contains("unsafe path"));
            assert!(!error.contains("first body"));
        }
    }

    #[test]
    fn symlink_fails_whole_archive_before_a_partial_report() {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file("first", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"first body").unwrap();
        writer
            .add_symlink("link", "target", SimpleFileOptions::default())
            .unwrap();
        let bytes = writer.finish().unwrap().into_inner();
        let error = render_ci_artifact_zip(&bytes).unwrap_err();
        assert!(error.contains("symlink"));
        assert!(!error.contains("first body"));
    }

    #[test]
    fn entry_count_entry_byte_and_aggregate_boundaries() {
        let names = (0..=MAX_ENTRIES).map(|n| n.to_string()).collect::<Vec<_>>();
        assert!(
            render_ci_artifact_zip(&zip(names
                .iter()
                .map(|n| (n.as_str(), Vec::new()))
                .collect()))
            .unwrap_err()
            .contains("entries")
        );
        assert!(
            render_ci_artifact_zip(&zip(names[..MAX_ENTRIES]
                .iter()
                .map(|n| (n.as_str(), Vec::new()))
                .collect()))
            .is_ok()
        );
        assert!(render_ci_artifact_zip(&zip(vec![("exact", vec![b'x'; MAX_ENTRY_BYTES])])).is_ok());
        assert!(
            render_ci_artifact_zip(&zip(vec![("over", vec![b'x'; MAX_ENTRY_BYTES + 1])]))
                .unwrap_err()
                .contains("entry exceeds")
        );
        let aggregate_names = (0..(MAX_TOTAL_BYTES / MAX_ENTRY_BYTES))
            .map(|n| n.to_string())
            .collect::<Vec<_>>();
        assert!(
            render_ci_artifact_zip(&zip(aggregate_names
                .iter()
                .map(|n| (n.as_str(), vec![b'x'; MAX_ENTRY_BYTES]))
                .collect()))
            .is_ok()
        );
        let mut over = aggregate_names
            .iter()
            .map(|n| (n.as_str(), vec![b'x'; MAX_ENTRY_BYTES]))
            .collect::<Vec<_>>();
        over.push(("over", vec![b'x']));
        assert!(
            render_ci_artifact_zip(&zip(over))
                .unwrap_err()
                .contains("decompressed")
        );
    }

    #[test]
    fn high_compression_ratio_is_still_decompression_bounded() {
        let bytes = zip(vec![("compressible", vec![b'a'; MAX_ENTRY_BYTES + 1])]);
        assert!(bytes.len() < 16 * 1024);
        assert!(
            render_ci_artifact_zip(&bytes)
                .unwrap_err()
                .contains("entry exceeds")
        );
    }

    #[test]
    fn skipped_binary_and_invalid_utf8_are_metadata_only_and_count() {
        let report = render_ci_artifact_zip(&zip(vec![
            ("nul", vec![0, b's', b'e', b'c', b'r', b'e', b't']),
            ("invalid", vec![0xff; MAX_ENTRY_BYTES]),
        ]))
        .unwrap();
        assert!(report.contains("== nul ==\n[skipped: NUL byte content]"));
        assert!(report.contains("== invalid ==\n[skipped: invalid UTF-8 content]"));
        assert!(!report.contains("secret"));
        let names = (0..(MAX_TOTAL_BYTES / MAX_ENTRY_BYTES))
            .map(|index| format!("skipped-{index}"))
            .collect::<Vec<_>>();
        let mut skipped = names
            .iter()
            .map(|name| (name.as_str(), vec![0; MAX_ENTRY_BYTES]))
            .collect::<Vec<_>>();
        skipped.push(("over", vec![0]));
        assert!(
            render_ci_artifact_zip(&zip(skipped))
                .unwrap_err()
                .contains("decompressed")
        );
    }

    #[test]
    fn binary_detection_only_inspects_first_eight_kib() {
        let mut after = vec![b'a'; BINARY_INSPECTION_BYTES];
        after.push(1);
        assert!(
            !render_ci_artifact_zip(&zip(vec![("after", after)]))
                .unwrap()
                .contains("binary content")
        );
        let mut within = vec![b'a'; BINARY_INSPECTION_BYTES - 1];
        within.push(1);
        assert!(
            render_ci_artifact_zip(&zip(vec![("within", within)]))
                .unwrap()
                .contains("[skipped: binary content]")
        );
    }

    #[test]
    fn keeps_last_thousand_lines_at_exact_and_plus_one() {
        let exact = (0..MAX_LINES_PER_FILE)
            .map(|n| format!("{n}\n"))
            .collect::<String>();
        assert!(
            !render_ci_artifact_zip(&zip(vec![("exact", exact.into_bytes())]))
                .unwrap()
                .contains("leading lines]")
        );
        let text = (0..=MAX_LINES_PER_FILE)
            .map(|n| format!("{n}\n"))
            .collect::<String>();
        let report = render_ci_artifact_zip(&zip(vec![("log", text.into_bytes())])).unwrap();
        assert!(report.contains("[omitted 1 leading lines]"));
        assert!(report.contains("1000\n"));
        assert!(!report.contains("== log ==\n0\n"));
    }

    #[test]
    fn file_byte_limit_is_exact_and_utf8_safe() {
        let exact = "é".repeat(MAX_RENDERED_FILE_BYTES / 2);
        assert!(
            !render_ci_artifact_zip(&zip(vec![("exact", exact.into_bytes())]))
                .unwrap()
                .contains("omitted leading bytes")
        );
        let over = "é".repeat(MAX_RENDERED_FILE_BYTES / 2 + 1);
        let report = render_ci_artifact_zip(&zip(vec![("over", over.into_bytes())])).unwrap();
        assert!(report.contains("[omitted leading bytes]"));
        assert!(report.ends_with('é'));
    }

    #[test]
    fn report_limit_is_exact_and_lists_omitted_paths() {
        let names = (0..32).map(|n| format!("f{n}")).collect::<Vec<_>>();
        let headings = names
            .iter()
            .map(|name| format!("== {name} ==\n").len())
            .sum::<usize>();
        let per_file = (MAX_REPORT_BYTES - headings) / names.len();
        let remainder = (MAX_REPORT_BYTES - headings) % names.len();
        assert!(per_file <= MAX_RENDERED_FILE_BYTES);
        let entries = names
            .iter()
            .enumerate()
            .map(|(i, n)| {
                (
                    n.as_str(),
                    vec![b'x'; per_file + usize::from(i < remainder)],
                )
            })
            .collect();
        assert_eq!(
            render_ci_artifact_zip(&zip(entries)).unwrap().len(),
            MAX_REPORT_BYTES
        );
        let mut over_names = names;
        over_names.push("later".to_owned());
        let entries = over_names
            .iter()
            .enumerate()
            .map(|(i, n)| {
                (
                    n.as_str(),
                    vec![
                        b'x';
                        if i < 32 {
                            per_file + usize::from(i < remainder)
                        } else {
                            1
                        }
                    ],
                )
            })
            .collect();
        let report = render_ci_artifact_zip(&zip(entries)).unwrap();
        assert!(report.len() <= MAX_REPORT_BYTES);
        assert!(report.contains("[report limit reached; omitted paths:]"));
        assert!(report.contains("- later\n"));
    }

    #[test]
    fn directory_payloads_are_counted_at_entry_and_aggregate_boundaries() {
        let exact = render_ci_artifact_zip(&zip(vec![("directory/", vec![b'x'; MAX_ENTRY_BYTES])]))
            .unwrap();
        assert!(exact.contains("== directory ==\n[skipped: directory]"));
        assert!(
            render_ci_artifact_zip(&zip(vec![("directory/", vec![b'x'; MAX_ENTRY_BYTES + 1],)]))
                .unwrap_err()
                .contains("entry exceeds")
        );

        let names = (0..(MAX_TOTAL_BYTES / MAX_ENTRY_BYTES))
            .map(|index| format!("directory-{index}/"))
            .collect::<Vec<_>>();
        assert!(
            render_ci_artifact_zip(&zip(names
                .iter()
                .map(|name| (name.as_str(), vec![b'x'; MAX_ENTRY_BYTES]))
                .collect(),))
            .is_ok()
        );
        let mut over = names
            .iter()
            .map(|name| (name.as_str(), vec![b'x'; MAX_ENTRY_BYTES]))
            .collect::<Vec<_>>();
        over.push(("one-more/", vec![b'x']));
        assert!(
            render_ci_artifact_zip(&zip(over))
                .unwrap_err()
                .contains("decompressed")
        );
    }
}
