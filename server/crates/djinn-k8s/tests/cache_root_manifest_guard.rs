//! Drift guard: no renderer may bake a cache root the manifest does not know.
//!
//! `djinn_core::paths::CacheRootId` is a closed manifest, and the coordinator's
//! `cache.stale_roots` doctor check reports anything under the cache PVC that
//! is not in it. That check is only as good as the manifest's completeness, and
//! the manifest is only complete if it cannot be bypassed.
//!
//! Two mechanisms keep it complete:
//!
//! 1. **Structural.** The manifest, the `CacheRootId` enum and its `ALL` slice
//!    all come out of one macro expansion, so a variant cannot exist without a
//!    description. Renderers call `CacheRootId::_::job_pod_path()`, so the
//!    obvious way to add a root goes through the manifest.
//! 2. **This test.** A renderer could still bypass (1) by interpolating a path
//!    string. So every `/cache/<segment>` literal in the files that render
//!    Pod specs and image ENV is extracted and checked against the manifest.
//!
//! Limits, stated plainly: this catches roots djinn *renders*. It cannot catch
//! a root created by a tool defaulting into `/cache`, by an ad-hoc command run
//! with the PVC mounted, or by a future renderer in a file not listed here —
//! those are exactly what the runtime doctor check is for. Nor does it force
//! *retirement*: removing a subsystem still requires deleting its manifest
//! entry by hand. The `declared_root_idle` finding is the runtime nudge for
//! that, and it is why the manifest carries a `declared_by` field naming the
//! code that would have to change.

use djinn_core::paths::{CacheRootId, JOB_POD_CACHE_MOUNT};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Files that bake a cache path into a Pod spec, a container ENV, or a
/// filesystem convention shared with Job pods.
const RENDERER_SOURCES: &[&str] = &[
    "crates/djinn-k8s/src/job.rs",
    "crates/djinn-k8s/src/warm_job.rs",
    "crates/djinn-image-builder/src/dockerfile.rs",
    "crates/djinn-agent-worker/src/cargo_target_seed.rs",
    "crates/djinn-supervisor/src/lib.rs",
];

/// `server/` — `CARGO_MANIFEST_DIR` is `server/crates/djinn-k8s`.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/<crate> has a workspace root")
        .to_path_buf()
}

/// Pull the first path segment out of every `/cache/<segment>` occurrence.
///
/// Interpolated segments (`{project_id}`, `{}`) are skipped: they are values
/// *inside* a root, not root names. A `/cache` preceded by a path or word
/// character is skipped too — prose like "workspace/cache/mirror volumes" is
/// not a rendered path.
fn cache_segments(source: &str) -> BTreeSet<String> {
    let needle = format!("{JOB_POD_CACHE_MOUNT}/");
    let mut found = BTreeSet::new();
    let mut rest = source;
    let mut consumed = 0usize;
    while let Some(index) = rest.find(&needle) {
        let absolute = consumed + index;
        let preceded_by_path_char = source[..absolute]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '/');
        if preceded_by_path_char {
            consumed = absolute + needle.len();
            rest = &source[consumed..];
            continue;
        }
        let after = &rest[index + needle.len()..];
        let end = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'))
            .unwrap_or(after.len());
        let segment = &after[..end];
        if !segment.is_empty() {
            found.insert(segment.to_owned());
        }
        consumed = absolute + needle.len();
        rest = &source[consumed..];
    }
    found
}

#[test]
fn every_rendered_cache_root_is_in_the_manifest() {
    let declared: BTreeSet<&str> = CacheRootId::ALL
        .iter()
        .copied()
        .map(CacheRootId::dir_name)
        .collect();
    let root = workspace_root();
    let mut checked_files = 0usize;
    let mut undeclared: Vec<String> = Vec::new();

    for relative in RENDERER_SOURCES {
        let path = root.join(relative);
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        checked_files += 1;
        for segment in cache_segments(&source) {
            if !declared.contains(segment.as_str()) {
                undeclared.push(format!("{relative}: /cache/{segment}"));
            }
        }
    }

    assert_eq!(
        checked_files,
        RENDERER_SOURCES.len(),
        "every renderer source must be readable; a moved file silently disables this guard"
    );
    assert!(
        undeclared.is_empty(),
        "these renderers bake cache roots that djinn_core::paths::CacheRootId does not declare, \
         so the cache.stale_roots doctor check would report them as unrecognised (or, worse, \
         never notice when they are retired). Add a manifest entry:\n{}",
        undeclared.join("\n")
    );
}

/// The guard is only meaningful if it actually looks at something.
#[test]
fn guard_extracts_the_roots_it_claims_to() {
    let segments = cache_segments(
        r#"format!("{CACHE_MOUNT_DIR}/xdg/{project_id}") "/cache/cargo-target-runs" "/cache/" "#,
    );
    assert!(segments.contains("cargo-target-runs"));
    assert!(!segments.contains("{project_id}"));
    assert!(
        cache_segments("workspace/cache/mirror volumes are group-owned").is_empty(),
        "prose containing a `/cache/` substring is not a rendered path"
    );

    // A hypothetical undeclared root must be detected, otherwise the guard
    // above would pass vacuously.
    let declared: BTreeSet<&str> = CacheRootId::ALL
        .iter()
        .copied()
        .map(CacheRootId::dir_name)
        .collect();
    let invented = cache_segments(r#""/cache/some-retired-subsystem/x""#);
    assert!(
        invented
            .iter()
            .any(|segment| !declared.contains(segment.as_str())),
        "the guard must flag a root that is not in the manifest"
    );
}

/// The k8s crate's own mount constant must be the manifest's, not a second
/// literal that could drift away from it.
#[test]
fn job_pod_cache_mount_is_single_sourced() {
    assert_eq!(djinn_k8s::job::CACHE_MOUNT_DIR, JOB_POD_CACHE_MOUNT);
    for root in CacheRootId::ALL.iter().copied() {
        assert!(root.job_pod_path().starts_with(JOB_POD_CACHE_MOUNT));
    }
}
