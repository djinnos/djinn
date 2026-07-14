//! Immutable, side-effect-free inventory planner for warm-base pressure GC.
//!
//! The planner deliberately knows nothing about locks, capacity probes, or
//! deletion.  Those are execution concerns.  Its only filesystem reads use
//! `symlink_metadata`; a link, unreadable directory, or escape rejects that
//! base rather than risking a partially trusted plan.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const PROFILES: [&str; 4] = ["debug", "release", "test", "doc"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PressureRung {
    Incremental,
    StaleProfile,
    WholeBase,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PressurePlanDisposition {
    Eligible,
    Retained(PressurePlanRetainReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressurePlanRetainReason {
    UnsafeBase,
    UnsafeProfile,
    MetadataError,
    TraversalError,
    ProfileNotIdle,
}

/// All non-filesystem facts used by an immutable plan.  Callers capture this
/// once from their activity snapshot before invoking the synchronous planner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PressureBaseSnapshot {
    pub project_id: String,
    /// Canonical base path captured by the caller's inventory snapshot.
    pub canonical_base: PathBuf,
    /// DB-derived effective latest project activity. `None` is conservatively
    /// treated as not profile-idle, but still has deterministic base ordering.
    pub effective_latest_activity: Option<SystemTime>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PressurePlanUnit {
    pub rung: PressureRung,
    pub project_id: String,
    pub canonical_base: PathBuf,
    pub canonical_target: PathBuf,
    pub projected_allocated_bytes: u64,
    pub disposition: PressurePlanDisposition,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ThreeRungPressurePlan {
    /// Global rung order is invariant: incremental, stale profile, whole base.
    pub units: Vec<PressurePlanUnit>,
}

/// Build a deterministic three-rung pressure plan from a single captured
/// activity/base snapshot. This function performs no mutation, capacity probe,
/// lock operation, or post-plan guard call.
pub fn build_three_rung_pressure_plan(
    mut bases: Vec<PressureBaseSnapshot>,
    now: SystemTime,
    profile_min_idle: Duration,
) -> ThreeRungPressurePlan {
    bases.sort_by(|left, right| {
        left.effective_latest_activity
            .cmp(&right.effective_latest_activity)
            .then_with(|| left.canonical_base.cmp(&right.canonical_base))
    });

    let mut incremental = Vec::new();
    let mut profiles = Vec::new();
    let mut whole_bases = Vec::new();
    for base in bases {
        match scan_base(&base, now, profile_min_idle) {
            Ok((mut first, mut second, whole)) => {
                incremental.append(&mut first);
                profiles.append(&mut second);
                whole_bases.push(whole);
            }
            Err(reason) => whole_bases.push(PressurePlanUnit {
                rung: PressureRung::WholeBase,
                project_id: base.project_id,
                canonical_base: base.canonical_base.clone(),
                canonical_target: base.canonical_base,
                projected_allocated_bytes: 0,
                disposition: PressurePlanDisposition::Retained(reason),
            }),
        }
    }
    // Bases are sorted above; profile roots are explicitly lexical per base.
    // Keep first-rung ordering base-first/profile-lexical rather than sorting
    // across identities, which makes snapshot ties stable and reproducible.
    incremental.extend(profiles);
    incremental.extend(whole_bases);
    ThreeRungPressurePlan { units: incremental }
}

fn scan_base(
    base: &PressureBaseSnapshot,
    now: SystemTime,
    profile_min_idle: Duration,
) -> Result<
    (
        Vec<PressurePlanUnit>,
        Vec<PressurePlanUnit>,
        PressurePlanUnit,
    ),
    PressurePlanRetainReason,
> {
    let base_meta = std::fs::symlink_metadata(&base.canonical_base)
        .map_err(|_| PressurePlanRetainReason::MetadataError)?;
    if base_meta.file_type().is_symlink() || !base_meta.is_dir() {
        return Err(PressurePlanRetainReason::UnsafeBase);
    }
    let actual_base = std::fs::canonicalize(&base.canonical_base)
        .map_err(|_| PressurePlanRetainReason::MetadataError)?;
    if actual_base != base.canonical_base {
        return Err(PressurePlanRetainReason::UnsafeBase);
    }

    let roots = profile_roots(&actual_base)?;
    let mut incremental = Vec::new();
    let mut profiles = Vec::new();
    for root in roots {
        let bytes = allocated_tree_bytes(&root)?;
        let profile_unit =
            |rung, target, projected_allocated_bytes, disposition| PressurePlanUnit {
                rung,
                project_id: base.project_id.clone(),
                canonical_base: actual_base.clone(),
                canonical_target: target,
                projected_allocated_bytes,
                disposition,
            };
        let incremental_path = root.join("incremental");
        match safe_directory(&incremental_path, &actual_base)? {
            Some(path) => incremental.push(profile_unit(
                PressureRung::Incremental,
                path.clone(),
                allocated_tree_bytes(&path)?,
                PressurePlanDisposition::Eligible,
            )),
            None => {}
        }
        let disposition = match base.effective_latest_activity {
            Some(activity)
                if now.duration_since(activity).unwrap_or_default() >= profile_min_idle =>
            {
                PressurePlanDisposition::Eligible
            }
            _ => PressurePlanDisposition::Retained(PressurePlanRetainReason::ProfileNotIdle),
        };
        profiles.push(profile_unit(
            PressureRung::StaleProfile,
            root,
            bytes,
            disposition,
        ));
    }
    let whole_bytes = allocated_tree_bytes(&actual_base)?;
    let whole = PressurePlanUnit {
        rung: PressureRung::WholeBase,
        project_id: base.project_id.clone(),
        canonical_base: actual_base.clone(),
        canonical_target: actual_base,
        projected_allocated_bytes: whole_bytes,
        disposition: PressurePlanDisposition::Eligible,
    };
    Ok((incremental, profiles, whole))
}

fn profile_roots(base: &Path) -> Result<Vec<PathBuf>, PressurePlanRetainReason> {
    let mut roots = Vec::new();
    for profile in PROFILES {
        if let Some(root) = safe_directory(&base.join(profile), base)? {
            roots.push(root);
        }
    }
    let entries = std::fs::read_dir(base).map_err(|_| PressurePlanRetainReason::TraversalError)?;
    for entry in entries {
        let entry = entry.map_err(|_| PressurePlanRetainReason::TraversalError)?;
        let name = entry.file_name();
        if PROFILES.iter().any(|profile| name == *profile) {
            continue;
        }
        let metadata = std::fs::symlink_metadata(entry.path())
            .map_err(|_| PressurePlanRetainReason::MetadataError)?;
        if metadata.file_type().is_symlink() {
            return Err(PressurePlanRetainReason::UnsafeProfile);
        }
        if !metadata.is_dir() {
            continue;
        }
        let triple = std::fs::canonicalize(entry.path())
            .map_err(|_| PressurePlanRetainReason::MetadataError)?;
        if !triple.starts_with(base) {
            return Err(PressurePlanRetainReason::UnsafeProfile);
        }
        for profile in PROFILES {
            if let Some(root) = safe_directory(&triple.join(profile), base)? {
                roots.push(root);
            }
        }
    }
    roots.sort();
    roots.dedup();
    Ok(roots)
}

fn safe_directory(path: &Path, base: &Path) -> Result<Option<PathBuf>, PressurePlanRetainReason> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(PressurePlanRetainReason::MetadataError),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(PressurePlanRetainReason::UnsafeProfile)
        }
        Ok(_) => {
            let canonical =
                std::fs::canonicalize(path).map_err(|_| PressurePlanRetainReason::MetadataError)?;
            if canonical.starts_with(base) {
                Ok(Some(canonical))
            } else {
                Err(PressurePlanRetainReason::UnsafeProfile)
            }
        }
    }
}

fn allocated_tree_bytes(root: &Path) -> Result<u64, PressurePlanRetainReason> {
    let mut total = 0u64;
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        for child in
            std::fs::read_dir(&path).map_err(|_| PressurePlanRetainReason::TraversalError)?
        {
            let child = child.map_err(|_| PressurePlanRetainReason::TraversalError)?;
            let metadata = std::fs::symlink_metadata(child.path())
                .map_err(|_| PressurePlanRetainReason::MetadataError)?;
            if metadata.file_type().is_symlink() {
                return Err(PressurePlanRetainReason::UnsafeProfile);
            }
            if metadata.is_dir() {
                pending.push(child.path());
            } else if metadata.is_file() {
                total = total.saturating_add(allocated_bytes(&metadata));
            } else {
                return Err(PressurePlanRetainReason::UnsafeProfile);
            }
        }
    }
    Ok(total)
}

#[cfg(unix)]
fn allocated_bytes(metadata: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.blocks().saturating_mul(512)
}
#[cfg(not(unix))]
fn allocated_bytes(metadata: &std::fs::Metadata) -> u64 {
    metadata.len()
}
