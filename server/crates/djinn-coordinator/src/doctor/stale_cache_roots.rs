//! Report-only detector for cache directories nothing points at any more.
//!
//! # Why an allowlist
//!
//! Every other cache cleanup in this crate is a *denylist*: one hardcoded guard
//! per remembered path (the sccache guard, the warm-base idle GC, the
//! cargo-debris sweep, the pressure sweep). A denylist can only clean paths
//! somebody enumerated, so a retired cache root is invisible to it *by
//! construction* — which is how `/cache/sccache` sat at 6.1 GB for forty days
//! with nothing writing it.
//!
//! This check inverts that. [`djinn_core::paths::CacheRootId`] is a closed,
//! macro-generated manifest of every cache root djinn actually uses; anything
//! under the cache root that is not in the manifest is reported. New junk is
//! caught without anyone having predicted it.
//!
//! # Two kinds of stale
//!
//! - **An unrecognised root** — a whole subsystem that was retired, or a
//!   directory an ad-hoc command dropped on the PVC.
//! - **An orphaned tenant namespace** — a *recognised* root containing a
//!   per-project namespace for a project that no longer exists in the database.
//!   On a shared, multi-tenant deployment this is the recurring case: removing
//!   a project leaves its warm base, sccache and XDG namespaces behind forever.
//!
//! # Report-only, by construction
//!
//! There is no deletion path here at all — not a disabled one, not an
//! arming flag. `fix` is the trait default (`FixNotSupported`). PR #2660 found
//! that sccache deletion had already been "authorised" by a dry-run watching a
//! path that does not exist in the server pod, so the authorising evidence was
//! structurally guaranteed to be empty. A detector that cannot delete cannot be
//! armed on vacuous evidence.
//!
//! # Fail-closed reconciliation
//!
//! Claiming a namespace is orphaned means claiming its owner is *gone*. A false
//! positive here, if anyone ever acted on it, deletes every warm base on the
//! deployment. So the reconciliation only ever narrows:
//!
//! - a transient enumeration failure yields **no** orphan claims;
//! - an **empty** project enumeration also yields no orphan claims, because an
//!   empty result is indistinguishable from a mis-scoped query — the price is
//!   that a genuinely projectless deployment never reports orphans;
//! - names the manifest reserves for djinn's own machinery (`.warm-locks`) are
//!   never treated as tenant namespaces;
//! - only *directories* directly under a `PerProject` root are considered.
//!
//! Whenever reconciliation is skipped the check says so with an explicit
//! finding, so a skipped run never reads as a clean run.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use djinn_core::doctor::{
    DoctorCheck, DoctorCheckCadence, DoctorResult, Finding, FindingSeverity, ResolverSnapshot,
};
use djinn_core::paths::{CacheRootId, CacheRootNamespacing};
use serde::Serialize;
use serde_json::json;
use std::sync::Arc;

pub const STALE_CACHE_ROOTS_CHECK_NAME: &str = "cache.stale_roots";

/// Age past which an unrecognised entry, or a declared-but-untouched root, is
/// worth an operator's attention.
pub const DEFAULT_IDLE_THRESHOLD_DAYS: u64 = 30;

/// Directory entries a single measurement may visit before it gives up and
/// reports a lower bound. Bounds the cost of walking a multi-gigabyte tree.
pub const DEFAULT_ENTRY_BUDGET: usize = 200_000;

const SECONDS_PER_DAY: u64 = 86_400;

/// Tunables for one scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheScanConfig {
    pub idle_threshold_days: u64,
    pub entry_budget: usize,
}

impl Default for CacheScanConfig {
    fn default() -> Self {
        Self {
            idle_threshold_days: DEFAULT_IDLE_THRESHOLD_DAYS,
            entry_budget: DEFAULT_ENTRY_BUDGET,
        }
    }
}

impl CacheScanConfig {
    fn idle_threshold_seconds(self) -> u64 {
        self.idle_threshold_days.saturating_mul(SECONDS_PER_DAY)
    }
}

/// The live tenant set, or a positive statement that it could not be
/// established. There is deliberately no third "empty but fine" state: see the
/// module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveProjectSet {
    /// Positively enumerated and non-empty.
    Known(BTreeSet<String>),
    /// Not established. No namespace may be called orphaned.
    Unavailable { reason: String },
}

impl LiveProjectSet {
    /// Build from a completed enumeration. An empty enumeration is treated as
    /// *unavailable*, not as "every namespace is orphaned".
    pub fn from_enumeration(ids: impl IntoIterator<Item = String>) -> Self {
        let ids: BTreeSet<String> = ids.into_iter().collect();
        if ids.is_empty() {
            Self::unavailable("empty_project_enumeration")
        } else {
            Self::Known(ids)
        }
    }

    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::Unavailable {
            reason: reason.into(),
        }
    }

    fn known(&self) -> Option<&BTreeSet<String>> {
        match self {
            Self::Known(ids) => Some(ids),
            Self::Unavailable { .. } => None,
        }
    }
}

/// What the scan found at a path, in filesystem terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheEntryKind {
    Dir,
    File,
    Symlink,
    Other,
}

/// Size / recency / ownership facts for one path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Measurement {
    /// Total bytes of regular files at or under the path. A lower bound when
    /// `size_truncated` is set.
    pub size_bytes: u64,
    /// The walk hit [`CacheScanConfig::entry_budget`] and stopped early.
    pub size_truncated: bool,
    /// Newest mtime at or under the path, as a Unix timestamp.
    pub newest_mtime_unix: Option<u64>,
    /// Owning uid of the path itself.
    pub uid: Option<u32>,
}

/// Why a path was reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "class", rename_all = "snake_case")]
pub enum CacheObservationClass {
    /// A directory directly under the cache root with no manifest entry.
    UnrecognisedRoot,
    /// A non-directory directly under the cache root. The manifest describes
    /// directories, so any loose file here is by definition undeclared.
    UnrecognisedEntry,
    /// A per-project namespace under a declared root whose project is absent
    /// from a positively-enumerated, non-empty live project set.
    OrphanedProjectNamespace {
        root: &'static str,
        project_id: String,
    },
    /// A manifest root that exists but has not been written for longer than the
    /// idle threshold. Not junk by itself — but if the subsystem was retired,
    /// this is the signal to delete its manifest entry.
    DeclaredRootIdle { root: &'static str },
}

/// One reported path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CacheObservation {
    /// Path relative to the cache root (`"sccache"`, `"cargo-target/<id>"`).
    pub relative_path: String,
    pub kind: CacheEntryKind,
    #[serde(flatten)]
    pub class: CacheObservationClass,
    #[serde(flatten)]
    pub measurement: Measurement,
    /// Seconds since `newest_mtime_unix`, at scan time.
    pub age_seconds: Option<u64>,
}

impl CacheObservation {
    fn age_days(&self) -> Option<u64> {
        self.age_seconds.map(|secs| secs / SECONDS_PER_DAY)
    }
}

/// Outcome of the tenant enumeration, carried into the report so a skipped
/// reconciliation is visible rather than silent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ProjectSetStatus {
    Known { project_count: usize },
    Unavailable { reason: String },
}

/// Everything one scan observed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CacheScanReport {
    pub cache_root: PathBuf,
    pub cache_root_exists: bool,
    pub project_set: ProjectSetStatus,
    pub idle_threshold_days: u64,
    pub scanned_at_unix: u64,
    /// Roots that were reconciled against the live project set. Empty when the
    /// set was unavailable.
    pub reconciled_roots: Vec<&'static str>,
    pub observations: Vec<CacheObservation>,
}

// ---------------------------------------------------------------------------
// Scanning
// ---------------------------------------------------------------------------

/// Scan `cache_root` and classify everything directly under it.
///
/// `cache_root` is passed explicitly so the scan is testable against a tempdir;
/// production callers resolve it through [`djinn_core::paths::cache_root`] and
/// never hardcode a path (the server pod mounts the cache PVC at
/// `$DJINN_HOME/cache`, not at the Job-pod `/cache`).
///
/// # Granularity
///
/// The manifest is about cache *roots*, so the scan classifies the immediate
/// children of `cache_root` and stops there — except for `PerProject` roots,
/// where it descends exactly one level to reach tenant namespaces. It never
/// descends into a `PerRun` root: `cargo-target-runs/<id>` directories are
/// created and destroyed by every task run, so their contents carry no
/// staleness signal and a dedicated sweep already owns them.
pub fn scan_cache_roots_under(
    cache_root: &Path,
    live_projects: &LiveProjectSet,
    now_unix: u64,
    config: CacheScanConfig,
) -> CacheScanReport {
    let project_set = match live_projects {
        LiveProjectSet::Known(ids) => ProjectSetStatus::Known {
            project_count: ids.len(),
        },
        LiveProjectSet::Unavailable { reason } => ProjectSetStatus::Unavailable {
            reason: reason.clone(),
        },
    };
    let mut report = CacheScanReport {
        cache_root: cache_root.to_path_buf(),
        cache_root_exists: false,
        project_set,
        idle_threshold_days: config.idle_threshold_days,
        scanned_at_unix: now_unix,
        reconciled_roots: Vec::new(),
        observations: Vec::new(),
    };

    let entries = match std::fs::read_dir(cache_root) {
        Ok(entries) => entries,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    path = %cache_root.display(),
                    %error,
                    "stale_cache_roots: cache root unreadable; reporting nothing"
                );
            }
            return report;
        }
    };
    report.cache_root_exists = true;

    let mut named: Vec<(String, CacheEntryKind)> = Vec::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        named.push((name, entry_kind(&entry)));
    }
    named.sort_by(|left, right| left.0.cmp(&right.0));

    for (name, kind) in named {
        let path = cache_root.join(&name);
        match (kind, CacheRootId::from_dir_name(&name)) {
            (CacheEntryKind::Dir, Some(root)) => {
                classify_declared_root(&mut report, root, &path, live_projects, now_unix, config);
            }
            (CacheEntryKind::Dir, None) => {
                let measurement = measure(&path, config.entry_budget);
                report.observations.push(observation(
                    name,
                    CacheEntryKind::Dir,
                    CacheObservationClass::UnrecognisedRoot,
                    measurement,
                    now_unix,
                ));
            }
            (kind, _) => {
                // A non-directory directly under the cache root: not a cache
                // root under any reading of the manifest.
                let measurement = measure(&path, config.entry_budget);
                report.observations.push(observation(
                    name,
                    kind,
                    CacheObservationClass::UnrecognisedEntry,
                    measurement,
                    now_unix,
                ));
            }
        }
    }

    report
}

fn classify_declared_root(
    report: &mut CacheScanReport,
    root: CacheRootId,
    path: &Path,
    live_projects: &LiveProjectSet,
    now_unix: u64,
    config: CacheScanConfig,
) {
    // Idleness gate first, and cheaply: only the root's own mtime and its
    // immediate children's. A live root (cargo-target is tens of gigabytes) is
    // therefore never deep-walked.
    if let Some(newest) = shallow_newest_mtime(path) {
        let age = now_unix.saturating_sub(newest);
        if age >= config.idle_threshold_seconds() {
            let measurement = measure(path, config.entry_budget);
            report.observations.push(observation(
                root.dir_name().to_owned(),
                CacheEntryKind::Dir,
                CacheObservationClass::DeclaredRootIdle {
                    root: root.dir_name(),
                },
                measurement,
                now_unix,
            ));
        }
    }

    // Exhaustive on purpose: a new namespacing kind (a per-user cache root, say
    // — none exists today, the only tenant key rendered into a cache path is
    // `project_id`) must not silently inherit "descend and reconcile against
    // projects".
    match root.namespacing() {
        CacheRootNamespacing::PerProject => {
            reconcile_project_namespaces(report, root, path, live_projects, now_unix, config);
        }
        // Content-addressed across every tenant: nothing under it is
        // attributable to a project, so nothing under it is reclaimable when a
        // project goes away.
        CacheRootNamespacing::Shared => {}
        // Churn by design; owned by the run-dir sweep.
        CacheRootNamespacing::PerRun => {}
    }
}

fn reconcile_project_namespaces(
    report: &mut CacheScanReport,
    root: CacheRootId,
    path: &Path,
    live_projects: &LiveProjectSet,
    now_unix: u64,
    config: CacheScanConfig,
) {
    let Some(live) = live_projects.known() else {
        // Fail closed: without a positively-established owner set there is no
        // basis for calling anything orphaned.
        return;
    };
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "stale_cache_roots: namespace root unreadable; skipping reconciliation"
            );
            return;
        }
    };
    report.reconciled_roots.push(root.dir_name());

    let mut names: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        // Only directories are namespaces. Anything else under a namespace root
        // is left alone rather than guessed at.
        if entry_kind(&entry) != CacheEntryKind::Dir {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        names.push(name);
    }
    names.sort();

    for name in names {
        if root.spec().reserved_children.contains(&name.as_str()) {
            continue;
        }
        if live.contains(&name) {
            continue;
        }
        let child = path.join(&name);
        let measurement = measure(&child, config.entry_budget);
        report.observations.push(observation(
            format!("{}/{name}", root.dir_name()),
            CacheEntryKind::Dir,
            CacheObservationClass::OrphanedProjectNamespace {
                root: root.dir_name(),
                project_id: name,
            },
            measurement,
            now_unix,
        ));
    }
}

fn observation(
    relative_path: String,
    kind: CacheEntryKind,
    class: CacheObservationClass,
    measurement: Measurement,
    now_unix: u64,
) -> CacheObservation {
    let age_seconds = measurement
        .newest_mtime_unix
        .map(|mtime| now_unix.saturating_sub(mtime));
    CacheObservation {
        relative_path,
        kind,
        class,
        measurement,
        age_seconds,
    }
}

fn entry_kind(entry: &std::fs::DirEntry) -> CacheEntryKind {
    match entry.file_type() {
        Ok(file_type) if file_type.is_dir() => CacheEntryKind::Dir,
        Ok(file_type) if file_type.is_file() => CacheEntryKind::File,
        Ok(file_type) if file_type.is_symlink() => CacheEntryKind::Symlink,
        Ok(_) => CacheEntryKind::Other,
        Err(_) => CacheEntryKind::Other,
    }
}

fn mtime_unix(metadata: &std::fs::Metadata) -> Option<u64> {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|delta| delta.as_secs())
}

fn uid_of(metadata: &std::fs::Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(metadata.uid())
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

/// Newest mtime of `path` and its immediate children. Cheap enough to run
/// against a live multi-gigabyte root on every scan.
fn shallow_newest_mtime(path: &Path) -> Option<u64> {
    let mut newest = std::fs::symlink_metadata(path)
        .ok()
        .as_ref()
        .and_then(mtime_unix);
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            if let Some(child) = entry.metadata().ok().as_ref().and_then(mtime_unix) {
                newest = Some(newest.map_or(child, |current: u64| current.max(child)));
            }
        }
    }
    newest
}

/// Total size, newest mtime and owning uid at or under `path`, visiting at most
/// `entry_budget` directory entries.
fn measure(path: &Path, entry_budget: usize) -> Measurement {
    let top = std::fs::symlink_metadata(path).ok();
    let mut measurement = Measurement {
        size_bytes: 0,
        size_truncated: false,
        newest_mtime_unix: top.as_ref().and_then(mtime_unix),
        uid: top.as_ref().and_then(uid_of),
    };
    let Some(top) = top else {
        return measurement;
    };
    if !top.is_dir() {
        measurement.size_bytes = top.len();
        return measurement;
    }

    let mut visited = 0usize;
    let mut stack: Vec<PathBuf> = vec![path.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            if visited >= entry_budget {
                measurement.size_truncated = true;
                return measurement;
            }
            visited += 1;
            // symlink_metadata: never follow a link out of the cache tree, and
            // never double-count its target.
            let Ok(metadata) = entry.path().symlink_metadata() else {
                continue;
            };
            if let Some(mtime) = mtime_unix(&metadata) {
                measurement.newest_mtime_unix = Some(
                    measurement
                        .newest_mtime_unix
                        .map_or(mtime, |current| current.max(mtime)),
                );
            }
            if metadata.is_dir() {
                stack.push(entry.path());
            } else if metadata.is_file() {
                measurement.size_bytes = measurement.size_bytes.saturating_add(metadata.len());
            }
        }
    }
    measurement
}

/// Production entry point: scan the host's own view of the cache PVC.
///
/// Resolves through [`djinn_core::paths::cache_root`] and nothing else. The
/// server pod mounts the claim at `$DJINN_HOME/cache` while Job pods mount it
/// at `/cache`; a Job-pod literal in server-pod code is a silent no-op, which
/// is precisely how four cache bugs shipped.
pub fn scan_production_cache_root(
    live_projects: &LiveProjectSet,
    now_unix: u64,
    config: CacheScanConfig,
) -> CacheScanReport {
    scan_cache_roots_under(
        &djinn_core::paths::cache_root(),
        live_projects,
        now_unix,
        config,
    )
}

// ---------------------------------------------------------------------------
// Doctor check
// ---------------------------------------------------------------------------

/// Read-only source of the latest scan.
pub trait StaleCacheRootsSource: Send + Sync {
    /// The most recent report, or `None` when no scan has completed.
    fn report(&self) -> Option<CacheScanReport>;
    /// Hook for a source that refreshes at check time.
    fn refresh_for_run(&self) {}
}

/// In-memory source for tests.
pub struct MemoryStaleCacheRootsSource {
    report: Option<CacheScanReport>,
}

impl MemoryStaleCacheRootsSource {
    pub fn new(report: CacheScanReport) -> Self {
        Self {
            report: Some(report),
        }
    }

    pub fn empty() -> Self {
        Self { report: None }
    }
}

impl StaleCacheRootsSource for MemoryStaleCacheRootsSource {
    fn report(&self) -> Option<CacheScanReport> {
        self.report.clone()
    }
}

/// Reports cache roots and tenant namespaces nothing points at. Never deletes.
pub struct StaleCacheRootsCheck {
    source: Arc<dyn StaleCacheRootsSource>,
}

impl StaleCacheRootsCheck {
    pub fn new(source: Arc<dyn StaleCacheRootsSource>) -> Self {
        Self { source }
    }

    /// Turn a report into findings. Pure, so the mapping is testable without
    /// touching a filesystem.
    pub fn findings_for(report: &CacheScanReport) -> Vec<Finding> {
        let mut findings = Vec::new();
        let idle_threshold_seconds = report.idle_threshold_days.saturating_mul(SECONDS_PER_DAY);

        if let ProjectSetStatus::Unavailable { reason } = &report.project_set {
            // Say so loudly: a skipped reconciliation must never read as a
            // clean deployment.
            findings.push(
                Finding::new(
                    FindingSeverity::Info,
                    STALE_CACHE_ROOTS_CHECK_NAME,
                    ResolverSnapshot::new(
                        "resolve_stale_cache_roots",
                        json!({
                            "cache_root": report.cache_root,
                            "project_set": report.project_set,
                        }),
                        json!({
                            "namespace_reconciliation": "skipped",
                            "reason": reason,
                        }),
                    ),
                    format!(
                        "per-project cache namespace reconciliation was skipped ({reason}); \
                         no namespace under {} is claimed orphaned",
                        report.cache_root.display()
                    ),
                )
                .with_entity_id("cache_root", report.cache_root.display().to_string())
                .with_evidence(json!({
                    "cache_root": report.cache_root,
                    "reason": reason,
                    "reconciled_roots": report.reconciled_roots,
                })),
            );
        }

        for observation in &report.observations {
            findings.push(Self::finding_for(
                report,
                observation,
                idle_threshold_seconds,
            ));
        }
        findings
    }

    fn finding_for(
        report: &CacheScanReport,
        observation: &CacheObservation,
        idle_threshold_seconds: u64,
    ) -> Finding {
        let stale = observation
            .age_seconds
            .is_some_and(|age| age >= idle_threshold_seconds);
        let age_days = observation.age_days();
        let size = observation.measurement.size_bytes;
        let at_least = if observation.measurement.size_truncated {
            "at least "
        } else {
            ""
        };
        let uid = observation.measurement.uid;

        let (severity, detail) = match &observation.class {
            CacheObservationClass::UnrecognisedRoot => (
                if stale {
                    FindingSeverity::Warn
                } else {
                    FindingSeverity::Info
                },
                format!(
                    "{} is not a cache root djinn declares ({at_least}{size} bytes, \
                     last modified {} days ago, uid {})",
                    observation.relative_path,
                    render_days(age_days),
                    render_uid(uid),
                ),
            ),
            CacheObservationClass::UnrecognisedEntry => (
                if stale {
                    FindingSeverity::Warn
                } else {
                    FindingSeverity::Info
                },
                format!(
                    "{} is a loose {:?} at the cache root, not a declared cache root \
                     ({at_least}{size} bytes, last modified {} days ago, uid {})",
                    observation.relative_path,
                    observation.kind,
                    render_days(age_days),
                    render_uid(uid),
                ),
            ),
            CacheObservationClass::OrphanedProjectNamespace { root, project_id } => (
                FindingSeverity::Warn,
                format!(
                    // Deliberately does not assert that `project_id` IS a
                    // project — only that it is not one of the live ones. A
                    // stray directory placed under a namespace root reads
                    // correctly under this wording; "project X was deleted"
                    // would not.
                    "{root} holds the namespace {project_id}, which is not in the live project \
                     set ({at_least}{size} bytes, last modified {} days ago, uid {})",
                    render_days(age_days),
                    render_uid(uid),
                ),
            ),
            CacheObservationClass::DeclaredRootIdle { root } => (
                FindingSeverity::Info,
                format!(
                    "{root} is declared in the cache-root manifest but has not been written for \
                     {} days ({at_least}{size} bytes, uid {}); if the subsystem was retired, \
                     remove its manifest entry",
                    render_days(age_days),
                    render_uid(uid),
                ),
            ),
        };

        let inputs = json!({
            "cache_root": report.cache_root,
            "relative_path": observation.relative_path,
            "manifest": CacheRootId::ALL
                .iter()
                .map(|root| root.dir_name())
                .collect::<Vec<_>>(),
            "project_set": report.project_set,
            "idle_threshold_days": report.idle_threshold_days,
        });
        let outputs = serde_json::to_value(observation).unwrap_or(serde_json::Value::Null);
        let mut finding = Finding::new(
            severity,
            STALE_CACHE_ROOTS_CHECK_NAME,
            ResolverSnapshot::new("resolve_stale_cache_roots", inputs, outputs.clone()),
            detail,
        )
        .with_entity_id("cache_root", report.cache_root.display().to_string())
        .with_entity_id("relative_path", observation.relative_path.clone())
        .with_evidence(json!({
            "cache_root": report.cache_root,
            "observation": outputs,
            "scanned_at_unix": report.scanned_at_unix,
            "reconciled_roots": report.reconciled_roots,
        }));
        if let CacheObservationClass::OrphanedProjectNamespace { project_id, .. } =
            &observation.class
        {
            finding = finding.with_entity_id("project_id", project_id.clone());
        }
        finding
    }
}

// ---------------------------------------------------------------------------
// Production source
// ---------------------------------------------------------------------------

/// Page size for the project enumeration. `list_ids_page_after` is the bounded
/// keyset API background jobs are required to use.
const PROJECT_PAGE_SIZE: i64 = 500;

/// Production source: enumerates the live project set through the repository
/// layer, then scans the host cache root.
pub struct ProjectRepositoryStaleCacheRootsSource {
    db: djinn_db::Database,
    config: CacheScanConfig,
    cache: std::sync::RwLock<Option<CacheScanReport>>,
}

impl ProjectRepositoryStaleCacheRootsSource {
    pub fn new(db: djinn_db::Database) -> Self {
        Self {
            db,
            config: CacheScanConfig::default(),
            cache: std::sync::RwLock::new(None),
        }
    }

    /// Enumerate every project id, or fail closed.
    ///
    /// "Every" is load-bearing: this is a shared, multi-tenant board, so the
    /// enumeration must not be scoped to a user, a project, or an activity
    /// window. A namespace is only orphaned relative to a *complete* owner set,
    /// so a partial page walk aborts the whole reconciliation rather than
    /// shrinking the set it compares against.
    async fn enumerate_live_projects(&self) -> LiveProjectSet {
        // Read-only enumeration: a noop bus, so this detector can never emit a
        // domain event as a side effect of looking.
        let repo =
            djinn_db::ProjectRepository::new(self.db.clone(), djinn_core::events::EventBus::noop());
        let mut ids: Vec<String> = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            match repo
                .list_ids_page_after(cursor.as_deref(), PROJECT_PAGE_SIZE)
                .await
            {
                Ok(page) => {
                    let Some(last) = page.last().cloned() else {
                        break;
                    };
                    ids.extend(page);
                    cursor = Some(last);
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "stale_cache_roots: project enumeration failed; \
                         no cache namespace will be claimed orphaned"
                    );
                    return LiveProjectSet::unavailable(format!(
                        "project_enumeration_failed: {error}"
                    ));
                }
            }
        }
        // An empty result is indistinguishable from a mis-scoped query, so
        // `from_enumeration` downgrades it to Unavailable.
        LiveProjectSet::from_enumeration(ids)
    }

    /// Refresh the cached report. Errors are absorbed into the report's own
    /// fail-closed state rather than surfaced, so a check run always has an
    /// interpretable answer.
    pub async fn refresh(&self) {
        let live = self.enumerate_live_projects().await;
        let config = self.config;
        let now: u64 = time::OffsetDateTime::now_utc()
            .unix_timestamp()
            .try_into()
            .unwrap_or(0);
        let report = match tokio::task::spawn_blocking(move || {
            scan_production_cache_root(&live, now, config)
        })
        .await
        {
            Ok(report) => report,
            Err(error) => {
                tracing::warn!(%error, "stale_cache_roots: cache scan task failed");
                return;
            }
        };
        if let Ok(mut guard) = self.cache.write() {
            *guard = Some(report);
        }
    }

    /// Bridge the async refresh into the synchronous `DoctorCheck::run` path.
    /// Mirrors `refinement_phantom_active`'s established shape.
    fn refresh_blocking(&self) {
        match tokio::runtime::Handle::try_current() {
            Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(|| handle.block_on(self.refresh()));
            }
            Ok(_) => {
                std::thread::scope(|scope| {
                    scope.spawn(|| match tokio::runtime::Runtime::new() {
                        Ok(runtime) => runtime.block_on(self.refresh()),
                        Err(error) => tracing::warn!(
                            %error,
                            "stale_cache_roots: failed to create refresh runtime"
                        ),
                    });
                });
            }
            Err(_) => match tokio::runtime::Runtime::new() {
                Ok(runtime) => runtime.block_on(self.refresh()),
                Err(error) => {
                    tracing::warn!(%error, "stale_cache_roots: failed to create refresh runtime")
                }
            },
        }
    }
}

impl StaleCacheRootsSource for ProjectRepositoryStaleCacheRootsSource {
    fn report(&self) -> Option<CacheScanReport> {
        self.cache.read().ok().and_then(|guard| guard.clone())
    }

    fn refresh_for_run(&self) {
        self.refresh_blocking();
    }
}

fn render_days(age_days: Option<u64>) -> String {
    age_days.map_or_else(|| "unknown".to_owned(), |days| days.to_string())
}

fn render_uid(uid: Option<u32>) -> String {
    uid.map_or_else(|| "unknown".to_owned(), |uid| uid.to_string())
}

impl DoctorCheck for StaleCacheRootsCheck {
    fn name(&self) -> &'static str {
        STALE_CACHE_ROOTS_CHECK_NAME
    }

    fn description(&self) -> &'static str {
        "Reports cache roots absent from the djinn_core::paths manifest and per-project cache \
         namespaces whose project no longer exists; read-only, never deletes"
    }

    /// On demand only. The scan stats the filesystem and, for candidates,
    /// walks them — too expensive for the coordinator's cheap periodic subset.
    fn cadence(&self) -> DoctorCheckCadence {
        DoctorCheckCadence::OnDemand
    }

    fn run(&self) -> DoctorResult<Vec<Finding>> {
        self.source.refresh_for_run();
        let Some(report) = self.source.report() else {
            // No completed scan is not the same as nothing to report.
            return Ok(vec![
                Finding::new(
                    FindingSeverity::Info,
                    STALE_CACHE_ROOTS_CHECK_NAME,
                    ResolverSnapshot::new(
                        "resolve_stale_cache_roots",
                        json!({ "scan": "unavailable" }),
                        json!({ "reported": false }),
                    ),
                    "cache-root scan has not completed; no cache-root claim is made",
                )
                .with_evidence(json!({ "scan": "unavailable" })),
            ]);
        };
        Ok(Self::findings_for(&report))
    }
}

#[cfg(test)]
mod tests;
