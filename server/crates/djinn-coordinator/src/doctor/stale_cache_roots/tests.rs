//! Tests for the cache-root manifest check.
//!
//! Every test drives a real temporary directory tree: the check's whole value
//! is that it notices things nobody enumerated, so a fake filesystem would
//! attest the classifier against exactly the cases the author remembered.

use super::*;
use djinn_core::doctor::{DoctorRegistry, doctor_run};
use std::fs;
use std::time::{Duration, SystemTime};

const DAY: u64 = 86_400;
const NOW: u64 = 1_800_000_000;

fn live(ids: &[&str]) -> LiveProjectSet {
    LiveProjectSet::from_enumeration(ids.iter().map(|id| (*id).to_owned()))
}

/// Create `path` with `bytes` of content and an mtime `age_days` in the past.
fn write_aged_file(path: &Path, bytes: usize, age_days: u64) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(path, vec![b'x'; bytes]).expect("write file");
    set_age(path, age_days);
}

fn set_age(path: &Path, age_days: u64) {
    let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(NOW - age_days * DAY);
    let times = fs::FileTimes::new().set_accessed(mtime).set_modified(mtime);
    let file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .or_else(|_| fs::File::open(path))
        .expect("open for utime");
    file.set_times(times).expect("set times");
}

/// Age a directory *after* its contents are in place (writing a child bumps the
/// directory mtime).
fn set_dir_age(path: &Path, age_days: u64) {
    set_age(path, age_days);
}

fn scan(root: &Path, live: &LiveProjectSet) -> CacheScanReport {
    scan_cache_roots_under(root, live, NOW, CacheScanConfig::default())
}

fn classes(report: &CacheScanReport) -> Vec<(String, CacheObservationClass)> {
    report
        .observations
        .iter()
        .map(|obs| (obs.relative_path.clone(), obs.class.clone()))
        .collect()
}

fn find<'a>(report: &'a CacheScanReport, relative_path: &str) -> Option<&'a CacheObservation> {
    report
        .observations
        .iter()
        .find(|obs| obs.relative_path == relative_path)
}

// ---------------------------------------------------------------------------
// (a) unrecognised roots
// ---------------------------------------------------------------------------

/// The headline case: a directory nobody declared is reported, with the size,
/// age and uid an operator needs to judge it.
#[test]
fn unrecognised_root_is_reported_with_size_age_and_uid() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let retired = root.join("retired-subsystem");
    write_aged_file(&retired.join("blob-a"), 4_096, 40);
    write_aged_file(&retired.join("nested/blob-b"), 2_048, 45);
    set_dir_age(&retired.join("nested"), 40);
    set_dir_age(&retired, 40);

    let report = scan(root, &live(&["project-live"]));

    let observed = find(&report, "retired-subsystem").expect("unrecognised root reported");
    assert_eq!(observed.class, CacheObservationClass::UnrecognisedRoot);
    assert_eq!(observed.kind, CacheEntryKind::Dir);
    assert_eq!(
        observed.measurement.size_bytes, 6_144,
        "size must be the recursive byte total"
    );
    assert!(!observed.measurement.size_truncated);
    assert_eq!(
        observed.age_seconds,
        Some(40 * DAY),
        "age must come from the newest mtime in the tree"
    );
    assert!(
        observed.measurement.uid.is_some(),
        "owning uid must be reported so an operator can attribute the directory"
    );

    let findings = StaleCacheRootsCheck::findings_for(&report);
    let finding = findings
        .iter()
        .find(|f| {
            f.entity_ids.get("relative_path").map(String::as_str) == Some("retired-subsystem")
        })
        .expect("finding for the unrecognised root");
    assert_eq!(finding.severity, FindingSeverity::Warn);
    assert!(
        finding.detail.contains("6144") && finding.detail.contains("40 days"),
        "detail must carry size and age: {}",
        finding.detail
    );
}

/// The inverse assertion, and the one that proves the check is a manifest and
/// not a match-anything reporter: every declared root is silent.
#[test]
fn manifest_roots_are_not_reported() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    for declared in CacheRootId::ALL.iter().copied() {
        let path = root.join(declared.dir_name());
        write_aged_file(&path.join("recent-content"), 128, 0);
        set_dir_age(&path, 0);
    }

    let report = scan(root, &live(&["project-live"]));

    assert!(
        report.observations.is_empty(),
        "a cache root holding only declared, freshly-written roots must be silent, got {:?}",
        classes(&report)
    );
    assert!(StaleCacheRootsCheck::findings_for(&report).is_empty());
}

/// A loose file at the cache root is not a cache root under any reading of the
/// manifest. The live PVC has 281 of these.
#[test]
fn loose_file_at_the_cache_root_is_reported() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    write_aged_file(&root.join("clippy.log"), 512, 60);

    let report = scan(root, &live(&["project-live"]));

    let observed = find(&report, "clippy.log").expect("loose file reported");
    assert_eq!(observed.class, CacheObservationClass::UnrecognisedEntry);
    assert_eq!(observed.kind, CacheEntryKind::File);
    assert_eq!(observed.measurement.size_bytes, 512);
    assert_eq!(observed.age_seconds, Some(60 * DAY));
}

/// A declared root that nothing writes any more is the sccache shape: still in
/// the manifest, still rendered, but idle. Reported as Info so retiring the
/// manifest entry becomes a visible decision.
#[test]
fn declared_but_idle_root_is_reported_as_info() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let sccache = root.join(CacheRootId::Sccache.dir_name());
    write_aged_file(&sccache.join("project-live/objects"), 1_024, 40);
    set_dir_age(&sccache.join("project-live"), 40);
    set_dir_age(&sccache, 40);

    let cargo = root.join(CacheRootId::Cargo.dir_name());
    write_aged_file(&cargo.join("registry/index"), 64, 0);
    set_dir_age(&cargo, 0);

    let report = scan(root, &live(&["project-live"]));

    let observed = find(&report, "sccache").expect("idle declared root reported");
    assert_eq!(
        observed.class,
        CacheObservationClass::DeclaredRootIdle { root: "sccache" }
    );
    assert_eq!(observed.measurement.size_bytes, 1_024);
    assert_eq!(observed.age_seconds, Some(40 * DAY));
    assert!(
        find(&report, "cargo").is_none(),
        "a freshly-written declared root must not be called idle"
    );

    let findings = StaleCacheRootsCheck::findings_for(&report);
    let finding = findings
        .iter()
        .find(|f| f.entity_ids.get("relative_path").map(String::as_str) == Some("sccache"))
        .expect("finding for the idle root");
    assert_eq!(finding.severity, FindingSeverity::Info);
    assert!(
        finding.detail.contains("remove its manifest entry"),
        "detail must point at the retirement action: {}",
        finding.detail
    );
}

// ---------------------------------------------------------------------------
// (3) per-run churn is not staleness
// ---------------------------------------------------------------------------

/// `cargo-target-runs/<uuid>` directories are created and destroyed by every
/// task run. Old ones are normal, not evidence of a retired subsystem, and a
/// dedicated sweep owns them — the manifest is about roots, not their contents.
#[test]
fn per_run_churn_under_a_known_root_is_not_reported() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let runs = root.join(CacheRootId::CargoTargetRuns.dir_name());
    for (id, age) in [
        ("019ea3bd-a305-73e3-806c-4edcc96ebfe2", 0),
        ("019eb111-1111-7111-8111-111111111111", 90),
    ] {
        write_aged_file(&runs.join(id).join("debug/build"), 256, age);
        set_dir_age(&runs.join(id), age);
    }
    set_dir_age(&runs, 0);

    let report = scan(root, &live(&["project-live"]));

    assert!(
        report.observations.is_empty(),
        "per-run directories must never be reported, got {:?}",
        classes(&report)
    );
    assert!(
        !report
            .reconciled_roots
            .contains(&CacheRootId::CargoTargetRuns.dir_name()),
        "a PerRun root must never be reconciled against the project set"
    );
}

/// Contents of a Shared root are content-addressed across every tenant, so no
/// child of one is attributable to a project.
#[test]
fn shared_root_children_are_never_reconciled() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let cargo = root.join(CacheRootId::Cargo.dir_name());
    write_aged_file(&cargo.join("registry/serde-1.0.0/lib.rs"), 32, 0);
    set_dir_age(&cargo, 0);

    let report = scan(root, &live(&["project-live"]));

    assert!(
        report.observations.is_empty(),
        "shared cache contents are not tenant namespaces, got {:?}",
        classes(&report)
    );
    assert!(!report.reconciled_roots.contains(&"cargo"));
}

// ---------------------------------------------------------------------------
// (b) orphaned per-project namespaces
// ---------------------------------------------------------------------------

#[test]
fn orphaned_project_namespace_is_reported_and_live_one_is_not() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let warm = root.join(CacheRootId::CargoTarget.dir_name());
    write_aged_file(
        &warm.join("live-project/mold-jobs-4/debug/deps/a.rlib"),
        900,
        1,
    );
    write_aged_file(
        &warm.join("gone-project/mold-jobs-4/debug/deps/b.rlib"),
        700,
        70,
    );
    set_dir_age(&warm.join("gone-project"), 70);
    // djinn's own flock directory, keyed by project id but reserved.
    fs::create_dir_all(warm.join(".warm-locks/live-project")).expect("lock dir");
    set_dir_age(&warm, 1);

    let report = scan(root, &live(&["live-project"]));

    assert_eq!(
        classes(&report),
        vec![(
            "cargo-target/gone-project".to_owned(),
            CacheObservationClass::OrphanedProjectNamespace {
                root: "cargo-target",
                project_id: "gone-project".to_owned(),
            }
        )],
        "only the namespace whose project is absent may be reported"
    );
    let observed = find(&report, "cargo-target/gone-project").expect("orphan reported");
    assert_eq!(observed.measurement.size_bytes, 700);
    assert_eq!(observed.age_seconds, Some(70 * DAY));

    let findings = StaleCacheRootsCheck::findings_for(&report);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].severity, FindingSeverity::Warn);
    assert_eq!(
        findings[0].entity_ids.get("project_id").map(String::as_str),
        Some("gone-project"),
        "the owning project id must be on the finding so an operator can check the claim"
    );
}

/// Fail-closed #1: a transient enumeration failure must produce no orphan
/// claim, and must say that it produced none.
#[test]
fn unavailable_project_set_claims_no_orphans_and_says_so() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let warm = root.join(CacheRootId::CargoTarget.dir_name());
    write_aged_file(&warm.join("some-project/mold-jobs-4/x"), 10, 5);
    set_dir_age(&warm, 5);

    let report = scan(
        root,
        &LiveProjectSet::unavailable("project_enumeration_failed: connection reset"),
    );

    assert!(
        report.observations.is_empty(),
        "no namespace may be called orphaned without a live owner set, got {:?}",
        classes(&report)
    );
    assert!(report.reconciled_roots.is_empty());

    let findings = StaleCacheRootsCheck::findings_for(&report);
    assert_eq!(findings.len(), 1, "the skip must be visible");
    assert_eq!(findings[0].severity, FindingSeverity::Info);
    assert!(
        findings[0].detail.contains("skipped") && findings[0].detail.contains("connection reset"),
        "the skip finding must name the reason: {}",
        findings[0].detail
    );
}

/// Fail-closed #2: an *empty* enumeration is indistinguishable from a
/// mis-scoped query. Reporting orphans here would flag every live project's
/// warm base on the deployment.
#[test]
fn empty_project_enumeration_is_treated_as_unavailable() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let warm = root.join(CacheRootId::CargoTarget.dir_name());
    write_aged_file(&warm.join("project-a/mold-jobs-4/x"), 10, 5);
    write_aged_file(&warm.join("project-b/mold-jobs-4/x"), 10, 5);
    set_dir_age(&warm, 5);

    let empty: [&str; 0] = [];
    let set = live(&empty);
    assert!(
        matches!(set, LiveProjectSet::Unavailable { .. }),
        "an empty enumeration must not become an empty Known set"
    );

    let report = scan(root, &set);
    assert!(
        report.observations.is_empty(),
        "an empty project set must not orphan every namespace, got {:?}",
        classes(&report)
    );
}

/// Every per-project root is reconciled, not just the one somebody remembered.
#[test]
fn every_per_project_root_is_reconciled() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let per_project: Vec<CacheRootId> = CacheRootId::ALL
        .iter()
        .copied()
        .filter(|id| id.namespacing() == CacheRootNamespacing::PerProject)
        .collect();
    assert!(
        per_project.len() >= 2,
        "manifest must have per-project roots"
    );
    for id in &per_project {
        let path = root.join(id.dir_name()).join("gone-project");
        write_aged_file(&path.join("content"), 16, 3);
        set_dir_age(&root.join(id.dir_name()), 3);
    }

    let report = scan(root, &live(&["live-project"]));

    let mut reported: Vec<String> = report
        .observations
        .iter()
        .map(|obs| obs.relative_path.clone())
        .collect();
    reported.sort();
    let mut expected: Vec<String> = per_project
        .iter()
        .map(|id| format!("{}/gone-project", id.dir_name()))
        .collect();
    expected.sort();
    assert_eq!(reported, expected);
}

// ---------------------------------------------------------------------------
// path resolution + wiring
// ---------------------------------------------------------------------------

/// The production entry point must resolve through `djinn_core::paths`, which
/// mounts the cache PVC at `$DJINN_HOME/cache` in the server pod. A hardcoded
/// Job-pod `/cache` here would make the whole check a permanent no-op — the
/// exact bug PR #2660 fixed three times over.
#[test]
fn production_scan_resolves_the_cache_root_through_paths() {
    let report = scan_production_cache_root(
        &LiveProjectSet::unavailable("test"),
        NOW,
        CacheScanConfig::default(),
    );
    assert_eq!(report.cache_root, djinn_core::paths::cache_root());
    assert_ne!(
        report.cache_root,
        PathBuf::from(djinn_core::paths::JOB_POD_CACHE_MOUNT),
        "the server pod has no /cache; the scan must use the host mount"
    );
    for root in CacheRootId::ALL.iter().copied() {
        assert!(
            root.host_path().starts_with(&report.cache_root),
            "{} must live under the scanned root",
            root.dir_name()
        );
    }
}

#[test]
fn check_reports_findings_through_the_doctor_registry() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    write_aged_file(&root.join("retired-subsystem/blob"), 100, 50);
    set_dir_age(&root.join("retired-subsystem"), 50);

    let report = scan(root, &live(&["live-project"]));
    let registry = DoctorRegistry::new();
    let replaced = crate::doctor::register_stale_cache_roots_check(
        &registry,
        Arc::new(MemoryStaleCacheRootsSource::new(report)),
    );
    assert!(replaced.is_none());

    let results = doctor_run(&registry, Some(&[STALE_CACHE_ROOTS_CHECK_NAME])).expect("run");
    assert_eq!(results.len(), 1);
    let (name, findings) = &results[0];
    assert_eq!(name, STALE_CACHE_ROOTS_CHECK_NAME);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].severity, FindingSeverity::Warn);
}

/// No deletion path exists, so no arming can be granted on vacuous evidence.
#[test]
fn check_never_offers_a_fix() {
    let temp = tempfile::tempdir().expect("tempdir");
    let report = scan(temp.path(), &live(&["live-project"]));
    let check = StaleCacheRootsCheck::new(Arc::new(MemoryStaleCacheRootsSource::new(report)));
    let finding = Finding::new(
        FindingSeverity::Warn,
        STALE_CACHE_ROOTS_CHECK_NAME,
        ResolverSnapshot::new("resolve_stale_cache_roots", json!({}), json!({})),
        "detail",
    );
    assert!(matches!(
        check.fix(&finding),
        Err(djinn_core::doctor::DoctorError::FixNotSupported { .. })
    ));
}

/// A source that has never produced a scan must not read as "all clean".
#[test]
fn missing_scan_reports_its_own_absence() {
    let check = StaleCacheRootsCheck::new(Arc::new(MemoryStaleCacheRootsSource::empty()));
    let findings = check.run().expect("run");
    assert_eq!(findings.len(), 1);
    assert!(findings[0].detail.contains("has not completed"));
}

/// A missing cache root (fresh deployment, local dev) is a non-event.
#[test]
fn absent_cache_root_reports_nothing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let report = scan(&temp.path().join("no-such-cache"), &live(&["p"]));
    assert!(!report.cache_root_exists);
    assert!(report.observations.is_empty());
}

/// The measurement is bounded, and says when it stopped short, so a huge tree
/// cannot turn an on-demand check into an unbounded walk.
#[test]
fn measurement_is_budgeted_and_reports_truncation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let big = root.join("big-unrecognised");
    for index in 0..12 {
        write_aged_file(&big.join(format!("file-{index}")), 100, 5);
    }
    set_dir_age(&big, 5);

    let report = scan_cache_roots_under(
        root,
        &live(&["p"]),
        NOW,
        CacheScanConfig {
            idle_threshold_days: DEFAULT_IDLE_THRESHOLD_DAYS,
            entry_budget: 5,
        },
    );
    let observed = find(&report, "big-unrecognised").expect("reported");
    assert!(observed.measurement.size_truncated);
    assert!(observed.measurement.size_bytes < 1_200);
}
