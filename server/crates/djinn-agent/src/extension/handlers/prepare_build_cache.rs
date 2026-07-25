//! Agent-facing `prepare_build_cache` worker-tool handler (epic 1bnj).
//!
//! Stack-neutral: this handler resolves the platform warm-cache descriptor for
//! the current stack, routes it through the pure admission classifier in
//! [`djinn_core::prepare_build_cache`], and returns exactly one typed outcome
//! plus one bounded telemetry increment. It never intercepts or blocks shell
//! commands and never encodes language-specific behaviour in the tool schema —
//! the Rust cargo target run-dir is resolved here from the pod environment, not
//! from any tool argument.
//!
//! While the worker still eagerly seeds the cargo target run-dir at startup
//! (nquz first-use / enforce is not yet live), this returns a compatible ready
//! `noop` rather than duplicating the seed work, preserving the seed
//! single-flight and disk-reservation invariants owned by the startup path.

use djinn_core::prepare_build_cache::{
    DiskAdmission, PlatformCache, PrepareBuildCacheOutcome, classify_prepare_build_cache,
};

/// Environment variable Cargo uses for the per-run private target directory.
const CARGO_TARGET_DIR_ENV: &str = "CARGO_TARGET_DIR";

/// Opt-in switch that flips the tool off its eager-seed compatibility no-op and
/// onto the disk-admission seam. Absent / not `"1"` means eager startup seeding
/// is still authoritative (the state on `main` today), so the tool must be a
/// ready no-op. Wired ahead of the nquz first-use client so the cutover is a
/// single env flip.
const NQUZ_FIRST_USE_ENFORCE_ENV: &str = "DJINN_NQUZ_FIRST_USE_ENFORCE";

/// Resolve the platform warm-cache descriptor for the current stack from the
/// pod environment. Today only the Rust cargo target run-dir exists; every
/// other stack resolves to [`PlatformCache::None`] (`not_applicable`).
fn resolve_platform_cache() -> PlatformCache {
    let Some(target_dir) = std::env::var(CARGO_TARGET_DIR_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
    else {
        return PlatformCache::None;
    };

    // A platform warm cache applies only when Cargo's target dir is a private
    // per-task-run directory under the conventional runs root. A project that
    // points CARGO_TARGET_DIR elsewhere (or has no warm base) is not applicable.
    let runs_root = djinn_core::paths::cargo_target_runs_root();
    if std::path::Path::new(&target_dir).starts_with(&runs_root) {
        PlatformCache::CargoTargetRunDir {
            cache_path: target_dir,
        }
    } else {
        PlatformCache::None
    }
}

/// `true` while eager startup seeding remains authoritative (nquz enforce off).
fn eager_seed_active() -> bool {
    std::env::var(NQUZ_FIRST_USE_ENFORCE_ENV).ok().as_deref() != Some("1")
}

/// Handle a `prepare_build_cache` tool call.
///
/// Returns exactly one typed outcome and emits exactly one bounded telemetry
/// increment. Identifiers are placed only in the structured `audit` object,
/// never in telemetry labels.
pub(crate) async fn call_prepare_build_cache(
    session_task_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let cache = resolve_platform_cache();
    let eager = eager_seed_active();
    // The disk-admission signal is consulted only once eager seeding is retired.
    // Until the nquz first-use client is wired, fail closed to queued (no
    // allocation) rather than allocating run-dir bytes speculatively.
    let disk = DiskAdmission::CapacityUnknown;
    Ok(build_result(&cache, eager, disk, session_task_id))
}

/// Pure result shaper: classify, emit exactly one telemetry increment, and
/// build the typed tool-result payload. Split out from environment resolution
/// so it can be unit-tested with explicit inputs.
fn build_result(
    cache: &PlatformCache,
    eager_seed_active: bool,
    disk: DiskAdmission,
    session_task_id: Option<&str>,
) -> serde_json::Value {
    let outcome = classify_prepare_build_cache(cache, eager_seed_active, disk);

    djinn_telemetry::prepare_build_cache::increment_outcome(telemetry_label(&outcome));

    let stack = match cache {
        PlatformCache::CargoTargetRunDir { .. } => "rust",
        PlatformCache::None => "none",
    };

    let mut audit = serde_json::Map::new();
    if let Some(task_id) = session_task_id {
        audit.insert(
            "task_id".to_string(),
            serde_json::Value::String(task_id.to_string()),
        );
    }

    let mut payload = serde_json::Map::new();
    payload.insert(
        "outcome".to_string(),
        serde_json::Value::String(outcome.outcome_field().to_string()),
    );
    payload.insert(
        "stack".to_string(),
        serde_json::Value::String(stack.to_string()),
    );

    let detail = match &outcome {
        PrepareBuildCacheOutcome::Ready { cache_path } => {
            payload.insert(
                "cache_path".to_string(),
                serde_json::Value::String(cache_path.clone()),
            );
            audit.insert(
                "cache_path".to_string(),
                serde_json::Value::String(cache_path.clone()),
            );
            "Platform build cache is prepared and ready. Build normally; do not override the pre-configured cache paths.".to_string()
        }
        PrepareBuildCacheOutcome::Noop { cache_path } => {
            payload.insert(
                "cache_path".to_string(),
                serde_json::Value::String(cache_path.clone()),
            );
            audit.insert(
                "cache_path".to_string(),
                serde_json::Value::String(cache_path.clone()),
            );
            "Platform build cache was already seeded for this run; nothing to do. Build normally against the pre-configured cache paths.".to_string()
        }
        PrepareBuildCacheOutcome::Queued { reason } => {
            payload.insert(
                "queued_reason".to_string(),
                serde_json::Value::String(reason.as_str().to_string()),
            );
            format!(
                "Cache preparation was deferred ({}) without allocating cache bytes; build normally and it will be prepared when capacity allows.",
                reason.as_str()
            )
        }
        PrepareBuildCacheOutcome::NotApplicable => {
            "This stack has no platform warm cache; nothing to prepare. Build normally.".to_string()
        }
    };
    payload.insert("detail".to_string(), serde_json::Value::String(detail));
    payload.insert("audit".to_string(), serde_json::Value::Object(audit));

    serde_json::Value::Object(payload)
}

/// Map a typed outcome onto its bounded telemetry constant.
fn telemetry_label(outcome: &PrepareBuildCacheOutcome) -> &'static str {
    use djinn_telemetry::prepare_build_cache as tel;
    match outcome {
        PrepareBuildCacheOutcome::Ready { .. } => tel::OUTCOME_READY,
        PrepareBuildCacheOutcome::Noop { .. } => tel::OUTCOME_NOOP,
        PrepareBuildCacheOutcome::Queued { .. } => tel::OUTCOME_QUEUED,
        PrepareBuildCacheOutcome::NotApplicable => tel::OUTCOME_NOT_APPLICABLE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cargo_cache() -> PlatformCache {
        PlatformCache::CargoTargetRunDir {
            cache_path: "/cache/cargo-target-runs/task-run-xyz".to_string(),
        }
    }

    #[test]
    fn no_platform_cache_is_not_applicable_and_hides_cache_path() {
        let value = build_result(
            &PlatformCache::None,
            true,
            DiskAdmission::Ok,
            Some("task-abc"),
        );
        assert_eq!(value["outcome"], "not_applicable");
        assert_eq!(value["stack"], "none");
        // Identifiers ride the audit object only, never the top-level payload.
        assert_eq!(value["audit"]["task_id"], "task-abc");
        assert!(value.get("cache_path").is_none());
        assert!(value["audit"].get("cache_path").is_none());
    }

    #[test]
    fn eager_active_returns_ready_noop_with_cache_path_contract() {
        let value = build_result(
            &cargo_cache(),
            true,
            DiskAdmission::CapacityUnknown,
            Some("t1"),
        );
        assert_eq!(value["outcome"], "noop");
        assert_eq!(value["stack"], "rust");
        assert_eq!(value["cache_path"], "/cache/cargo-target-runs/task-run-xyz");
        assert_eq!(
            value["audit"]["cache_path"],
            "/cache/cargo-target-runs/task-run-xyz"
        );
    }

    #[test]
    fn eager_retired_disk_pressure_queues_without_cache_path() {
        let value = build_result(&cargo_cache(), false, DiskAdmission::Pressure, None);
        assert_eq!(value["outcome"], "queued");
        assert_eq!(value["queued_reason"], "disk_pressure");
        // A queued outcome allocates nothing and returns no ready cache-path.
        assert!(value.get("cache_path").is_none());
    }

    #[test]
    fn eager_retired_ok_disk_is_ready() {
        let value = build_result(&cargo_cache(), false, DiskAdmission::Ok, None);
        assert_eq!(value["outcome"], "ready");
        assert_eq!(value["cache_path"], "/cache/cargo-target-runs/task-run-xyz");
    }
}
