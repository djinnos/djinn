//! Retention garbage collection for durable output-stash pointers and blobs.

use std::collections::HashSet;
use std::path::Path;

use djinn_core::models::SessionStatus;

use super::{DurablePointerKind, DurablePointerRecord, blob_path, parse_durable_pointer};

/// Session state needed by durable output-stash GC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputStashGcSession {
    pub status: SessionStatus,
    /// Terminal timestamp as Unix seconds. `None` is treated conservatively.
    pub ended_at_unix_secs: Option<u64>,
}

/// Best-effort GC statistics suitable for maintenance logs/metrics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutputStashGcReport {
    pub pointers_scanned: usize,
    pub pointers_deleted: usize,
    pub pointers_retained: usize,
    pub blobs_scanned: usize,
    pub blobs_deleted: usize,
    pub blobs_retained: usize,
    pub errors: Vec<String>,
}

impl OutputStashGcReport {
    pub fn is_success(&self) -> bool {
        self.errors.is_empty()
    }
}

fn is_terminal_session_status(status: SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::Completed | SessionStatus::Interrupted | SessionStatus::Failed
    )
}

fn file_modified_unix_secs(path: &Path) -> Result<u64, String> {
    let modified = std::fs::metadata(path)
        .map_err(|e| format!("metadata {}: {e}", path.display()))?
        .modified()
        .map_err(|e| format!("modified time {}: {e}", path.display()))?;
    Ok(modified
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0))
}

/// Garbage-collect durable output-stash id-pointers and content-addressed blobs.
///
/// `retention_cutoff_unix_secs` is the precomputed cutoff timestamp: terminal
/// session pointers are removed when their `ended_at` is at or before this
/// value, and unreferenced blobs are removed when their file mtime is at or
/// before it. The supplied lookup keeps this engine reusable and easy to unit
/// test; coordinator wiring can adapt it to the session repository later.
pub fn gc_durable_output_stash<F>(
    root: &Path,
    retention_cutoff_unix_secs: u64,
    mut lookup_session: F,
) -> OutputStashGcReport
where
    F: FnMut(&str) -> Result<Option<OutputStashGcSession>, String>,
{
    let mut report = OutputStashGcReport::default();
    let ids_dir = root.join("ids");
    let blobs_dir = root.join("blobs");

    let mut retained_hashes: HashSet<String> = HashSet::new();

    scan_pointers(
        root,
        &ids_dir,
        &mut report,
        &mut retained_hashes,
        &mut lookup_session,
        retention_cutoff_unix_secs,
    );
    scan_blobs(
        &blobs_dir,
        &mut report,
        &retained_hashes,
        retention_cutoff_unix_secs,
    );

    report
}

fn scan_pointers<F>(
    root: &Path,
    ids_dir: &Path,
    report: &mut OutputStashGcReport,
    retained_hashes: &mut HashSet<String>,
    lookup_session: &mut F,
    retention_cutoff_unix_secs: u64,
) where
    F: FnMut(&str) -> Result<Option<OutputStashGcSession>, String>,
{
    match std::fs::read_dir(ids_dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(e) => {
                        report.errors.push(format!("read pointer entry: {e}"));
                        continue;
                    }
                };
                process_pointer_entry(
                    root,
                    entry,
                    report,
                    retained_hashes,
                    lookup_session,
                    retention_cutoff_unix_secs,
                );
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => report
            .errors
            .push(format!("read ids dir {}: {e}", ids_dir.display())),
    }
}

fn process_pointer_entry<F>(
    root: &Path,
    entry: std::fs::DirEntry,
    report: &mut OutputStashGcReport,
    retained_hashes: &mut HashSet<String>,
    lookup_session: &mut F,
    retention_cutoff_unix_secs: u64,
) where
    F: FnMut(&str) -> Result<Option<OutputStashGcSession>, String>,
{
    let pointer_path = entry.path();
    if !pointer_path.is_file() {
        return;
    }
    report.pointers_scanned += 1;

    let raw = match std::fs::read_to_string(&pointer_path) {
        Ok(raw) => raw,
        Err(e) => {
            report
                .errors
                .push(format!("read pointer {}: {e}", pointer_path.display()));
            report.pointers_retained += 1;
            return;
        }
    };
    let record = match parse_durable_pointer(&raw) {
        Ok(record) => record,
        Err(e) => {
            report
                .errors
                .push(format!("parse pointer {}: {e}", pointer_path.display()));
            report.pointers_retained += 1;
            return;
        }
    };

    if !blob_path(root, &record.content_hash).is_file() {
        match std::fs::remove_file(&pointer_path) {
            Ok(()) => report.pointers_deleted += 1,
            Err(e) => {
                report.errors.push(format!(
                    "delete missing-blob pointer {}: {e}",
                    pointer_path.display()
                ));
                report.pointers_retained += 1;
            }
        }
        return;
    }

    let should_delete =
        should_delete_pointer(&record, retention_cutoff_unix_secs, lookup_session, report);

    if should_delete {
        match std::fs::remove_file(&pointer_path) {
            Ok(()) => report.pointers_deleted += 1,
            Err(e) => {
                report.errors.push(format!(
                    "delete expired pointer {}: {e}",
                    pointer_path.display()
                ));
                report.pointers_retained += 1;
                retained_hashes.insert(record.content_hash);
            }
        }
    } else {
        report.pointers_retained += 1;
        retained_hashes.insert(record.content_hash);
    }
}

fn should_delete_pointer<F>(
    record: &DurablePointerRecord,
    retention_cutoff_unix_secs: u64,
    lookup_session: &mut F,
    report: &mut OutputStashGcReport,
) -> bool
where
    F: FnMut(&str) -> Result<Option<OutputStashGcSession>, String>,
{
    if record.kind == DurablePointerKind::Legacy {
        return false;
    }
    let Some(session_id) = record.session_id.as_deref() else {
        return false;
    };
    match lookup_session(session_id) {
        Ok(Some(session)) if is_terminal_session_status(session.status) => session
            .ended_at_unix_secs
            .is_some_and(|ended_at| ended_at <= retention_cutoff_unix_secs),
        Ok(Some(_live_or_paused)) => false,
        Ok(None) => false,
        Err(e) => {
            report
                .errors
                .push(format!("lookup session {session_id}: {e}"));
            false
        }
    }
}

fn scan_blobs(
    blobs_dir: &Path,
    report: &mut OutputStashGcReport,
    retained_hashes: &HashSet<String>,
    retention_cutoff_unix_secs: u64,
) {
    match std::fs::read_dir(blobs_dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(e) => {
                        report.errors.push(format!("read blob entry: {e}"));
                        continue;
                    }
                };
                process_blob_entry(entry, report, retained_hashes, retention_cutoff_unix_secs);
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => report
            .errors
            .push(format!("read blobs dir {}: {e}", blobs_dir.display())),
    }
}

fn process_blob_entry(
    entry: std::fs::DirEntry,
    report: &mut OutputStashGcReport,
    retained_hashes: &HashSet<String>,
    retention_cutoff_unix_secs: u64,
) {
    let blob = entry.path();
    if !blob.is_file() {
        return;
    }
    report.blobs_scanned += 1;
    let Some(name) = blob.file_name().and_then(|n| n.to_str()) else {
        report.blobs_retained += 1;
        return;
    };
    if retained_hashes.contains(name) {
        report.blobs_retained += 1;
        return;
    }
    match file_modified_unix_secs(&blob) {
        Ok(modified_at) if modified_at <= retention_cutoff_unix_secs => {
            match std::fs::remove_file(&blob) {
                Ok(()) => report.blobs_deleted += 1,
                Err(e) => {
                    report
                        .errors
                        .push(format!("delete blob {}: {e}", blob.display()));
                    report.blobs_retained += 1;
                }
            }
        }
        Ok(_) => report.blobs_retained += 1,
        Err(e) => {
            report.errors.push(e);
            report.blobs_retained += 1;
        }
    }
}
