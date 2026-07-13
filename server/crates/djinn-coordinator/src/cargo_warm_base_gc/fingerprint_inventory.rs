//! Report-only, side-effect-free inventory of Cargo fingerprint units inside a
//! warm base.
//!
//! The inventory is additive and never calls removal APIs or interprets a
//! newest fingerprint mtime as a last-use signal.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub const FINGERPRINT_DIR: &str = ".fingerprint";

/// Report-only entry for a single Cargo fingerprint unit directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FingerprintUnitEntry {
    pub profile_root: PathBuf,
    pub unit_path: PathBuf,
    pub unit_name: String,
    /// Newest file mtime observed inside the unit directory.
    ///
    /// This is **not** a last-use timestamp: Cargo may reuse an artifact
    /// without refreshing files beneath `.fingerprint/<unit>`. The field name
    /// makes that conservative interpretation explicit.
    pub compiled_at_upper_bound: SystemTime,
    /// Measured bytes inside the `.fingerprint/<unit>` directory.
    pub projected_bytes: u64,
}

/// Report-only, side-effect-free inventory of Cargo fingerprint units inside a
/// warm base.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FingerprintUnitInventory {
    pub units: Vec<FingerprintUnitEntry>,
}

fn is_profile_root_name(name: &str) -> bool {
    matches!(name, "debug" | "release" | "test" | "doc")
}

/// Return whether `path` exists and is a directory, treating `NotFound` as a
/// normal absence and any other metadata error as a fail-closed error.
fn is_directory_fallible(path: &Path) -> Result<bool, String> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!(
            "failed to read metadata for {}: {e}",
            path.display()
        )),
    }
}

/// Directory size measurement that fails closed rather than silently skipping
/// unreadable entries.
fn directory_size_fail_closed(root: &Path) -> Result<u64, String> {
    let mut total: u64 = 0;
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let children = std::fs::read_dir(&path)
            .map_err(|e| format!("failed to read directory {}: {e}", path.display()))?;
        for child in children {
            let child = child.map_err(|e| {
                format!(
                    "failed to read directory entry under {}: {e}",
                    path.display()
                )
            })?;
            let kind = child.file_type().map_err(|e| {
                format!(
                    "failed to read file type for {}: {e}",
                    child.path().display()
                )
            })?;
            if kind.is_dir() {
                pending.push(child.path());
            } else if kind.is_file() {
                let metadata = child.metadata().map_err(|e| {
                    format!(
                        "failed to read metadata for {}: {e}",
                        child.path().display()
                    )
                })?;
                total = total.saturating_add(metadata.len());
            }
        }
    }
    Ok(total)
}

/// Newest file mtime inside `root`, failing closed on traversal errors.
fn newest_file_mtime(root: &Path) -> Result<SystemTime, String> {
    let mut newest: Option<SystemTime> = None;
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let children = std::fs::read_dir(&path)
            .map_err(|e| format!("failed to read directory {}: {e}", path.display()))?;
        for child in children {
            let child = child.map_err(|e| {
                format!(
                    "failed to read directory entry under {}: {e}",
                    path.display()
                )
            })?;
            let kind = child.file_type().map_err(|e| {
                format!(
                    "failed to read file type for {}: {e}",
                    child.path().display()
                )
            })?;
            if kind.is_dir() {
                pending.push(child.path());
            } else if kind.is_file() {
                let metadata = child.metadata().map_err(|e| {
                    format!(
                        "failed to read metadata for {}: {e}",
                        child.path().display()
                    )
                })?;
                let mtime = metadata.modified().map_err(|e| {
                    format!("failed to read mtime for {}: {e}", child.path().display())
                })?;
                newest = Some(match newest {
                    Some(prev) => prev.max(mtime),
                    None => mtime,
                });
            }
        }
    }
    newest.ok_or_else(|| format!("no files found in {}", root.display()))
}

/// Report-only, side-effect-free inventory of Cargo fingerprint units inside a
/// warm base.
///
/// Discovers profile roots containing `.fingerprint` across the top-level
/// `debug`, `release`, `test`, and `doc` layouts and target-triple nested
/// layouts. For each `.fingerprint/<unit>` directory the result reports the
/// profile root, unit path/name, newest file mtime (`compiled_at_upper_bound`),
/// and measured bytes that could be projected.
///
/// This function never calls removal APIs, never labels a unit as stale, and
/// treats any traversal or metadata error as a fail-closed error.
pub fn inventory_fingerprint_units(base: &Path) -> Result<FingerprintUnitInventory, String> {
    let base_canonical = std::fs::canonicalize(base)
        .map_err(|e| format!("failed to canonicalize base {}: {e}", base.display()))?;
    if !base_canonical.is_dir() {
        return Err(format!(
            "warm base {} is not a directory",
            base_canonical.display()
        ));
    }

    let mut profile_roots: Vec<PathBuf> = Vec::new();

    // Top-level profile roots.
    for name in ["debug", "release", "test", "doc"] {
        let root = base_canonical.join(name);
        if is_directory_fallible(&root)? {
            profile_roots.push(root);
        }
    }

    // Target-triple nested profile roots: any non-profile top-level directory
    // that contains profile directories.
    let top_children = std::fs::read_dir(&base_canonical)
        .map_err(|e| format!("failed to read base {}: {e}", base_canonical.display()))?;
    for child in top_children {
        let child = child.map_err(|e| {
            format!(
                "failed to read directory entry under {}: {e}",
                base_canonical.display()
            )
        })?;
        let name = child.file_name();
        let name_str = name.to_string_lossy();
        if is_profile_root_name(&name_str) {
            continue;
        }
        let kind = child.file_type().map_err(|e| {
            format!(
                "failed to read file type for {}: {e}",
                child.path().display()
            )
        })?;
        if !kind.is_dir() {
            continue;
        }
        for profile in ["debug", "release", "test", "doc"] {
            let nested = child.path().join(profile);
            if is_directory_fallible(&nested)? {
                profile_roots.push(nested);
            }
        }
    }

    profile_roots.sort();
    profile_roots.dedup();

    let mut units = Vec::new();
    for profile_root in profile_roots {
        let profile_canonical = std::fs::canonicalize(&profile_root).map_err(|e| {
            format!(
                "failed to canonicalize profile root {}: {e}",
                profile_root.display()
            )
        })?;
        if !profile_canonical.starts_with(&base_canonical) {
            return Err(format!(
                "profile root {} escapes warm base {}",
                profile_canonical.display(),
                base_canonical.display()
            ));
        }

        let fingerprint_dir = profile_canonical.join(FINGERPRINT_DIR);
        let fingerprint_metadata = match std::fs::metadata(&fingerprint_dir) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(format!(
                    "failed to read metadata for .fingerprint {}: {e}",
                    fingerprint_dir.display()
                ));
            }
        };
        if !fingerprint_metadata.is_dir() {
            return Err(format!(
                ".fingerprint at {} is not a directory",
                fingerprint_dir.display()
            ));
        }
        let fingerprint_canonical = std::fs::canonicalize(&fingerprint_dir).map_err(|e| {
            format!(
                "failed to canonicalize .fingerprint {}: {e}",
                fingerprint_dir.display()
            )
        })?;
        if !fingerprint_canonical.starts_with(&base_canonical) {
            return Err(format!(
                ".fingerprint {} escapes warm base {}",
                fingerprint_canonical.display(),
                base_canonical.display()
            ));
        }

        let unit_entries = std::fs::read_dir(&fingerprint_canonical).map_err(|e| {
            format!(
                "failed to read .fingerprint directory {}: {e}",
                fingerprint_canonical.display()
            )
        })?;
        for unit_entry in unit_entries {
            let unit_entry = unit_entry.map_err(|e| {
                format!(
                    "failed to read .fingerprint entry under {}: {e}",
                    fingerprint_canonical.display()
                )
            })?;
            let kind = unit_entry.file_type().map_err(|e| {
                format!(
                    "failed to read file type for {}: {e}",
                    unit_entry.path().display()
                )
            })?;
            if !kind.is_dir() {
                continue;
            }

            let unit_path = unit_entry.path();
            let unit_name = unit_path
                .file_name()
                .ok_or_else(|| format!("missing unit name for {}", unit_path.display()))?
                .to_str()
                .ok_or_else(|| format!("non-UTF8 unit name in {}", unit_path.display()))?
                .to_owned();

            let unit_canonical = std::fs::canonicalize(&unit_path)
                .map_err(|e| format!("failed to canonicalize unit {}: {e}", unit_path.display()))?;
            if !unit_canonical.starts_with(&base_canonical) {
                return Err(format!(
                    "unit {} escapes warm base {}",
                    unit_canonical.display(),
                    base_canonical.display()
                ));
            }

            let compiled_at_upper_bound = newest_file_mtime(&unit_path)?;
            let projected_bytes = directory_size_fail_closed(&unit_path)?;
            units.push(FingerprintUnitEntry {
                profile_root: profile_root.clone(),
                unit_path: unit_path.clone(),
                unit_name,
                compiled_at_upper_bound,
                projected_bytes,
            });
        }
    }

    units.sort_by(|left, right| {
        left.profile_root
            .cmp(&right.profile_root)
            .then_with(|| left.unit_name.cmp(&right.unit_name))
    });

    Ok(FingerprintUnitInventory { units })
}
