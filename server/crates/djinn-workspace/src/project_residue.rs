//! Per-project, no-follow cleanup gate for retired project-local `.djinn` state.
//!
//! The gate is deliberately local: an unsafe checkout produces a report and is
//! left untouched, while callers can continue processing other projects.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResidueKind {
    UnparseableSettings,
    NonEmptySkillsDirectory,
    UnknownEntry,
    UnsafeLegacyEntry,
}

/// Approved tracked inputs were generated leaf files, not arbitrary containers.
/// Validate both their no-follow type and their narrow legacy content before
/// deletion so an operator cannot hide residue below an approved basename.
fn approved_tracked_file(path: &Path, name: &str) -> Result<Option<String>, ProjectResidueError> {
    let Some(entry_metadata) = metadata(path)? else {
        return Ok(None);
    };
    if !entry_metadata.is_file() {
        return Ok(Some(
            "approved tracked input is not a regular file".to_owned(),
        ));
    }
    let bytes = fs::read(path).map_err(|source| ProjectResidueError::Inspect {
        path: path.to_path_buf(),
        source,
    })?;
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        // Invalid legacy bytes are reportable operator residue, not a fatal
        // inspection error that would hide this project's classification.
        Err(_) => return Ok(Some("approved tracked input is not UTF-8".to_owned())),
    };
    let approved = match name {
        "skills.json" => text.trim() == "[]",
        ".gitignore" => text
            .lines()
            .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
            .all(|line| matches!(line.trim(), "worktrees/" | "read-sources/")),
        _ => false,
    };
    Ok((!approved).then_some("approved tracked input has unexpected content".to_owned()))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResidueEntry {
    pub kind: ResidueKind,
    pub path: PathBuf,
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ProjectResidueReport {
    pub project_root: PathBuf,
    pub blocked: Vec<ResidueEntry>,
    pub removed: Vec<PathBuf>,
}

impl ProjectResidueReport {
    pub fn is_clean(&self) -> bool {
        self.blocked.is_empty()
    }
}

#[derive(Debug, Error)]
pub enum ProjectResidueError {
    #[error("unable to inspect project-local residue at {path}: {source}")]
    Inspect {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("unable to remove approved project-local residue at {path}: {source}")]
    Remove {
        path: PathBuf,
        source: std::io::Error,
    },
}

fn metadata(path: &Path) -> Result<Option<fs::Metadata>, ProjectResidueError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ProjectResidueError::Inspect {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Recursively validate an approved disposable directory without following a
/// link. A symlink, special entry, or unreadable child is operator residue,
/// never a cleanup target.
fn safe_disposable_tree(path: &Path) -> Result<Option<String>, ProjectResidueError> {
    let Some(dir_metadata) = metadata(path)? else {
        return Ok(None);
    };
    if dir_metadata.file_type().is_symlink() {
        return Ok(Some("symlink".to_owned()));
    }
    if !dir_metadata.is_dir() {
        return Ok(Some("not a directory".to_owned()));
    }
    let entries = fs::read_dir(path).map_err(|source| ProjectResidueError::Inspect {
        path: path.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| ProjectResidueError::Inspect {
            path: path.to_path_buf(),
            source,
        })?;
        let child = entry.path();
        let Some(child_metadata) = metadata(&child)? else {
            return Ok(Some(format!(
                "entry disappeared during inspection {}",
                child.display()
            )));
        };
        if child_metadata.file_type().is_symlink() {
            return Ok(Some(format!("contains symlink {}", child.display())));
        }
        if child_metadata.is_dir() {
            if let Some(detail) = safe_disposable_tree(&child)? {
                return Ok(Some(detail));
            }
        } else if !child_metadata.is_file() {
            return Ok(Some(format!("contains special entry {}", child.display())));
        }
    }
    Ok(None)
}

/// Inspect one checkout and, only when every entry is approved, remove the
/// disposable worktree/read-source state and the now-empty retired parent.
///
/// `settings.json` is intentionally a blocker here: callers must first run the
/// transactional settings importer. This makes a failed or malformed import
/// observable rather than silently deleting its evidence.
pub fn cleanup_project_local_djinn(
    project_root: &Path,
) -> Result<ProjectResidueReport, ProjectResidueError> {
    let root = project_root.join(".djinn");
    let mut report = ProjectResidueReport {
        project_root: project_root.to_path_buf(),
        ..ProjectResidueReport::default()
    };
    let Some(root_metadata) = metadata(&root)? else {
        return Ok(report);
    };
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        report.blocked.push(ResidueEntry {
            kind: ResidueKind::UnknownEntry,
            path: root,
            detail: "retired parent is not a directory".to_owned(),
        });
        return Ok(report);
    }

    let entries = fs::read_dir(&root).map_err(|source| ProjectResidueError::Inspect {
        path: root.clone(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| ProjectResidueError::Inspect {
            path: root.clone(),
            source,
        })?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(entry_metadata) = metadata(&path)? else {
            continue;
        };
        if entry_metadata.file_type().is_symlink() {
            report.blocked.push(ResidueEntry {
                kind: ResidueKind::UnknownEntry,
                path,
                detail: "symlink entries are never followed".to_owned(),
            });
            continue;
        }
        match name.as_ref() {
            "settings.json" => report.blocked.push(ResidueEntry {
                kind: ResidueKind::UnparseableSettings,
                path,
                detail: "settings import did not remove this source".to_owned(),
            }),
            "skills" if entry_metadata.is_dir() => {
                if fs::read_dir(&path)
                    .map_err(|source| ProjectResidueError::Inspect {
                        path: path.clone(),
                        source,
                    })?
                    .next()
                    .is_some()
                {
                    report.blocked.push(ResidueEntry {
                        kind: ResidueKind::NonEmptySkillsDirectory,
                        path,
                        detail: "project-local skills must be removed by an operator".to_owned(),
                    });
                }
            }
            "worktrees" | "read-sources" => {
                if let Some(detail) = safe_disposable_tree(&path)? {
                    report.blocked.push(ResidueEntry {
                        kind: ResidueKind::UnsafeLegacyEntry,
                        path,
                        detail,
                    });
                }
            }
            ".gitignore" | "skills.json" => {
                if let Some(detail) = approved_tracked_file(&path, &name)? {
                    report.blocked.push(ResidueEntry {
                        kind: ResidueKind::UnknownEntry,
                        path,
                        detail,
                    });
                }
            }
            _ => report.blocked.push(ResidueEntry {
                kind: ResidueKind::UnknownEntry,
                path,
                detail: "unexpected project-local .djinn entry".to_owned(),
            }),
        }
    }
    if !report.is_clean() {
        return Ok(report);
    }

    // All removals happen only after the complete no-follow inventory passed.
    for name in [
        "worktrees",
        "read-sources",
        "skills",
        ".gitignore",
        "skills.json",
    ] {
        let path = root.join(name);
        let Some(entry_metadata) = metadata(&path)? else {
            continue;
        };
        let removed = if entry_metadata.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        removed.map_err(|source| ProjectResidueError::Remove {
            path: path.clone(),
            source,
        })?;
        report.removed.push(path);
    }
    if fs::read_dir(&root)
        .map_err(|source| ProjectResidueError::Inspect {
            path: root.clone(),
            source,
        })?
        .next()
        .is_none()
    {
        fs::remove_dir(&root).map_err(|source| ProjectResidueError::Remove {
            path: root.clone(),
            source,
        })?;
        report.removed.push(root);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_project_removes_disposable_state_without_migration_records() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".djinn");
        fs::create_dir_all(root.join("worktrees/index")).unwrap();
        fs::create_dir_all(root.join("read-sources/cache")).unwrap();
        fs::write(root.join(".gitignore"), "worktrees/\nread-sources/\n").unwrap();
        let report = cleanup_project_local_djinn(temp.path()).unwrap();
        assert!(report.is_clean());
        assert!(!root.exists());
        assert_eq!(report.removed.len(), 4);
    }

    #[test]
    fn blocked_project_preserves_unknown_and_nonempty_skill_residue() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".djinn");
        fs::create_dir_all(root.join("skills")).unwrap();
        fs::write(root.join("skills/custom.md"), "operator content").unwrap();
        fs::write(root.join("operator.txt"), "do not delete").unwrap();
        let report = cleanup_project_local_djinn(temp.path()).unwrap();
        assert!(!report.is_clean());
        assert!(
            report
                .blocked
                .iter()
                .any(|entry| entry.kind == ResidueKind::NonEmptySkillsDirectory)
        );
        assert!(
            report
                .blocked
                .iter()
                .any(|entry| entry.kind == ResidueKind::UnknownEntry)
        );
        assert!(root.join("operator.txt").exists());
        assert!(root.join("skills/custom.md").exists());
    }

    #[test]
    fn retained_settings_json_blocks_cleanup_until_the_importer_removes_it() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".djinn");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("settings.json"), "{\"malformed\": true}").unwrap();
        let report = cleanup_project_local_djinn(temp.path()).unwrap();
        assert!(!report.is_clean());
        assert!(
            report
                .blocked
                .iter()
                .any(|entry| entry.kind == ResidueKind::UnparseableSettings
                    && entry.path == root.join("settings.json"))
        );
        assert!(root.join("settings.json").exists());
    }

    #[test]
    fn approved_name_directory_is_reported_and_never_recursively_deleted() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".djinn");
        fs::create_dir_all(root.join("skills.json")).unwrap();
        fs::write(root.join("skills.json/operator-secret"), "do not delete").unwrap();
        let report = cleanup_project_local_djinn(temp.path()).unwrap();
        assert!(!report.is_clean());
        assert!(
            report
                .blocked
                .iter()
                .any(|entry| entry.path == root.join("skills.json"))
        );
        assert!(root.join("skills.json/operator-secret").exists());
    }

    #[test]
    fn invalid_utf8_in_approved_tracked_file_is_reported_not_a_fatal_inspection() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".djinn");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("skills.json"), b"[\xff\xfe]").unwrap();
        let report = cleanup_project_local_djinn(temp.path()).unwrap();
        assert!(!report.is_clean());
        assert!(
            report
                .blocked
                .iter()
                .any(|entry| entry.path == root.join("skills.json")
                    && entry.detail.contains("UTF-8"))
        );
        assert!(root.join("skills.json").exists());
    }

    #[test]
    fn symlink_child_in_disposable_tree_blocks_cleanup() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".djinn");
        fs::create_dir_all(root.join("worktrees")).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", root.join("worktrees/escape")).unwrap();
        }
        let report = cleanup_project_local_djinn(temp.path()).unwrap();
        assert!(!report.is_clean());
        assert!(
            report
                .blocked
                .iter()
                .any(|entry| entry.kind == ResidueKind::UnsafeLegacyEntry
                    && entry.path == root.join("worktrees"))
        );
        assert!(root.join("worktrees").exists());
    }

    #[test]
    fn symlink_root_entry_is_classified_as_unknown_and_never_followed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".djinn");
        fs::create_dir_all(&root).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", root.join("settings.json")).unwrap();
        }
        let report = cleanup_project_local_djinn(temp.path()).unwrap();
        assert!(!report.is_clean());
        assert!(report.blocked.iter().any(
            |entry| entry.kind == ResidueKind::UnknownEntry && entry.detail.contains("symlink")
        ));
        assert!(fs::symlink_metadata(root.join("settings.json")).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn special_file_in_disposable_tree_blocks_cleanup_and_remains_untouched() {
        use std::os::unix::fs::FileTypeExt;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".djinn");
        let fifo = root.join("worktrees/operator.pipe");
        fs::create_dir_all(fifo.parent().unwrap()).unwrap();
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo available on unix");
        assert!(status.success(), "mkfifo should succeed");

        let report = cleanup_project_local_djinn(temp.path()).unwrap();

        assert!(!report.is_clean());
        assert!(report.blocked.iter().any(|entry| {
            entry.kind == ResidueKind::UnsafeLegacyEntry
                && entry.path == root.join("worktrees")
                && entry.detail.contains("special entry")
        }));
        assert!(fs::symlink_metadata(&fifo).unwrap().file_type().is_fifo());
    }

    #[test]
    fn blocked_checkout_does_not_stop_an_independent_clean_checkout() {
        let blocked = tempfile::tempdir().unwrap();
        let blocked_root = blocked.path().join(".djinn");
        fs::create_dir_all(blocked_root.join("skills")).unwrap();
        fs::write(blocked_root.join("skills/custom.md"), "operator content").unwrap();

        let clean = tempfile::tempdir().unwrap();
        let clean_root = clean.path().join(".djinn");
        fs::create_dir_all(clean_root.join("worktrees/index")).unwrap();
        fs::write(clean_root.join(".gitignore"), "worktrees/\n").unwrap();

        let blocked_report = cleanup_project_local_djinn(blocked.path()).unwrap();
        let clean_report = cleanup_project_local_djinn(clean.path()).unwrap();

        assert!(!blocked_report.is_clean());
        assert!(blocked_root.join("skills/custom.md").exists());
        assert!(clean_report.is_clean());
        assert!(!clean_root.exists());
    }
}
