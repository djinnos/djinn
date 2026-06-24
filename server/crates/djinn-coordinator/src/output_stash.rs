//! Compatibility shim: output stash GC utilities.
//!
//! The subset of `djinn-agent::output_stash` used by coordinator health sweeps.

use std::collections::HashSet;
use std::path::Path;

use djinn_core::models::SessionStatus;

pub fn durable_root_for_gc() -> Option<std::path::PathBuf> {
    std::env::var("DJINN_DURABLE_OUTPUT_ROOT")
        .ok()
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_dir())
}

#[derive(Debug, Clone)]
pub struct OutputStashGcSession {
    pub status: SessionStatus,
    pub ended_at_unix_secs: Option<u64>,
}

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

/// Run GC over durable output stash directories.
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

    match std::fs::read_dir(&ids_dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(e) => {
                        report.errors.push(format!("read pointer entry: {e}"));
                        continue;
                    }
                };
                let pointer_path = entry.path();
                if !pointer_path.is_file() {
                    continue;
                }
                report.pointers_scanned += 1;
                let session_id = pointer_path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                match lookup_session(session_id) {
                    Ok(Some(session)) => {
                        if is_terminal_session_status(session.status)
                            && session.ended_at_unix_secs.unwrap_or(0) <= retention_cutoff_unix_secs
                        {
                            if let Err(e) = std::fs::remove_file(&pointer_path) {
                                report.errors.push(format!("{session_id}: {e}"));
                            } else {
                                report.pointers_deleted += 1;
                            }
                        } else {
                            if let Ok(hash) = std::fs::read_to_string(&pointer_path) {
                                retained_hashes.insert(hash.trim().to_string());
                            }
                            report.pointers_retained += 1;
                        }
                    }
                    Ok(None) => {
                        report.pointers_retained += 1;
                    }
                    Err(e) => {
                        report.errors.push(format!("{session_id}: {e}"));
                        report.pointers_retained += 1;
                    }
                }
            }
        }
        Err(e) => {
            report.errors.push(format!("read ids dir: {e}"));
        }
    }

    match std::fs::read_dir(&blobs_dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(e) => {
                        report.errors.push(format!("read blob entry: {e}"));
                        continue;
                    }
                };
                let blob_path = entry.path();
                if !blob_path.is_file() {
                    continue;
                }
                report.blobs_scanned += 1;
                let hash = blob_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                if retained_hashes.contains(hash) {
                    report.blobs_retained += 1;
                } else {
                    if let Err(e) = std::fs::remove_file(&blob_path) {
                        report.errors.push(format!("blob {hash}: {e}"));
                    } else {
                        report.blobs_deleted += 1;
                    }
                }
            }
        }
        Err(e) => {
            report.errors.push(format!("read blobs dir: {e}"));
        }
    }

    report
}
