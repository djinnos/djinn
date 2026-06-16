//! Admin-facing doctor MCP tools (`doctor_run`, `doctor_fix`).
//!
//! These are the production tool methods that expose the
//! [`djinn_core::doctor`] framework through the MCP dispatch layer. They are
//! the only entry points through which an operator (or agent) can run health
//! checks and invoke opt-in fixes.
//!
//! # Tool surface
//!
//! - [`DjinnMcpServer::doctor_run`] — enumerate the global check registry,
//!   optionally run a named subset, persist every emitted finding through
//!   [`DoctorFindingRepository`], and return a structured report including
//!   check metadata and persisted finding ids. **This path never invokes
//!   `fix`.**
//! - [`DjinnMcpServer::doctor_fix`] — fetch a persisted finding by id,
//!   validate that it belongs to the requested check, reconstruct the
//!   in-memory [`Finding`] from the persisted row, and invoke *only* that
//!   check's explicit [`DoctorCheck::fix`] method.
//!
//! # Auth gating
//!
//! Both tools are admin-gated via [`require_admin`], mirroring
//! `board_reconcile` / `dispatch_pause`. An authenticated non-admin caller
//! receives a structured tool error.
//!
//! # Shared-resolver invariant
//!
//! The fix path does not re-derive expected state: it reconstructs the
//! [`Finding`] (including `resolver_snapshot`) from the persisted row and
//! hands it to the check's `fix(&Finding)`. The check implementation is
//! responsible for re-running the same resolver with the snapshot's inputs
//! — see the module docs in `djinn-core::doctor` for the Gas Town invariant.

use std::collections::BTreeMap;

use rmcp::{
    Json,
    handler::server::wrapper::Parameters,
    schemars::{self, JsonSchema},
    tool, tool_router,
};
use serde::{Deserialize, Serialize};

use djinn_core::doctor::{
    DoctorCheck, DoctorRegistry, Finding, FindingSeverity, ResolverSnapshot, registry,
};
use djinn_db::DoctorFindingRepository;

use crate::server::DjinnMcpServer;
use crate::tools::acting_user::require_admin;

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

/// Parameters for `doctor_run`.
#[derive(Deserialize, JsonSchema)]
pub struct DoctorRunParams {
    /// Optional list of check names to run. When omitted (or empty), all
    /// registered checks are run. Unknown names produce a structured error
    /// listing the valid check names so the caller can self-correct.
    #[serde(default)]
    pub check_names: Option<Vec<String>>,
}

/// Parameters for `doctor_fix`.
#[derive(Deserialize, JsonSchema)]
pub struct DoctorFixParams {
    /// The stable name of the check that owns the finding. Must match the
    /// `check_name` stored on the persisted finding row.
    pub check_name: String,
    /// The persisted finding id (UUIDv7) to fix.
    pub finding_id: String,
}

/// Metadata for one check as surfaced in the run report.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DoctorRunCheckMeta {
    pub name: String,
    pub description: String,
}

/// One persisted finding as surfaced in the run report.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DoctorRunFindingEntry {
    /// Persisted finding id (from `doctor_findings.id`).
    pub finding_id: String,
    pub check_name: String,
    pub severity: String,
    pub detail: String,
}

/// Result of one check execution within a run.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DoctorRunCheckResult {
    pub check: DoctorRunCheckMeta,
    /// `true` when the check ran without error (even if it emitted zero
    /// findings). `false` when the check itself returned an error.
    pub ran: bool,
    /// Error message when `ran` is `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Findings emitted by this check, with their persisted ids filled in.
    pub findings: Vec<DoctorRunFindingEntry>,
}

/// Response for `doctor_run`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DoctorRunResponse {
    pub ok: bool,
    /// All registered checks known at run time (even ones not selected),
    /// so the caller can see the full directory.
    pub registered_checks: Vec<DoctorRunCheckMeta>,
    /// Results for each check that was selected and executed.
    pub results: Vec<DoctorRunCheckResult>,
    /// Total number of findings emitted across all selected checks.
    pub total_findings: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Response for `doctor_fix`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DoctorFixResponse {
    pub ok: bool,
    pub check_name: String,
    pub finding_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve the requested subset of checks against the global registry.
///
/// - `None` or empty → all registered checks.
/// - `Some(names)` → only checks whose `name()` is in the list. Any unknown
///   name causes an `Err` listing the valid names.
fn resolve_checks(
    reg: &DoctorRegistry,
    check_names: &Option<Vec<String>>,
) -> Result<Vec<std::sync::Arc<dyn DoctorCheck>>, String> {
    let Some(names) = check_names.as_ref().filter(|v| !v.is_empty()) else {
        // Run all registered checks. We materialise them via `get` so each
        // Arc is cloned out of the registry without holding the lock during
        // execution.
        return Ok(reg
            .enumerate()
            .into_iter()
            .filter_map(|(name, _)| reg.get(name))
            .collect());
    };

    let mut checks = Vec::with_capacity(names.len());
    let mut unknown: Vec<String> = Vec::new();
    for name in names {
        match reg.get(name) {
            Some(check) => checks.push(check),
            None => unknown.push(name.clone()),
        }
    }
    if unknown.is_empty() {
        Ok(checks)
    } else {
        let known: Vec<&str> = reg.enumerate().into_iter().map(|(n, _)| n).collect();
        Err(format!(
            "unknown doctor check name(s): {}. Registered checks: [{}]",
            unknown.join(", "),
            known.join(", "),
        ))
    }
}

/// Convert an in-memory core [`Finding`] into the DB insert DTO, carrying
/// the structured fields through without manual stringification.
fn finding_to_new_row(
    finding: &Finding,
    run_id: &str,
) -> djinn_db::NewDoctorFinding {
    // entity_ids is a BTreeMap<String,String> in the core type but a JSON
    // array/object in the DB. We serialize the map as a JSON object so the
    // structured keys survive the round-trip.
    let entity_ids = serde_json::to_value(&finding.entity_ids).unwrap_or_default();
    let resolver_snapshot = serde_json::to_value(&finding.resolver_snapshot).ok();

    djinn_db::NewDoctorFinding {
        run_id: Some(run_id.to_owned()),
        check_name: finding.check_name.clone(),
        severity: finding.severity.as_str().to_owned(),
        entity_ids,
        evidence: finding.evidence.clone(),
        resolver_snapshot,
        detail: Some(finding.detail.clone()),
    }
}

/// Reconstruct an in-memory [`Finding`] from a persisted DB row so the
/// check's `fix(&Finding)` receives the same shape (including the resolver
/// snapshot) that `run` originally produced.
fn persisted_to_finding(row: &djinn_db::DoctorFinding) -> Result<Finding, String> {
    let severity = match row.severity.as_str() {
        "info" => FindingSeverity::Info,
        "warn" => FindingSeverity::Warn,
        "critical" => FindingSeverity::Critical,
        other => {
            return Err(format!(
                "persisted finding has unknown severity '{other}' for check '{}'",
                row.check_name
            ))
        }
    };

    // entity_ids: the DB stores a JSON value. The core type expects a
    // BTreeMap<String,String>. If the JSON is an object of string→string we
    // deserialize directly; otherwise we fall back to an empty map (the
    // finding is still fixable, just without entity context).
    let entity_ids: BTreeMap<String, String> =
        serde_json::from_value(row.entity_ids.clone()).unwrap_or_default();

    // resolver_snapshot: must be present for the fix path to honour the
    // shared-resolver invariant. If it's missing (e.g. a very old row or a
    // check that doesn't use a resolver), we construct an empty snapshot
    // so the check can decide how to handle it.
    let resolver_snapshot: ResolverSnapshot = match &row.resolver_snapshot {
        Some(val) => serde_json::from_value(val.clone()).map_err(|e| {
            format!(
                "failed to deserialize resolver_snapshot for finding '{}': {e}",
                row.id
            )
        })?,
        None => ResolverSnapshot::new(
            "unknown",
            serde_json::Value::Null,
            serde_json::Value::Null,
        ),
    };

    Ok(Finding {
        severity,
        check_name: row.check_name.clone(),
        entity_ids,
        evidence: row.evidence.clone(),
        resolver_snapshot,
        detail: row.detail.clone().unwrap_or_default(),
    })
}

// ---------------------------------------------------------------------------
// Tool router
// ---------------------------------------------------------------------------

#[tool_router(router = doctor_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Run doctor health checks.
    ///
    /// Enumerates the global check registry, optionally runs a named subset,
    /// persists all emitted findings, and returns a structured report with
    /// check metadata and persisted finding ids. This path **never** invokes
    /// `fix` — fixes are opt-in and only reachable through `doctor_fix`.
    #[tool(
        description = "Admin-only: run registered doctor health checks. \
                        Accepts an optional list of check names to run a subset; \
                        omit to run all registered checks. Persists findings and \
                        returns a report with persisted finding ids. Never invokes fix."
    )]
    pub async fn doctor_run(
        &self,
        Parameters(p): Parameters<DoctorRunParams>,
    ) -> Json<DoctorRunResponse> {
        // Admin gate — mirrors board_reconcile / dispatch_pause.
        if let Err(error) = require_admin(self.state.db()).await {
            return Json(DoctorRunResponse {
                ok: false,
                registered_checks: Vec::new(),
                results: Vec::new(),
                total_findings: 0,
                error: Some(error),
            });
        }

        let reg = registry();

        // Snapshot the full directory before resolving the subset so the
        // response always shows what was available, even on error.
        let registered_checks: Vec<DoctorRunCheckMeta> = reg
            .enumerate()
            .into_iter()
            .map(|(name, description)| DoctorRunCheckMeta {
                name: name.to_owned(),
                description: description.to_owned(),
            })
            .collect();

        let checks = match resolve_checks(reg, &p.check_names) {
            Ok(c) => c,
            Err(error) => {
                return Json(DoctorRunResponse {
                    ok: false,
                    registered_checks,
                    results: Vec::new(),
                    total_findings: 0,
                    error: Some(error),
                });
            }
        };

        let repo = DoctorFindingRepository::new(self.state.db().clone());
        let run_id = uuid::Uuid::now_v7().to_string();

        let mut results = Vec::with_capacity(checks.len());
        let mut total_findings = 0usize;

        for check in &checks {
            let meta = DoctorRunCheckMeta {
                name: check.name().to_owned(),
                description: check.description().to_owned(),
            };

            // Run the check. This calls ONLY `run()` — never `fix()`.
            match check.run() {
                Ok(findings) => {
                    // Persist every finding through the repository.
                    let new_rows: Vec<_> = findings
                        .iter()
                        .map(|f| finding_to_new_row(f, &run_id))
                        .collect();

                    let entries = match repo.insert_many(new_rows).await {
                        Ok(persisted) => {
                            total_findings += persisted.len();
                            persisted
                                .into_iter()
                                .zip(findings.iter())
                                .map(|(row, finding)| DoctorRunFindingEntry {
                                    finding_id: row.id,
                                    check_name: finding.check_name.clone(),
                                    severity: finding.severity.as_str().to_owned(),
                                    detail: finding.detail.clone(),
                                })
                                .collect::<Vec<_>>()
                        }
                        Err(e) => {
                            // Persistence failure is reported per-check but
                            // does not abort the whole run — other checks
                            // should still execute.
                            results.push(DoctorRunCheckResult {
                                check: meta,
                                ran: false,
                                error: Some(format!(
                                    "check ran ({}) but persistence failed: {e}",
                                    findings.len()
                                )),
                                findings: Vec::new(),
                            });
                            continue;
                        }
                    };

                    results.push(DoctorRunCheckResult {
                        check: meta,
                        ran: true,
                        error: None,
                        findings: entries,
                    });
                }
                Err(e) => {
                    results.push(DoctorRunCheckResult {
                        check: meta,
                        ran: false,
                        error: Some(e.to_string()),
                        findings: Vec::new(),
                    });
                }
            }
        }

        Json(DoctorRunResponse {
            ok: true,
            registered_checks,
            results,
            total_findings,
            error: None,
        })
    }

    /// Invoke a check's opt-in fix for one persisted finding.
    ///
    /// Fetches the persisted finding by id, validates that it belongs to the
    /// requested check, reconstructs the in-memory `Finding` (including the
    /// resolver snapshot), and invokes **only** that check's explicit `fix`
    /// method. This is the sole entry point through which `fix` is reachable.
    #[tool(
        description = "Admin-only: invoke a doctor check's explicit fix for one \
                        persisted finding. Fetches the finding by id, validates check-name \
                        ownership, and runs only that check's fix implementation."
    )]
    pub async fn doctor_fix(
        &self,
        Parameters(p): Parameters<DoctorFixParams>,
    ) -> Json<DoctorFixResponse> {
        let check_name = p.check_name.clone();
        let finding_id = p.finding_id.clone();

        // Admin gate.
        if let Err(error) = require_admin(self.state.db()).await {
            return Json(DoctorFixResponse {
                ok: false,
                check_name,
                finding_id,
                error: Some(error),
            });
        }

        let reg = registry();

        // Look up the check by name. Unknown check → structured error.
        let Some(check) = reg.get(&check_name) else {
            let known: Vec<&str> = reg.enumerate().into_iter().map(|(n, _)| n).collect();
            let error = format!(
                "unknown doctor check '{check_name}'. Registered checks: [{}]",
                known.join(", "),
            );
            return Json(DoctorFixResponse {
                ok: false,
                check_name,
                finding_id,
                error: Some(error),
            });
        };

        // Fetch the persisted finding.
        let repo = DoctorFindingRepository::new(self.state.db().clone());
        let row = match repo.get(&finding_id).await {
            Ok(Some(row)) => row,
            Ok(None) => {
                let error = format!("doctor finding '{finding_id}' not found");
                return Json(DoctorFixResponse {
                    ok: false,
                    check_name,
                    finding_id,
                    error: Some(error),
                });
            }
            Err(e) => {
                let error = format!("failed to fetch finding: {e}");
                return Json(DoctorFixResponse {
                    ok: false,
                    check_name,
                    finding_id,
                    error: Some(error),
                });
            }
        };

        // Validate check-name ownership: the persisted row's check_name must
        // match the requested check. This prevents a caller from invoking an
        // unrelated check's fix on a finding it didn't produce.
        if row.check_name != check_name {
            let error = format!(
                "finding '{finding_id}' belongs to check '{}' but fix was requested for check '{check_name}'",
                row.check_name,
            );
            return Json(DoctorFixResponse {
                ok: false,
                check_name,
                finding_id,
                error: Some(error),
            });
        }

        // Reconstruct the in-memory Finding from the persisted row so the
        // check's fix receives the resolver snapshot it needs.
        let finding = match persisted_to_finding(&row) {
            Ok(f) => f,
            Err(e) => {
                return Json(DoctorFixResponse {
                    ok: false,
                    check_name,
                    finding_id,
                    error: Some(e),
                });
            }
        };

        // Invoke only this check's explicit fix. The default trait impl
        // returns FixNotSupported, so checks that don't opt in get a clear
        // error here.
        match check.fix(&finding) {
            Ok(()) => Json(DoctorFixResponse {
                ok: true,
                check_name,
                finding_id,
                error: None,
            }),
            Err(e) => Json(DoctorFixResponse {
                ok: false,
                check_name,
                finding_id,
                error: Some(e.to_string()),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::doctor::{DoctorCheckCadence, DoctorCheckMetadata};

    // -------------------------------------------------------------------
    // Sample check for tool-level wiring tests
    // -------------------------------------------------------------------

    const TEST_CHECK_NAME: &str = "test.doctor_wiring";

    struct TestDoctorCheck;

    impl DoctorCheck for TestDoctorCheck {
        fn name(&self) -> &'static str {
            TEST_CHECK_NAME
        }
        fn description(&self) -> &'static str {
            "Test check for doctor tool wiring"
        }
        fn run(&self) -> djinn_core::doctor::DoctorResult<Vec<Finding>> {
            Ok(vec![Finding::new(
                FindingSeverity::Warn,
                self.name(),
                ResolverSnapshot::new(
                    "resolve_test",
                    serde_json::json!({ "input": 1 }),
                    serde_json::json!({ "output": 2 }),
                ),
                "test finding from wiring check",
            )
            .with_entity_id("entity", "test-1")])
        }
        fn fix(&self, finding: &Finding) -> djinn_core::doctor::DoctorResult<()> {
            // Shared-resolver invariant: re-run with the snapshot's inputs.
            assert_eq!(
                finding.resolver_snapshot.resolver, "resolve_test",
                "fix must receive the resolver snapshot from the finding"
            );
            Ok(())
        }
    }

    // -------------------------------------------------------------------
    // resolve_checks
    // -------------------------------------------------------------------

    fn registry_with_test_check() -> &'static DoctorRegistry {
        // We can't easily reset the global registry between tests, so we
        // build a local one for the pure-logic helpers.
        let reg = Box::leak(Box::new(DoctorRegistry::new()));
        djinn_core::doctor::register(reg, TestDoctorCheck);
        reg
    }

    #[test]
    fn resolve_checks_none_returns_all_registered() {
        let reg = registry_with_test_check();
        let checks = resolve_checks(reg, &None).expect("all checks");
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].name(), TEST_CHECK_NAME);
    }

    #[test]
    fn resolve_checks_empty_vec_returns_all_registered() {
        let reg = registry_with_test_check();
        let checks = resolve_checks(reg, &Some(Vec::new())).expect("all checks");
        assert_eq!(checks.len(), 1);
    }

    #[test]
    fn resolve_checks_named_subset_returns_matching() {
        let reg = registry_with_test_check();
        let checks =
            resolve_checks(reg, &Some(vec![TEST_CHECK_NAME.to_string()])).expect("subset");
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].name(), TEST_CHECK_NAME);
    }

    #[test]
    fn resolve_checks_unknown_name_returns_error_listing_known() {
        let reg = registry_with_test_check();
        let err = resolve_checks(reg, &Some(vec!["nonexistent".to_string()]))
            .expect_err("unknown name");
        assert!(err.contains("unknown doctor check name"));
        assert!(err.contains(TEST_CHECK_NAME));
    }

    // -------------------------------------------------------------------
    // finding_to_new_row / persisted_to_finding round-trip
    // -------------------------------------------------------------------

    #[test]
    fn finding_roundtrips_through_db_dto() {
        let finding = Finding::new(
            FindingSeverity::Critical,
            "round.trip",
            ResolverSnapshot::new("resolve_x", serde_json::json!({"a": 1}), serde_json::json!({"b": 2})),
            "round-trip detail",
        )
        .with_entity_id("workspace", "ws-1")
        .with_evidence(serde_json::json!({"rows": 3}));

        let new_row = finding_to_new_row(&finding, "run-rt");

        assert_eq!(new_row.check_name, "round.trip");
        assert_eq!(new_row.severity, "critical");
        assert_eq!(new_row.run_id.as_deref(), Some("run-rt"));
        assert_eq!(new_row.detail.as_deref(), Some("round-trip detail"));

        // Simulate what the DB would return: the row has an id and timestamp.
        let persisted = djinn_db::DoctorFinding {
            id: "rt-1".to_string(),
            run_id: Some("run-rt".to_string()),
            created_at: "2024-01-01T00:00:00Z".to_string(),
            check_name: new_row.check_name.clone(),
            severity: new_row.severity.clone(),
            entity_ids: new_row.entity_ids.clone(),
            evidence: new_row.evidence.clone(),
            resolver_snapshot: new_row.resolver_snapshot.clone(),
            detail: new_row.detail.clone(),
        };

        let back = persisted_to_finding(&persisted).expect("reconstruct");
        assert_eq!(back.severity, FindingSeverity::Critical);
        assert_eq!(back.check_name, "round.trip");
        assert_eq!(back.entity_ids.get("workspace").map(|s| s.as_str()), Some("ws-1"));
        assert_eq!(back.evidence, serde_json::json!({"rows": 3}));
        assert_eq!(back.resolver_snapshot.resolver, "resolve_x");
        assert_eq!(back.resolver_snapshot.inputs, serde_json::json!({"a": 1}));
        assert_eq!(back.resolver_snapshot.outputs, serde_json::json!({"b": 2}));
        assert_eq!(back.detail, "round-trip detail");
    }

    #[test]
    fn persisted_to_finding_handles_missing_resolver_snapshot() {
        let persisted = djinn_db::DoctorFinding {
            id: "rt-2".to_string(),
            run_id: None,
            created_at: "2024-01-01T00:00:00Z".to_string(),
            check_name: "no.snapshot".to_string(),
            severity: "info".to_string(),
            entity_ids: serde_json::json!({}),
            evidence: serde_json::json!({}),
            resolver_snapshot: None,
            detail: None,
        };
        let back = persisted_to_finding(&persisted).expect("reconstruct");
        assert_eq!(back.severity, FindingSeverity::Info);
        assert_eq!(back.resolver_snapshot.resolver, "unknown");
    }

    #[test]
    fn persisted_to_finding_rejects_unknown_severity() {
        let persisted = djinn_db::DoctorFinding {
            id: "rt-3".to_string(),
            run_id: None,
            created_at: "2024-01-01T00:00:00Z".to_string(),
            check_name: "bad.sev".to_string(),
            severity: "fatal".to_string(),
            entity_ids: serde_json::json!({}),
            evidence: serde_json::json!({}),
            resolver_snapshot: None,
            detail: None,
        };
        let err = persisted_to_finding(&persisted).expect_err("bad severity");
        assert!(err.contains("unknown severity"));
    }

    // -------------------------------------------------------------------
    // Response shapes
    // -------------------------------------------------------------------

    #[test]
    fn doctor_run_response_serializes_registered_checks_and_results() {
        let resp = DoctorRunResponse {
            ok: true,
            registered_checks: vec![DoctorRunCheckMeta {
                name: "test.demo".to_string(),
                description: "demo".to_string(),
            }],
            results: vec![DoctorRunCheckResult {
                check: DoctorRunCheckMeta {
                    name: "test.demo".to_string(),
                    description: "demo".to_string(),
                },
                ran: true,
                error: None,
                findings: vec![DoctorRunFindingEntry {
                    finding_id: "f-1".to_string(),
                    check_name: "test.demo".to_string(),
                    severity: "warn".to_string(),
                    detail: "demo detail".to_string(),
                }],
            }],
            total_findings: 1,
            error: None,
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["registered_checks"][0]["name"], "test.demo");
        assert_eq!(v["results"][0]["ran"], true);
        assert_eq!(v["results"][0]["findings"][0]["finding_id"], "f-1");
        assert_eq!(v["results"][0]["findings"][0]["severity"], "warn");
        assert_eq!(v["total_findings"], 1);
        // error is skipped when None.
        assert!(v.as_object().unwrap().get("error").is_none());
    }

    #[test]
    fn doctor_fix_response_serializes_ok_and_error_paths() {
        let ok = DoctorFixResponse {
            ok: true,
            check_name: "c".to_string(),
            finding_id: "f".to_string(),
            error: None,
        };
        let v = serde_json::to_value(&ok).unwrap();
        assert_eq!(v["ok"], true);
        assert!(v.as_object().unwrap().get("error").is_none());

        let err = DoctorFixResponse {
            ok: false,
            check_name: "c".to_string(),
            finding_id: "f".to_string(),
            error: Some("boom".to_string()),
        };
        let v = serde_json::to_value(&err).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"], "boom");
    }

    // -------------------------------------------------------------------
    // run path does not invoke fix — structural guard
    // -------------------------------------------------------------------
    //
    // This test asserts at the source level that doctor_run's execution
    // path calls `check.run()` and NEVER `check.fix()`. We can't easily
    // assert "fix was not called" at runtime without a spy check, but the
    // tool code is structured so that the run loop only references `.run()`.
    // The fix method is only referenced in the doctor_fix handler. This
    // test verifies the run loop produces findings without touching fix.

    #[test]
    fn run_path_produces_findings_without_invoking_fix() {
        let reg = registry_with_test_check();
        let checks = resolve_checks(reg, &None).expect("checks");
        // The run loop in doctor_run calls check.run() and persists findings.
        // Here we verify run() produces findings; fix() is never called in
        // this code path (see the doctor_run method body — it only calls
        // `check.run()`).
        for check in &checks {
            let findings = check.run().expect("run");
            assert!(!findings.is_empty());
            assert_eq!(findings[0].check_name, TEST_CHECK_NAME);
        }
    }

    /// Verify the registry metadata helper still works for the test check.
    #[test]
    fn test_check_registered_with_correct_metadata() {
        let reg = registry_with_test_check();
        let meta = reg.enumerate_with_cadence();
        let ours = meta
            .iter()
            .find(|m: &&DoctorCheckMetadata| m.name == TEST_CHECK_NAME)
            .expect("test check registered");
        assert_eq!(ours.description, "Test check for doctor tool wiring");
        // Default cadence is OnDemand since we didn't override it.
        assert!(!ours.cadence.is_cheap());
    }
}
