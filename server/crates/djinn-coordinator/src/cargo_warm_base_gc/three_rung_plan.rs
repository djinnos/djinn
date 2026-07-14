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
        if let Some(path) = safe_directory(&incremental_path, &actual_base)? {
            incremental.push(profile_unit(
                PressureRung::Incremental,
                path.clone(),
                allocated_tree_bytes(&path)?,
                PressurePlanDisposition::Eligible,
            ));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::UNIX_EPOCH;

    fn snapshot(base: &Path, id: &str, activity: Option<SystemTime>) -> PressureBaseSnapshot {
        PressureBaseSnapshot {
            project_id: id.into(),
            canonical_base: fs::canonicalize(base).expect("canonical base"),
            effective_latest_activity: activity,
        }
    }

    fn mkdir(base: &Path, relative: &str) -> PathBuf {
        let path = base.join(relative);
        fs::create_dir_all(&path).expect("create directory");
        path
    }

    #[test]
    fn plans_top_level_and_triple_profiles_in_global_rung_order() {
        let temp = tempfile::tempdir().expect("tempdir");
        let older = mkdir(temp.path(), "older");
        let newer = mkdir(temp.path(), "newer");
        mkdir(&older, "debug/incremental");
        mkdir(&older, "debug/sibling-preserved");
        mkdir(&older, "release");
        mkdir(&older, "aarch64-unknown-linux-gnu/debug/incremental");
        mkdir(&older, "x86_64-unknown-linux-gnu/release/incremental");
        mkdir(&newer, "test/incremental");
        let now = UNIX_EPOCH + Duration::from_secs(10_000);
        let plan = build_three_rung_pressure_plan(
            vec![
                snapshot(&newer, "newer", Some(UNIX_EPOCH + Duration::from_secs(2))),
                snapshot(&older, "older", Some(UNIX_EPOCH)),
            ],
            now,
            Duration::ZERO,
        );

        let rungs: Vec<_> = plan.units.iter().map(|unit| unit.rung).collect();
        assert_eq!(
            rungs,
            vec![
                PressureRung::Incremental,
                PressureRung::Incremental,
                PressureRung::Incremental,
                PressureRung::Incremental,
                PressureRung::StaleProfile,
                PressureRung::StaleProfile,
                PressureRung::StaleProfile,
                PressureRung::StaleProfile,
                PressureRung::StaleProfile,
                PressureRung::WholeBase,
                PressureRung::WholeBase,
            ]
        );
        let targets: Vec<_> = plan
            .units
            .iter()
            .filter(|unit| unit.rung == PressureRung::StaleProfile)
            .map(|unit| {
                unit.canonical_target
                    .strip_prefix(&older)
                    .ok()
                    .map(Path::to_path_buf)
            })
            .collect();
        assert_eq!(
            targets[..4],
            [
                Some(PathBuf::from("aarch64-unknown-linux-gnu/debug")),
                Some(PathBuf::from("debug")),
                Some(PathBuf::from("release")),
                Some(PathBuf::from("x86_64-unknown-linux-gnu/release")),
            ]
        );
        let debug = plan
            .units
            .iter()
            .find(|unit| {
                unit.rung == PressureRung::StaleProfile
                    && unit.canonical_target == older.join("debug")
            })
            .expect("debug profile");
        assert_ne!(
            debug.canonical_target,
            older.join("debug/sibling-preserved")
        );
        assert_eq!(plan.units[9].canonical_target, older);
        assert_eq!(plan.units[10].canonical_target, newer);
    }

    #[test]
    fn profile_staleness_uses_activity_only_and_allocated_bytes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let base = mkdir(temp.path(), "base");
        let incremental = mkdir(&base, "debug/incremental");
        fs::write(incremental.join("artifact"), vec![7u8; 4096]).expect("artifact");
        let now = UNIX_EPOCH + Duration::from_secs(48 * 60 * 60);
        let active = build_three_rung_pressure_plan(
            vec![snapshot(
                &base,
                "base",
                Some(now - Duration::from_secs(60 * 60)),
            )],
            now,
            Duration::from_secs(24 * 60 * 60),
        );
        assert!(matches!(
            active.units[1].disposition,
            PressurePlanDisposition::Retained(PressurePlanRetainReason::ProfileNotIdle)
        ));
        assert!(active.units[0].projected_allocated_bytes > 0);
        let immediate = build_three_rung_pressure_plan(
            vec![snapshot(&base, "base", Some(now))],
            now,
            Duration::ZERO,
        );
        assert_eq!(
            immediate.units[1].disposition,
            PressurePlanDisposition::Eligible
        );
    }

    #[test]
    fn ties_are_deterministic_and_unsafe_layouts_fail_closed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let a = mkdir(temp.path(), "a");
        let b = mkdir(temp.path(), "b");
        mkdir(&a, "debug");
        mkdir(&b, "debug");
        let now = UNIX_EPOCH + Duration::from_secs(100);
        let first = build_three_rung_pressure_plan(
            vec![
                snapshot(&b, "b", Some(UNIX_EPOCH)),
                snapshot(&a, "a", Some(UNIX_EPOCH)),
            ],
            now,
            Duration::ZERO,
        );
        let second = build_three_rung_pressure_plan(
            vec![
                snapshot(&a, "a", Some(UNIX_EPOCH)),
                snapshot(&b, "b", Some(UNIX_EPOCH)),
            ],
            now,
            Duration::ZERO,
        );
        assert_eq!(first, second);
        assert_eq!(first.units.last().expect("whole base").canonical_target, b);

        let file = temp.path().join("not-a-directory");
        fs::write(&file, "x").expect("file");
        let missing = temp.path().join("missing");
        let rejected = build_three_rung_pressure_plan(
            vec![
                PressureBaseSnapshot {
                    project_id: "file".into(),
                    canonical_base: file,
                    effective_latest_activity: Some(UNIX_EPOCH),
                },
                PressureBaseSnapshot {
                    project_id: "missing".into(),
                    canonical_base: missing,
                    effective_latest_activity: Some(UNIX_EPOCH),
                },
            ],
            now,
            Duration::ZERO,
        );
        assert!(
            rejected
                .units
                .iter()
                .all(|unit| matches!(unit.disposition, PressurePlanDisposition::Retained(_)))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_link_beneath_a_valid_incremental_tree_rejects_the_entire_base() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("tempdir");
        let base = mkdir(temp.path(), "base");
        let outside = mkdir(temp.path(), "outside");
        fs::write(outside.join("must-not-be-counted"), vec![0u8; 4096]).expect("outside file");
        let incremental = mkdir(&base, "debug/incremental");
        symlink(&outside, incremental.join("outside-link")).expect("tree link");

        let plan = build_three_rung_pressure_plan(
            vec![snapshot(&base, "base", Some(UNIX_EPOCH))],
            UNIX_EPOCH + Duration::from_secs(1),
            Duration::ZERO,
        );
        assert_eq!(plan.units.len(), 1);
        assert_eq!(plan.units[0].rung, PressureRung::WholeBase);
        assert_eq!(plan.units[0].projected_allocated_bytes, 0);
        assert_eq!(
            plan.units[0].disposition,
            PressurePlanDisposition::Retained(PressurePlanRetainReason::UnsafeProfile)
        );
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_profile_traversal_rejects_the_entire_base() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("tempdir");
        let base = mkdir(temp.path(), "base");
        let incremental = mkdir(&base, "debug/incremental");
        fs::write(incremental.join("artifact"), b"artifact").expect("artifact");
        let profile_root = base.join("debug");
        let original = fs::metadata(&profile_root)
            .expect("profile metadata")
            .permissions();
        let mut unreadable = original.clone();
        unreadable.set_mode(0o000);
        fs::set_permissions(&profile_root, unreadable).expect("remove profile permissions");

        let plan = build_three_rung_pressure_plan(
            vec![snapshot(&base, "base", Some(UNIX_EPOCH))],
            UNIX_EPOCH + Duration::from_secs(1),
            Duration::ZERO,
        );

        fs::set_permissions(&profile_root, original).expect("restore profile permissions");
        assert_eq!(plan.units.len(), 1);
        assert_eq!(plan.units[0].rung, PressureRung::WholeBase);
        assert_eq!(
            plan.units[0].disposition,
            PressurePlanDisposition::Retained(PressurePlanRetainReason::TraversalError)
        );
    }
}
