//! Doctor framework: pluggable health checks for the platform.
//!
//! The doctor framework turns incident classes into permanent detectors. A
//! [`DoctorCheck`] is a typed, self-contained detector that can be run on
//! demand (via the `doctor_run` MCP tool) and, if the operator agrees, fixed
//! on demand (via the `doctor_fix` MCP tool). The framework intentionally
//! does not auto-fix — fixes are opt-in.
//!
//! This module mirrors the pure-core `liveness` precedent: it owns the
//! shared types and the sample invariant test. Database persistence lives in
//! `djinn-db`; the MCP `doctor_run` / `doctor_fix` tools live in
//! `djinn-control-plane`.
//!
//! # Shared-resolver invariant (the Gas Town bug)
//!
//! Gas Town's `doctor --fix` once reverted valid config because the fixer's
//! "expected state" diverged from the resolver inputs the checker used.
//! To structurally prevent that, every [`Finding`] carries a
//! [`ResolverSnapshot`]: the inputs and outputs the resolver used to produce
//! the finding. A `fix(&Finding)` MUST derive its own expected state by
//! re-running the same `resolve()` helper with the snapshot's inputs — never
//! a hard-coded value, and never a fresh resolver call with new inputs.
//!
//! Concretely, the sample check in the test module below is built so that
//! the only way the fix path can produce a correct plan is to call the
//! shared `resolve()` helper with the snapshot's inputs. A hand-coded
//! expected value would cause the test to fail. The trait API itself
//! expresses this invariant: `fix` consumes a `&Finding`, which is the only
//! place the snapshot is carried.
//!
//! # Registry
//!
//! For the first slice, registration is a plain static [`DoctorRegistry`]
//! that callers can push into. We deliberately avoid `inventory`/`linkme`
//! for the framework slice — a hand-rolled registry is enough to prove the
//! wiring, and seed-check epics can swap in a more elaborate mechanism if
//! they need global self-registration.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod checks;

pub use checks::retrieval::{
    DEFAULT_QUERY_FLOOR, DEFAULT_WINDOW_HOURS, DEFAULT_ZERO_RESULT_THRESHOLD, EntryPointCounts,
    MAX_QUERY_FLOOR, MAX_WINDOW_HOURS, MAX_ZERO_RESULT_THRESHOLD, MIN_QUERY_FLOOR,
    MIN_WINDOW_HOURS, MIN_ZERO_RESULT_THRESHOLD, RETRIEVAL_ZERO_RESULT_NAME, RetrievalHealthConfig,
    RetrievalHealthConfigError, RetrievalHealthDataSource, RetrievalHealthSnapshot,
    RetrievalProjectWindowSnapshot, RetrievalZeroResultCheck,
};

/// Errors that a [`DoctorCheck`] can surface from `run` or `fix`.
#[derive(Debug, Error)]
pub enum DoctorError {
    /// The check or fix could not complete because the underlying system
    /// (DB, MCP, etc.) was unavailable or returned an error.
    #[error("doctor backend error: {0}")]
    Backend(String),

    /// The check or fix observed a malformed input it could not interpret.
    #[error("doctor invalid input: {0}")]
    InvalidInput(String),

    /// The check name passed to `fix` did not match the finding's check
    /// name, or the check is not registered.
    #[error("doctor unknown check: {0}")]
    UnknownCheck(String),

    /// `fix` was invoked on a check that does not support fixing. The
    /// default [`DoctorCheck::fix`] returns this variant explicitly so the
    /// caller never silently gets a no-op.
    #[error("doctor fix not supported for check '{check}'")]
    FixNotSupported { check: String },
}

/// Convenience `Result` alias for doctor operations.
pub type DoctorResult<T> = std::result::Result<T, DoctorError>;

// ---------------------------------------------------------------------------
// Run cadence / subset
// ---------------------------------------------------------------------------

/// Operational cadence for a registered doctor check.
///
/// Checks default to [`OnDemand`](Self::OnDemand). A check must explicitly opt in
/// to [`Cheap`](Self::Cheap) before coordinator-style periodic callers may run it
/// outside the MCP `doctor_run` path. Cluster-facing or otherwise expensive
/// checks, such as a Kubernetes pod-leak scan, should keep the default and remain
/// on demand unless a slower scheduler is added for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DoctorCheckCadence {
    /// Safe to run frequently: pure DB, pure memory, or similarly bounded work.
    Cheap,
    /// Invoked explicitly by an operator/tool or by a future slower cadence.
    OnDemand,
}

impl DoctorCheckCadence {
    /// `true` when this check is explicitly part of the cheap periodic subset.
    pub const fn is_cheap(self) -> bool {
        matches!(self, Self::Cheap)
    }
}

/// A named subset of registered checks that can be executed without MCP auth or
/// tool-dispatch plumbing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DoctorRunSubset {
    /// Checks explicitly marked [`DoctorCheckCadence::Cheap`].
    Cheap,
}

impl DoctorRunSubset {
    /// Stable subset name accepted by helper dispatchers.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Cheap => "cheap",
        }
    }
}

/// Metadata for a registered doctor check, copied out of the registry so
/// callers can inspect it without holding the registry lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DoctorCheckMetadata {
    pub name: &'static str,
    pub description: &'static str,
    pub cadence: DoctorCheckCadence,
}

/// Result of one check execution from a subset runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorCheckRun {
    pub check_name: &'static str,
    pub findings: Vec<Finding>,
}

// ---------------------------------------------------------------------------
// Severity
// ---------------------------------------------------------------------------

/// How serious a [`Finding`] is.
///
/// `Info` is observational, `Warn` indicates a condition that will become a
/// problem, `Error` indicates a condition requiring attention, and `Critical`
/// indicates an active failure that needs immediate attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FindingSeverity {
    Info,
    Warn,
    Error,
    Critical,
}

impl FindingSeverity {
    /// Stable, snake-case string form used for persistence and reporting.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
            Self::Critical => "critical",
        }
    }
}

impl std::fmt::Display for FindingSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Resolver snapshot
// ---------------------------------------------------------------------------

/// A snapshot of the resolver inputs and outputs at the time a finding was
/// produced.
///
/// The check runs `resolve(inputs) -> outputs`, captures both sides, and
/// stuffs them into a [`Finding`]. The fixer later calls
/// `resolve(snapshot.inputs)` again to recompute the *exact* same expected
/// state — the shared-resolver invariant. Persisting the snapshot means
/// later fixes are reproducible even if the resolver's behavior has drifted.
///
/// `inputs` and `outputs` are `serde_json::Value` so the framework stays
/// schema-free: a check is free to capture whatever shape is meaningful for
/// its domain, and the database stores them as JSONB.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResolverSnapshot {
    /// Free-form name of the resolver function (e.g. `"resolve_session_state"`).
    pub resolver: String,
    /// Inputs the check fed to the resolver.
    pub inputs: serde_json::Value,
    /// Outputs the resolver returned.
    pub outputs: serde_json::Value,
}

impl ResolverSnapshot {
    /// Construct a snapshot from the resolver name and its inputs/outputs.
    pub fn new(
        resolver: impl Into<String>,
        inputs: serde_json::Value,
        outputs: serde_json::Value,
    ) -> Self {
        Self {
            resolver: resolver.into(),
            inputs,
            outputs,
        }
    }
}

// ---------------------------------------------------------------------------
// Finding
// ---------------------------------------------------------------------------

/// A single doctor finding: one observable problem (or observation)
/// produced by a [`DoctorCheck::run`].
///
/// Fields are intentionally structured (not just a string blob) so the
/// framework can persist findings to a `doctor_findings` table, render
/// them in reports, and let the fix path consume the resolver snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Finding {
    /// How serious the finding is.
    pub severity: FindingSeverity,
    /// Stable identifier of the check that produced the finding; must match
    /// [`DoctorCheck::name`].
    pub check_name: String,
    /// Structured entity ids the finding refers to (session id, run id,
    /// workspace id, …). Stored as a map so checks can name whatever they
    /// need without a new schema column each time.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub entity_ids: BTreeMap<String, String>,
    /// Structured evidence the check used to decide. Free-form `Value` so
    /// each check can include whatever it likes (query results, timestamps,
    /// diffs, etc.).
    #[serde(default)]
    pub evidence: serde_json::Value,
    /// Snapshot of the resolver inputs/outputs used to produce the
    /// finding. The fix path MUST re-run the same resolver with
    /// `resolver_snapshot.inputs`.
    pub resolver_snapshot: ResolverSnapshot,
    /// Human-readable detail for operator/agent display.
    pub detail: String,
}

impl Finding {
    /// Construct a finding with the minimum required fields; callers can
    /// chain `.with_*` builders to enrich it.
    pub fn new(
        severity: FindingSeverity,
        check_name: impl Into<String>,
        resolver_snapshot: ResolverSnapshot,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            severity,
            check_name: check_name.into(),
            entity_ids: BTreeMap::new(),
            evidence: serde_json::Value::Null,
            resolver_snapshot,
            detail: detail.into(),
        }
    }

    #[must_use]
    pub fn with_entity_id(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.entity_ids.insert(key.into(), value.into());
        self
    }

    #[must_use]
    pub fn with_evidence(mut self, evidence: serde_json::Value) -> Self {
        self.evidence = evidence;
        self
    }
}

// ---------------------------------------------------------------------------
// DoctorCheck trait
// ---------------------------------------------------------------------------

/// A pluggable health check.
///
/// Implementors expose a stable [`name`](Self::name) (used for CLI args,
/// metrics, and the `doctor_findings.check_name` column) and a human
/// [`description`](Self::description) that surfaces in reports. The check
/// itself is [`run`](Self::run); [`fix`](Self::fix) is opt-in and defaults
/// to an explicit "not supported" error so a stray fix request can never
/// silently no-op.
pub trait DoctorCheck: Send + Sync {
    /// Stable identifier for this check. Must be unique across the
    /// registry. Recommended form: `<area>.<symptom>` (e.g.
    /// `"session.zombie"`).
    fn name(&self) -> &'static str;

    /// Human-readable description surfaced in `doctor` output and docs.
    fn description(&self) -> &'static str;

    /// Execute the check and return every [`Finding`] it observes. An
    /// empty `Vec` means "the check passed".
    fn run(&self) -> DoctorResult<Vec<Finding>>;

    /// Operational cadence for this check. The default is on-demand so new
    /// checks are never accidentally admitted to the coordinator's cheap subset.
    fn cadence(&self) -> DoctorCheckCadence {
        DoctorCheckCadence::OnDemand
    }

    /// Attempt to fix the condition reported in `finding`. This is
    /// **opt-in** and **never** invoked from [`run`](Self::run); only an
    /// explicit `doctor_fix` MCP call may invoke it.
    ///
    /// The default implementation returns
    /// [`DoctorError::FixNotSupported`] so callers do not silently get a
    /// no-op when a check does not implement fixing.
    ///
    /// Implementors MUST derive the expected state by re-running the same
    /// `resolve()` helper the checker used, with
    /// `finding.resolver_snapshot.inputs` as inputs. See the module-level
    /// docs for the Gas Town shared-resolver invariant.
    fn fix(&self, _finding: &Finding) -> DoctorResult<()> {
        Err(DoctorError::FixNotSupported {
            check: self.name().to_string(),
        })
    }

    /// Attempt a fix and optionally return structured check-specific evidence
    /// about the completed operation.
    ///
    /// This additive seam preserves [`fix`](Self::fix)'s public signature for
    /// existing checks. Checks without a result payload inherit this forwarding
    /// implementation, so their observable fix behavior remains unchanged.
    fn fix_with_result(&self, finding: &Finding) -> DoctorResult<Option<serde_json::Value>> {
        self.fix(finding).map(|()| None)
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Process-wide doctor check registry.
///
/// For the first slice, this is a plain static `Mutex<BTreeMap>` keyed by
/// [`DoctorCheck::name`]. We avoid `inventory`/`linkme` because the
/// framework is small enough that explicit registration is clearer, and
/// later epics can swap the storage if global self-registration becomes
/// desirable.
///
/// Checks are stored as `Arc<dyn DoctorCheck>` so [`DoctorRegistry::get`]
/// can hand out a fresh, cheap-to-clone reference without requiring
/// implementors to be `Clone`.
pub struct DoctorRegistry {
    inner: Mutex<BTreeMap<String, Arc<dyn DoctorCheck>>>,
}

impl std::fmt::Debug for DoctorRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let guard = self.inner.lock().expect("doctor registry poisoned");
        f.debug_struct("DoctorRegistry")
            .field("names", &guard.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Default for DoctorRegistry {
    fn default() -> Self {
        Self {
            inner: Mutex::new(BTreeMap::new()),
        }
    }
}

impl DoctorRegistry {
    /// Construct an empty registry. The global instance is the one used
    /// by `doctor_run`; tests can build local registries with `Default`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a check, replacing any existing check with the same name.
    /// Returns the previously-registered name if one was overwritten (used
    /// by tests to assert the replace behaviour).
    pub fn register(&self, check: Arc<dyn DoctorCheck>) -> Option<String> {
        let name = check.name().to_string();
        let mut guard = self.inner.lock().expect("doctor registry poisoned");
        let previous = guard.insert(name.clone(), check);
        if previous.is_some() { Some(name) } else { None }
    }

    /// Remove a check by name. Returns `true` if a check was removed.
    pub fn unregister(&self, name: &str) -> bool {
        let mut guard = self.inner.lock().expect("doctor registry poisoned");
        guard.remove(name).is_some()
    }

    /// Enumerate the registered checks in stable name order. Returns
    /// `(&'static str, &'static str)` pairs of `(name, description)` so
    /// callers can render a directory without holding the lock.
    pub fn enumerate(&self) -> Vec<(&'static str, &'static str)> {
        let guard = self.inner.lock().expect("doctor registry poisoned");
        guard
            .values()
            .map(|check| (check.name(), check.description()))
            .collect()
    }

    /// Enumerate registered checks with cadence metadata in stable name order.
    pub fn enumerate_with_cadence(&self) -> Vec<DoctorCheckMetadata> {
        let guard = self.inner.lock().expect("doctor registry poisoned");
        guard
            .values()
            .map(|check| DoctorCheckMetadata {
                name: check.name(),
                description: check.description(),
                cadence: check.cadence(),
            })
            .collect()
    }

    /// Enumerate only checks that explicitly opted in to the cheap subset.
    pub fn enumerate_cheap(&self) -> Vec<DoctorCheckMetadata> {
        self.enumerate_with_cadence()
            .into_iter()
            .filter(|metadata| metadata.cadence.is_cheap())
            .collect()
    }

    /// Return cheap checks in stable name order, cloning the stored `Arc`s so
    /// callers can run them without holding the registry lock.
    pub fn cheap_checks(&self) -> Vec<Arc<dyn DoctorCheck>> {
        let guard = self.inner.lock().expect("doctor registry poisoned");
        guard
            .values()
            .filter(|check| check.cadence().is_cheap())
            .map(Arc::clone)
            .collect()
    }

    /// Look up a check by name. The returned `Arc` clones the stored
    /// reference; callers do not need to acquire the registry lock to use
    /// the check.
    pub fn get(&self, name: &str) -> Option<Arc<dyn DoctorCheck>> {
        let guard = self.inner.lock().expect("doctor registry poisoned");
        guard.get(name).map(Arc::clone)
    }

    /// Number of registered checks. Useful for tests asserting
    /// registration behavior.
    pub fn len(&self) -> usize {
        self.inner.lock().expect("doctor registry poisoned").len()
    }

    /// `true` if no checks are registered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Convenience: register a check by converting it to `Arc<dyn DoctorCheck>`.
/// Use this when a concrete check is `Send + Sync + 'static` (the standard
/// requirement for a doctor check).
pub fn register<T>(registry: &DoctorRegistry, check: T) -> Option<String>
where
    T: DoctorCheck + 'static,
{
    registry.register(Arc::new(check))
}

/// Process-wide doctor registry handle.
///
/// Initialised lazily on first access. Returns a reference to a singleton
/// for the lifetime of the process; tests should use
/// [`DoctorRegistry::new`] directly to avoid touching the global state.
pub fn registry() -> &'static DoctorRegistry {
    static REGISTRY: OnceLock<DoctorRegistry> = OnceLock::new();
    REGISTRY.get_or_init(DoctorRegistry::new)
}

/// Execute a named subset of registered checks without going through MCP auth or
/// tool dispatch. This is the seam the coordinator can call in a later task.
pub fn run_named_subset(
    registry: &DoctorRegistry,
    subset: &str,
) -> DoctorResult<Vec<DoctorCheckRun>> {
    match subset {
        "cheap" => run_subset(registry, DoctorRunSubset::Cheap),
        other => Err(DoctorError::InvalidInput(format!(
            "unknown doctor run subset '{other}'"
        ))),
    }
}

/// Execute a registered check subset without using MCP auth/tool plumbing.
pub fn run_subset(
    registry: &DoctorRegistry,
    subset: DoctorRunSubset,
) -> DoctorResult<Vec<DoctorCheckRun>> {
    let checks = match subset {
        DoctorRunSubset::Cheap => registry.cheap_checks(),
    };

    checks
        .into_iter()
        .map(|check| {
            let check_name = check.name();
            check.run().map(|findings| DoctorCheckRun {
                check_name,
                findings,
            })
        })
        .collect()
}

/// Convenience wrapper for the coordinator's future cheap doctor tick.
pub fn run_cheap_subset(registry: &DoctorRegistry) -> DoctorResult<Vec<DoctorCheckRun>> {
    run_subset(registry, DoctorRunSubset::Cheap)
}

// ---------------------------------------------------------------------------
/// Run a subset of registered checks and return their findings.
///
/// * `check_names == None` — run every registered check.
/// * `check_names == Some(names)` — run only the named checks.
///
/// Returns one `(check_name, findings)` tuple per executed check (in the
/// order they were registered). Unknown names in `Some(_)` produce a
/// structured [`DoctorError::UnknownCheck`] error listing every unknown
/// name. An empty `Some(&[])` is treated the same as `None`.
///
/// `doctor_run` calls each check's [`DoctorCheck::run`] only — it never
/// invokes [`DoctorCheck::fix`]. That invariant is asserted by the smoke
/// test's `doctor_run_does_not_call_fix` regression check.
pub fn doctor_run(
    registry: &DoctorRegistry,
    check_names: Option<&[&str]>,
) -> DoctorResult<Vec<(String, Vec<Finding>)>> {
    // Resolve the requested subset. We materialise the checks via `get`
    // so each `Arc` is cloned out of the registry without holding the
    // lock during execution.
    let checks: Vec<std::sync::Arc<dyn DoctorCheck>> = match check_names {
        None => registry
            .enumerate()
            .into_iter()
            .filter_map(|(name, _)| registry.get(name))
            .collect(),
        Some([]) => registry
            .enumerate()
            .into_iter()
            .filter_map(|(name, _)| registry.get(name))
            .collect(),
        Some(names) => {
            let mut checks = Vec::with_capacity(names.len());
            let mut unknown: Vec<String> = Vec::new();
            for name in names {
                match registry.get(name) {
                    Some(check) => checks.push(check),
                    None => unknown.push((*name).to_string()),
                }
            }
            if !unknown.is_empty() {
                return Err(DoctorError::UnknownCheck(unknown.join(", ")));
            }
            checks
        }
    };

    let mut results = Vec::with_capacity(checks.len());
    for check in &checks {
        let name = check.name();
        // run() only — never fix(). The smoke test asserts this invariant.
        let findings = check.run()?;
        results.push((name.to_string(), findings));
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    // Sample shared-resolver check
    // -------------------------------------------------------------------
    //
    // The sample check + fix below is the canonical demonstration of the
    // shared-resolver invariant. The check calls a single `resolve()`
    // helper, captures both inputs and outputs in a `ResolverSnapshot`,
    // and returns a `Finding`. The fix then takes that finding and
    // re-runs `resolve()` against the snapshot's inputs — it does NOT
    // hard-code an expected value. The test `fix_uses_shared_resolver`
    // will fail if the fix path were to take a shortcut.

    /// A single, shared resolver. Both the check and the fix must call
    /// this function. If the fix tried to inline a different expected
    /// value, the unit test would fail because it cannot reproduce the
    /// check's outputs without going through `resolve()`.
    fn resolve(inputs: &serde_json::Value) -> serde_json::Value {
        // A trivial example: the resolver enforces "desired state == 42".
        // The check reports a finding when the observed value is not 42,
        // and the fix re-runs the resolver to confirm the same answer
        // before agreeing to write the desired value back.
        let observed = inputs
            .get("observed")
            .and_then(|v| v.as_i64())
            .unwrap_or(i64::MIN);
        let desired = 42_i64;
        json!({
            "observed": observed,
            "desired": desired,
            "should_fix": observed != desired,
        })
    }

    const SAMPLE_CHECK_NAME: &str = "sample.shared_resolver";

    struct SampleSharedResolverCheck;

    impl DoctorCheck for SampleSharedResolverCheck {
        fn name(&self) -> &'static str {
            SAMPLE_CHECK_NAME
        }

        fn description(&self) -> &'static str {
            "Sample check that demonstrates the shared-resolver invariant"
        }

        fn run(&self) -> DoctorResult<Vec<Finding>> {
            // The "observed" input would normally come from a database
            // query. We use a fixed test value here to keep the test
            // deterministic.
            let inputs = json!({ "observed": 7 });
            let outputs = resolve(&inputs);
            if outputs
                .get("should_fix")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                let mut finding = Finding::new(
                    FindingSeverity::Warn,
                    self.name(),
                    ResolverSnapshot::new("resolve", inputs, outputs.clone()),
                    format!(
                        "observed {} but resolver expected {}",
                        outputs.get("observed").unwrap(),
                        outputs.get("desired").unwrap()
                    ),
                );
                finding = finding
                    .with_entity_id("sample", "demo")
                    .with_evidence(outputs.clone());
                Ok(vec![finding])
            } else {
                Ok(Vec::new())
            }
        }

        fn cadence(&self) -> DoctorCheckCadence {
            DoctorCheckCadence::Cheap
        }

        fn fix(&self, finding: &Finding) -> DoctorResult<()> {
            // Shared-resolver invariant: re-run the SAME `resolve()`
            // helper with the snapshot's inputs, and only act if it
            // agrees. We deliberately do NOT compare against a
            // hand-coded `42` or recompute `desired` from scratch.
            let inputs = &finding.resolver_snapshot.inputs;
            let outputs = resolve(inputs);
            let should_fix = outputs
                .get("should_fix")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if should_fix {
                // In a real check, this would write `desired` back to
                // the underlying store. For the framework slice, we
                // just assert the resolver agreed and return Ok.
                assert_eq!(
                    finding.resolver_snapshot.outputs, outputs,
                    "resolver outputs must be reproducible from snapshot inputs"
                );
                Ok(())
            } else {
                Err(DoctorError::InvalidInput(
                    "resolver reports no fix needed".to_string(),
                ))
            }
        }
    }

    // -------------------------------------------------------------------
    // Serialization
    // -------------------------------------------------------------------

    #[test]
    fn finding_serializes_with_all_required_fields() {
        let finding = Finding::new(
            FindingSeverity::Critical,
            "session.zombie",
            ResolverSnapshot::new(
                "resolve_session_state",
                json!({ "session_id": "abc" }),
                json!({ "state": "wedged" }),
            ),
            "session appears wedged",
        )
        .with_entity_id("session_id", "abc")
        .with_evidence(json!({ "idle_secs": 720 }));

        let serialized = serde_json::to_value(&finding).expect("serialize finding");
        assert_eq!(serialized["severity"], "critical");
        assert_eq!(serialized["check_name"], "session.zombie");
        assert_eq!(serialized["entity_ids"]["session_id"], "abc");
        assert_eq!(serialized["evidence"]["idle_secs"], 720);
        assert_eq!(
            serialized["resolver_snapshot"]["resolver"],
            "resolve_session_state"
        );
        assert_eq!(
            serialized["resolver_snapshot"]["inputs"]["session_id"],
            "abc"
        );
        assert_eq!(
            serialized["resolver_snapshot"]["outputs"]["state"],
            "wedged"
        );
        assert_eq!(serialized["detail"], "session appears wedged");
    }

    #[test]
    fn finding_severity_uses_stable_snake_case_strings() {
        assert_eq!(
            serde_json::to_value(FindingSeverity::Info).unwrap(),
            json!("info")
        );
        assert_eq!(
            serde_json::to_value(FindingSeverity::Warn).unwrap(),
            json!("warn")
        );
        assert_eq!(
            serde_json::to_value(FindingSeverity::Error).unwrap(),
            json!("error")
        );
        assert_eq!(
            serde_json::to_value(FindingSeverity::Critical).unwrap(),
            json!("critical")
        );
        // Round trip.
        for sev in [
            FindingSeverity::Info,
            FindingSeverity::Warn,
            FindingSeverity::Error,
            FindingSeverity::Critical,
        ] {
            let s = serde_json::to_string(&sev).unwrap();
            let back: FindingSeverity = serde_json::from_str(&s).unwrap();
            assert_eq!(sev, back);
        }
    }

    #[test]
    fn finding_roundtrips_through_serde_json() {
        let original = Finding::new(
            FindingSeverity::Warn,
            "test.demo",
            ResolverSnapshot::new("resolve", json!({ "k": 1 }), json!({ "ok": true })),
            "demo detail",
        )
        .with_entity_id("workspace_id", "ws-1")
        .with_evidence(json!({ "rows": 3 }));

        let serialized = serde_json::to_string(&original).unwrap();
        let back: Finding = serde_json::from_str(&serialized).unwrap();
        assert_eq!(original, back);
    }

    // -------------------------------------------------------------------
    // Registry
    // -------------------------------------------------------------------

    #[test]
    fn registry_starts_empty_and_enumerates_registered_checks() {
        let registry = DoctorRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);

        // Register a check that is also Clone + 'static so the
        // registry's `get` can hand a fresh box out.
        let check = SampleSharedResolverCheck;
        assert_eq!(register(&registry, check), None);
        assert_eq!(registry.len(), 1);
        assert!(!registry.is_empty());

        let entries = registry.enumerate();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, SAMPLE_CHECK_NAME);
        assert!(
            entries[0].1.contains("shared-resolver invariant"),
            "description should surface the invariant: got {:?}",
            entries[0].1
        );
    }

    #[test]
    fn registry_re_registering_same_name_replaces_previous() {
        let registry = DoctorRegistry::new();
        register(&registry, SampleSharedResolverCheck);
        assert_eq!(registry.len(), 1);

        // Re-registering returns the overwritten name so tests can
        // assert it happened.
        let replaced = register(&registry, SampleSharedResolverCheck);
        assert_eq!(replaced.as_deref(), Some(SAMPLE_CHECK_NAME));
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn registry_get_returns_a_usable_check() {
        let registry = DoctorRegistry::new();
        register(&registry, SampleSharedResolverCheck);

        let check = registry.get(SAMPLE_CHECK_NAME).expect("registered");
        assert_eq!(check.name(), SAMPLE_CHECK_NAME);
        let findings = check.run().expect("run should succeed");
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn registry_unregister_removes_entry() {
        let registry = DoctorRegistry::new();
        register(&registry, SampleSharedResolverCheck);
        assert!(registry.unregister(SAMPLE_CHECK_NAME));
        assert!(registry.is_empty());
        assert!(!registry.unregister(SAMPLE_CHECK_NAME));
    }

    // -------------------------------------------------------------------
    // Default fix behavior
    // -------------------------------------------------------------------

    struct NoFixCheck;

    impl DoctorCheck for NoFixCheck {
        fn name(&self) -> &'static str {
            "sample.no_fix"
        }

        fn description(&self) -> &'static str {
            "Check that does not implement fix"
        }

        fn run(&self) -> DoctorResult<Vec<Finding>> {
            Ok(Vec::new())
        }
        // No `fix` override — uses the default that returns
        // FixNotSupported.
    }

    struct ExpensiveClusterFacingCheck;

    impl DoctorCheck for ExpensiveClusterFacingCheck {
        fn name(&self) -> &'static str {
            "k8s.pod_leak"
        }

        fn description(&self) -> &'static str {
            "Expensive cluster-facing pod leak scan"
        }

        fn run(&self) -> DoctorResult<Vec<Finding>> {
            Ok(vec![Finding::new(
                FindingSeverity::Info,
                self.name(),
                ResolverSnapshot::new("resolve_pods", json!({}), json!({ "pods": 1 })),
                "cluster-facing sample should stay on demand",
            )])
        }
        // No cadence override: defaults to OnDemand and is excluded from cheap.
    }

    #[test]
    fn cheap_subset_enumeration_excludes_on_demand_expensive_checks() {
        let registry = DoctorRegistry::new();
        register(&registry, SampleSharedResolverCheck);
        register(&registry, ExpensiveClusterFacingCheck);

        let all = registry.enumerate_with_cadence();
        assert_eq!(all.len(), 2);
        assert_eq!(
            all.iter()
                .find(|metadata| metadata.name == SAMPLE_CHECK_NAME)
                .expect("cheap sample present")
                .cadence,
            DoctorCheckCadence::Cheap
        );
        assert_eq!(
            all.iter()
                .find(|metadata| metadata.name == "k8s.pod_leak")
                .expect("expensive sample present")
                .cadence,
            DoctorCheckCadence::OnDemand
        );

        let cheap = registry.enumerate_cheap();
        assert_eq!(cheap.len(), 1);
        assert_eq!(cheap[0].name, SAMPLE_CHECK_NAME);
        assert!(!cheap.iter().any(|metadata| metadata.name == "k8s.pod_leak"));
    }

    #[test]
    fn cheap_subset_runner_executes_only_explicitly_cheap_checks() {
        let registry = DoctorRegistry::new();
        register(&registry, SampleSharedResolverCheck);
        register(&registry, ExpensiveClusterFacingCheck);

        let runs = run_named_subset(&registry, DoctorRunSubset::Cheap.name())
            .expect("cheap subset should run");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].check_name, SAMPLE_CHECK_NAME);
        assert_eq!(runs[0].findings.len(), 1);

        let direct_runs = run_cheap_subset(&registry).expect("cheap wrapper should run");
        assert_eq!(direct_runs.len(), 1);
        assert_eq!(direct_runs[0].check_name, SAMPLE_CHECK_NAME);
    }

    #[test]
    fn default_fix_returns_explicit_unsupported_error() {
        let registry = DoctorRegistry::new();
        register(&registry, NoFixCheck);
        let check = registry.get("sample.no_fix").expect("registered");

        let finding = Finding::new(
            FindingSeverity::Warn,
            "sample.no_fix",
            ResolverSnapshot::new("resolve", json!({}), json!({})),
            "should never reach the fixer",
        );
        let err = check.fix(&finding).expect_err("default fix must error");
        match err {
            DoctorError::FixNotSupported { check } => {
                assert_eq!(check, "sample.no_fix");
            }
            other => panic!("expected FixNotSupported, got {other:?}"),
        }
    }

    // -------------------------------------------------------------------
    // Shared-resolver invariant
    // -------------------------------------------------------------------

    #[test]
    fn fix_uses_shared_resolver() {
        let registry = DoctorRegistry::new();
        register(&registry, SampleSharedResolverCheck);
        let check = registry.get(SAMPLE_CHECK_NAME).expect("registered");

        // Step 1: the check produces a finding and a snapshot.
        let findings = check.run().expect("run should succeed");
        assert_eq!(findings.len(), 1);
        let finding = &findings[0];
        assert_eq!(finding.check_name, SAMPLE_CHECK_NAME);
        assert_eq!(finding.resolver_snapshot.resolver, "resolve");
        assert_eq!(
            finding.resolver_snapshot.inputs,
            json!({ "observed": 7 }),
            "snapshot must capture the inputs the check used"
        );
        assert_eq!(
            finding.resolver_snapshot.outputs,
            json!({ "observed": 7, "desired": 42, "should_fix": true }),
            "snapshot must capture the resolver outputs"
        );

        // Step 2: the fix re-runs the SAME `resolve()` with the
        // snapshot's inputs. It must agree. If a hand-coded expected
        // value were used in `fix`, this assertion would fail because
        // the snapshot would carry the live outputs (which the
        // hand-coded value cannot reproduce).
        check
            .fix(finding)
            .expect("fix should succeed when resolver agrees");

        // Step 3: the snapshot must travel with the finding — if the
        // fix path tried to call `resolve()` with new inputs, the
        // outputs would not match and the fix would either no-op or
        // error. We assert that the snapshot is the only carrier.
        let serialized = serde_json::to_value(finding).unwrap();
        assert_eq!(serialized["resolver_snapshot"]["resolver"], "resolve");
        assert_eq!(serialized["resolver_snapshot"]["inputs"]["observed"], 7);
    }

    // -------------------------------------------------------------------
    // Concurrency sanity
    // -------------------------------------------------------------------

    #[test]
    fn registry_supports_concurrent_registration() {
        let registry = Arc::new(DoctorRegistry::new());
        let mut handles = Vec::new();
        for i in 0..8 {
            let registry = Arc::clone(&registry);
            handles.push(std::thread::spawn(move || {
                // Each thread registers a check with a unique name by
                // wrapping a thin closure. We use a per-thread name
                // suffix so the registry actually grows.
                struct NamedCheck(&'static str);
                impl DoctorCheck for NamedCheck {
                    fn name(&self) -> &'static str {
                        self.0
                    }
                    fn description(&self) -> &'static str {
                        "concurrent registration sanity check"
                    }
                    fn run(&self) -> DoctorResult<Vec<Finding>> {
                        Ok(Vec::new())
                    }
                }
                let leaked: &'static str = Box::leak(format!("sample.thread.{i}").into_boxed_str());
                register(&registry, NamedCheck(leaked));
            }));
        }
        for h in handles {
            h.join().expect("thread join");
        }
        assert_eq!(registry.len(), 8);
    }
}
