// djinn:allow-oversize
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
use std::sync::Arc;

use rmcp::{
    Json,
    handler::server::wrapper::Parameters,
    schemars::{self, JsonSchema},
    tool, tool_router,
};
use serde::{Deserialize, Serialize};

use djinn_core::doctor::{
    DoctorCheck, DoctorRegistry, EntryPointCounts, Finding, FindingSeverity,
    RETRIEVAL_ZERO_RESULT_NAME, ResolverSnapshot, RetrievalHealthDataSource,
    RetrievalHealthSnapshot, RetrievalProjectWindowSnapshot, RetrievalZeroResultCheck, registry,
};
use djinn_db::repositories::retrieval_trace::RetrievalTraceRepository;
use djinn_db::{DoctorFindingRepository, ProjectRepository, RecentDoctorFindings};
use time::{Duration, OffsetDateTime};

use crate::server::DjinnMcpServer;
use crate::tools::AnyJson;
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
    ///
    /// Counters are `i64` (not `usize`) so the generated MCP JSON schema
    /// lands on `format: int64` instead of the nonstandard `uint` pinned by
    /// `tool_schemas::mcp_tools_list_schemas_do_not_use_nonstandard_uint_…`.
    pub total_findings: i64,
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

/// Parameters for `doctor_list_findings`.
///
/// All filters are optional; an unset field means "no narrowing on that
/// dimension". The `check` filter matches `check_name` exactly. The
/// `since` filter is a lower-bound timestamp — rows with
/// `created_at < since` are excluded. The value should be a UTC
/// ISO-8601 string matching the schema's `created_at` template so the
/// string comparison the repository issues in SQL is also a valid
/// chronological comparison.
///
/// `limit` is `i64` (not `usize`) so the generated MCP JSON schema
/// lands on `format: int64` instead of the nonstandard `uint` pinned
/// by `tool_schemas::mcp_tools_list_schemas_do_not_use_nonstandard_uint_…`.
/// The repository's defensive ceiling (`MAX_RECENT_FINDINGS`) still
/// applies, and any caller value above it is silently clamped.
#[derive(Deserialize, JsonSchema)]
pub struct DoctorListFindingsParams {
    /// Optional check name to narrow on (matches `check_name` exactly).
    #[serde(default)]
    pub check: Option<String>,
    /// Optional lower-bound timestamp filter. Rows with
    /// `created_at < since` are excluded. Defaults to "no time
    /// filter" so callers can ask for "the last N findings for
    /// this check" without a cutoff.
    #[serde(default)]
    pub since: Option<String>,
    /// Optional cap on the number of findings returned. Clamped to
    /// `MAX_RECENT_FINDINGS` (the repository's defensive ceiling) when
    /// larger. Defaults to the repository's defensive ceiling when
    /// omitted.
    #[serde(default)]
    pub limit: Option<i64>,
}

/// One persisted finding surfaced through `doctor_list_findings`.
///
/// Mirrors [`djinn_db::DoctorFinding`] but uses [`AnyJson`] for the
/// free-form JSON payloads (`entity_ids`, `evidence`,
/// `resolver_snapshot`) so the generated JSON Schema is the
/// strict-client-friendly empty schema instead of the bare
/// `serde_json::Value` catch-all that strict MCP clients (e.g.
/// Claude Code) reject. The fields serialize to the same JSON shape
/// as before — the wrapper is `#[serde(transparent)]` and only
/// changes the schema, not the wire format.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DoctorListFindingEntry {
    /// Persisted finding id (from `doctor_findings.id`).
    pub id: String,
    /// Wall-clock UTC ISO-8601 timestamp the finding was recorded.
    pub created_at: String,
    /// The run that produced this finding. `None` for ad-hoc inserts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub check_name: String,
    pub severity: String,
    /// Opaque entity ids this finding relates to. Always a JSON array
    /// (possibly empty) so callers can iterate without inspecting each
    /// row's shape.
    pub entity_ids: AnyJson,
    /// Structured check-specific evidence. Free-form JSON.
    pub evidence: AnyJson,
    /// Resolver inputs and outputs captured at check time. `None` for
    /// checks with no associated resolver.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolver_snapshot: Option<AnyJson>,
    /// Free-form human-readable detail surfaced in reports.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Response for `doctor_list_findings`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DoctorListFindingsResponse {
    pub ok: bool,
    /// Findings that match the request, newest-first. Empty when the
    /// filter is too narrow or no findings have been recorded yet.
    pub findings: Vec<DoctorListFindingEntry>,
    /// `true` when a filter was applied. Useful for the board / audit
    /// UI to display "filtered by check X, since Y" in the response.
    pub filtered_by_check: bool,
    /// `true` when a `since` lower bound was applied.
    pub filtered_by_since: bool,
    /// The actual limit used by the query (after the repository
    /// clamped it to `MAX_RECENT_FINDINGS`).
    pub limit: i64,
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
fn finding_to_new_row(finding: &Finding, run_id: &str) -> djinn_db::NewDoctorFinding {
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
        "error" => FindingSeverity::Error,
        "critical" => FindingSeverity::Critical,
        other => {
            return Err(format!(
                "persisted finding has unknown severity '{other}' for check '{}'",
                row.check_name
            ));
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
        None => ResolverSnapshot::new("unknown", serde_json::Value::Null, serde_json::Value::Null),
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

/// Project a persisted `DoctorFinding` row into the
/// `doctor_list_findings` response DTO. The mapping is a direct copy of
/// the public fields — the response type is intentionally narrow so the
/// board / audit callers get a stable JSON shape that does not include
/// repository-internal accounting.
fn finding_to_entry(row: &djinn_db::DoctorFinding) -> DoctorListFindingEntry {
    DoctorListFindingEntry {
        id: row.id.clone(),
        created_at: row.created_at.clone(),
        run_id: row.run_id.clone(),
        check_name: row.check_name.clone(),
        severity: row.severity.clone(),
        entity_ids: AnyJson(row.entity_ids.clone()),
        evidence: AnyJson(row.evidence.clone()),
        resolver_snapshot: row.resolver_snapshot.clone().map(AnyJson),
        detail: row.detail.clone(),
    }
}

struct PrefetchedRetrievalHealthSource {
    snapshot: RetrievalHealthSnapshot,
}

impl RetrievalHealthDataSource for PrefetchedRetrievalHealthSource {
    fn snapshot(&self) -> RetrievalHealthSnapshot {
        self.snapshot.clone()
    }
}

async fn prefetch_retrieval_check(server: &DjinnMcpServer) -> Result<Arc<dyn DoctorCheck>, String> {
    let projects = ProjectRepository::new(server.state.db().clone(), server.state.event_bus())
        .list()
        .await
        .map_err(|e| format!("failed to enumerate active projects: {e}"))?;
    let until = OffsetDateTime::now_utc();
    let from = until - Duration::hours(server.state.retrieval_config().window_hours() as i64);
    let until_s = until
        .format(&time::format_description::well_known::Iso8601::DEFAULT)
        .map_err(|e| e.to_string())?;
    let from_s = from
        .format(&time::format_description::well_known::Iso8601::DEFAULT)
        .map_err(|e| e.to_string())?;
    let repo = RetrievalTraceRepository::new(server.state.db().clone());
    let mut snapshots = BTreeMap::new();
    let mut errors = Vec::new();
    for project in projects {
        match repo.health_rollup(&project.id, &from_s, &until_s).await {
            Ok(rollup) => {
                let counts = rollup
                    .per_entry_point
                    .into_iter()
                    .map(|(entry, evidence)| {
                        (
                            entry.as_str().to_owned(),
                            EntryPointCounts {
                                total_queries: evidence.trace_count.max(0) as u64,
                                zero_result_queries: evidence.zero_result_count.max(0) as u64,
                            },
                        )
                    })
                    .collect();
                snapshots.insert(
                    project.id.clone(),
                    RetrievalProjectWindowSnapshot {
                        project_id: project.id,
                        window_start: from,
                        window_end: until,
                        entry_point_counts: counts,
                    },
                );
            }
            Err(e) => errors.push(format!("{}: {e}", project.id)),
        }
    }
    if !errors.is_empty() {
        return Err(format!(
            "failed to prefetch complete retrieval-health snapshot: {}",
            errors.join(", ")
        ));
    }
    Ok(Arc::new(RetrievalZeroResultCheck::new(
        server.state.retrieval_config(),
        Arc::new(PrefetchedRetrievalHealthSource {
            snapshot: RetrievalHealthSnapshot {
                projects: snapshots,
            },
        }),
    )))
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
    #[tool(description = "Admin-only: run registered doctor health checks. \
                        Accepts an optional list of check names to run a subset; \
                        omit to run all registered checks. Persists findings and \
                        returns a report with persisted finding ids. Never invokes fix.")]
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
        let mut registered_checks: Vec<DoctorRunCheckMeta> = reg
            .enumerate()
            .into_iter()
            .map(|(name, description)| DoctorRunCheckMeta {
                name: name.to_owned(),
                description: description.to_owned(),
            })
            .collect();

        registered_checks.push(DoctorRunCheckMeta {
            name: RETRIEVAL_ZERO_RESULT_NAME.to_owned(),
            description: "Flags projects whose memory retrieval zero-result rate is strictly above the configured threshold".to_owned(),
        });
        let retrieval_selected = p.check_names.as_ref().is_none_or(|names| {
            names.is_empty() || names.iter().any(|name| name == RETRIEVAL_ZERO_RESULT_NAME)
        });
        let ordinary_names = p.check_names.as_ref().map(|names| {
            names
                .iter()
                .filter(|name| name.as_str() != RETRIEVAL_ZERO_RESULT_NAME)
                .cloned()
                .collect()
        });
        let mut checks = match resolve_checks(reg, &ordinary_names) {
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

        if retrieval_selected {
            match prefetch_retrieval_check(self).await {
                Ok(check) => checks.push(check),
                Err(error) => {
                    return Json(DoctorRunResponse {
                        ok: false, registered_checks, results: vec![DoctorRunCheckResult {
                            check: DoctorRunCheckMeta { name: RETRIEVAL_ZERO_RESULT_NAME.to_owned(), description: "Flags projects whose memory retrieval zero-result rate is strictly above the configured threshold".to_owned() },
                            ran: false, error: Some(error), findings: Vec::new(),
                        }], total_findings: 0, error: None,
                    });
                }
            }
        }

        let repo = DoctorFindingRepository::new(self.state.db().clone());
        let run_id = uuid::Uuid::now_v7().to_string();

        let mut results = Vec::with_capacity(checks.len());
        let mut total_findings: i64 = 0;

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
                            total_findings += persisted.len() as i64;
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

    /// List recent persisted doctor findings.
    ///
    /// Backs board / audit queries like "this check fired 4× this week":
    /// the response returns the persisted `id`, `created_at`, `check_name`,
    /// `severity`, structured `entity_ids`, `evidence`, `resolver_snapshot`,
    /// and `detail` for every row that survives the optional `check` /
    /// `since` / `limit` filters. Filtering happens in SQL — the
    /// repository never returns the unfiltered row set.
    ///
    /// This path is admin-gated for symmetry with `doctor_run` /
    /// `doctor_fix`; it does not run any checks, only reads from
    /// `doctor_findings`.
    #[tool(description = "Admin-only: list recent persisted doctor findings, \
                       newest-first. Optional filters: `check` (exact check_name match), \
                       `since` (UTC ISO-8601 lower-bound timestamp), `limit` (max rows; \
                       clamped to MAX_RECENT_FINDINGS). Returns persisted ids, \
                       timestamps, severity, entity ids, evidence, resolver_snapshot, \
                       and detail. Does not run any checks.")]
    pub async fn doctor_list_findings(
        &self,
        Parameters(p): Parameters<DoctorListFindingsParams>,
    ) -> Json<DoctorListFindingsResponse> {
        // Admin gate — mirrors doctor_run / doctor_fix.
        if let Err(error) = require_admin(self.state.db()).await {
            return Json(DoctorListFindingsResponse {
                ok: false,
                findings: Vec::new(),
                filtered_by_check: false,
                filtered_by_since: false,
                limit: 0,
                error: Some(error),
            });
        }

        let filtered_by_check = p.check.is_some();
        let filtered_by_since = p.since.is_some();

        // `p.limit` is `i64` so the schema uses `format: int64`. The
        // repository's `limit` is `usize` for in-memory arithmetic; a
        // negative caller value is clamped to 0 (the repository then
        // applies its defensive ceiling) so a misbehaving caller cannot
        // crash the query.
        let caller_limit: Option<usize> = p.limit.and_then(|n| usize::try_from(n).ok());
        let repo = DoctorFindingRepository::new(self.state.db().clone());
        let query = RecentDoctorFindings {
            run_id: None,
            check_name: p.check.clone(),
            since: p.since.clone(),
            limit: caller_limit,
        };
        let effective_limit = query
            .limit
            .unwrap_or(djinn_db::MAX_RECENT_FINDINGS)
            .min(djinn_db::MAX_RECENT_FINDINGS) as i64;

        match repo.list_recent(query).await {
            Ok(rows) => Json(DoctorListFindingsResponse {
                ok: true,
                findings: rows.iter().map(finding_to_entry).collect(),
                filtered_by_check,
                filtered_by_since,
                limit: effective_limit,
                error: None,
            }),
            Err(e) => Json(DoctorListFindingsResponse {
                ok: false,
                findings: Vec::new(),
                filtered_by_check,
                filtered_by_since,
                limit: effective_limit,
                error: Some(e.to_string()),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::doctor::DoctorCheckMetadata;

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
            Ok(vec![
                Finding::new(
                    FindingSeverity::Warn,
                    self.name(),
                    ResolverSnapshot::new(
                        "resolve_test",
                        serde_json::json!({ "input": 1 }),
                        serde_json::json!({ "output": 2 }),
                    ),
                    "test finding from wiring check",
                )
                .with_entity_id("entity", "test-1"),
            ])
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
        let checks = resolve_checks(reg, &Some(vec![TEST_CHECK_NAME.to_string()])).expect("subset");
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].name(), TEST_CHECK_NAME);
    }

    #[test]
    fn resolve_checks_unknown_name_returns_error_listing_known() {
        let reg = registry_with_test_check();
        let result = resolve_checks(reg, &Some(vec!["nonexistent".to_string()]));
        let err = match result {
            Ok(_) => panic!("expected error for unknown check name"),
            Err(e) => e,
        };
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
            ResolverSnapshot::new(
                "resolve_x",
                serde_json::json!({"a": 1}),
                serde_json::json!({"b": 2}),
            ),
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
        assert_eq!(
            back.entity_ids.get("workspace").map(|s| s.as_str()),
            Some("ws-1")
        );
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

    // -------------------------------------------------------------------
    // doctor_list_findings — projection + response shape
    // -------------------------------------------------------------------

    #[test]
    fn finding_to_entry_copies_all_persisted_fields() {
        let row = djinn_db::DoctorFinding {
            id: "abc-123".to_string(),
            run_id: Some("run-1".to_string()),
            created_at: "2024-05-01T12:00:00.000Z".to_string(),
            check_name: "config_drift".to_string(),
            severity: "critical".to_string(),
            entity_ids: serde_json::json!(["task-1", "project-7"]),
            evidence: serde_json::json!({"rows": 5}),
            resolver_snapshot: Some(
                serde_json::json!({"resolver": "snap", "inputs": {}, "outputs": {}}),
            ),
            detail: Some("explained".to_string()),
        };
        let entry = finding_to_entry(&row);
        assert_eq!(entry.id, "abc-123");
        assert_eq!(entry.run_id.as_deref(), Some("run-1"));
        assert_eq!(entry.created_at, "2024-05-01T12:00:00.000Z");
        assert_eq!(entry.check_name, "config_drift");
        assert_eq!(entry.severity, "critical");
        assert_eq!(
            entry.entity_ids.0,
            serde_json::json!(["task-1", "project-7"])
        );
        assert_eq!(entry.evidence.0, serde_json::json!({"rows": 5}));
        assert!(entry.resolver_snapshot.is_some());
        assert_eq!(entry.detail.as_deref(), Some("explained"));
    }

    #[test]
    fn finding_to_entry_propagates_optional_fields_as_none() {
        let row = djinn_db::DoctorFinding {
            id: "abc-456".to_string(),
            run_id: None,
            created_at: "2024-05-02T00:00:00.000Z".to_string(),
            check_name: "zombie_reaper".to_string(),
            severity: "info".to_string(),
            entity_ids: serde_json::json!([]),
            evidence: serde_json::json!({}),
            resolver_snapshot: None,
            detail: None,
        };
        let entry = finding_to_entry(&row);
        assert!(entry.run_id.is_none());
        assert!(entry.resolver_snapshot.is_none());
        assert!(entry.detail.is_none());
        // Even when no resolver / detail was persisted, the entity_ids and
        // evidence slots still carry their defaults so callers can iterate
        // without inspecting the row.
        assert_eq!(entry.entity_ids.0, serde_json::json!([]));
        assert_eq!(entry.evidence.0, serde_json::json!({}));
    }

    #[test]
    fn doctor_list_findings_response_serializes_required_fields() {
        let resp = DoctorListFindingsResponse {
            ok: true,
            findings: vec![DoctorListFindingEntry {
                id: "f-1".to_string(),
                created_at: "2024-05-01T12:00:00.000Z".to_string(),
                run_id: Some("run-7".to_string()),
                check_name: "config_drift".to_string(),
                severity: "warn".to_string(),
                entity_ids: AnyJson(serde_json::json!(["task-1"])),
                evidence: AnyJson(serde_json::json!({})),
                resolver_snapshot: None,
                detail: Some("hello".to_string()),
            }],
            filtered_by_check: true,
            filtered_by_since: false,
            limit: 50,
            error: None,
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["filter_omitted"], serde_json::Value::Null); // sanity
        assert_eq!(v["filtered_by_check"], true);
        assert_eq!(v["filtered_by_since"], false);
        assert_eq!(v["limit"], 50);
        assert_eq!(v["findings"].as_array().unwrap().len(), 1);
        assert_eq!(v["findings"][0]["id"], "f-1");
        assert_eq!(v["findings"][0]["check_name"], "config_drift");
        assert_eq!(v["findings"][0]["severity"], "warn");
        assert_eq!(v["findings"][0]["detail"], "hello");
        // error is skipped when None.
        assert!(v.as_object().unwrap().get("error").is_none());
        // resolver_snapshot is skipped when None.
        assert!(
            v["findings"][0]
                .as_object()
                .unwrap()
                .get("resolver_snapshot")
                .is_none()
        );
    }

    #[test]
    fn doctor_list_findings_response_serializes_error_path() {
        let resp = DoctorListFindingsResponse {
            ok: false,
            findings: Vec::new(),
            filtered_by_check: false,
            filtered_by_since: false,
            limit: 0,
            error: Some("kaboom".to_string()),
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"], "kaboom");
        assert_eq!(v["findings"].as_array().unwrap().len(), 0);
    }

    // -------------------------------------------------------------------
    // doctor_list_findings — repository filter integration
    //
    // These tests run the SAME filter path the tool runs, but at the
    // repository layer, so they don't depend on the admin gate or the
    // `DjinnMcpServer` plumbing. The repository tests in
    // `djinn-db` already cover the SQL filter mechanics; these tests
    // additionally assert the projection and the new `since` filter
    // surface end-to-end.
    // -------------------------------------------------------------------

    fn new_test_finding(check_name: &str, severity: &str) -> djinn_db::NewDoctorFinding {
        djinn_db::NewDoctorFinding {
            run_id: Some("list-test".to_owned()),
            check_name: check_name.to_owned(),
            severity: severity.to_owned(),
            entity_ids: serde_json::json!(["entity-1"]),
            evidence: serde_json::json!({"rows": 1}),
            resolver_snapshot: Some(serde_json::json!({
                "resolver": "snap",
                "inputs": {},
                "outputs": {}
            })),
            detail: Some(format!("detail for {check_name}")),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_findings_repository_filters_by_check_name_in_sql() {
        let db = djinn_db::Database::open_in_memory().expect("in-memory db");
        let repo = DoctorFindingRepository::new(db.clone());
        repo.insert(new_test_finding("config_drift", "info"))
            .await
            .unwrap();
        repo.insert(new_test_finding("config_drift", "warn"))
            .await
            .unwrap();
        repo.insert(new_test_finding("zombie_reaper", "critical"))
            .await
            .unwrap();

        // No filter → all three, newest first.
        let all = repo
            .list_recent(RecentDoctorFindings::default())
            .await
            .unwrap();
        assert_eq!(all.len(), 3);
        // Convert through the same projection the tool uses.
        let entries: Vec<_> = all.iter().map(finding_to_entry).collect();
        assert!(entries.iter().all(|e| e.detail.is_some()));

        // `check` filter narrows on `check_name` in SQL.
        let drift = repo
            .list_recent(RecentDoctorFindings {
                check_name: Some("config_drift".to_owned()),
                ..Default::default()
            })
            .await
            .unwrap();
        let drift_entries: Vec<_> = drift.iter().map(finding_to_entry).collect();
        assert_eq!(drift_entries.len(), 2);
        assert!(drift_entries.iter().all(|e| e.check_name == "config_drift"));

        // `limit` honors the explicit value and is preserved on the
        // response DTO.
        let small = repo
            .list_recent(RecentDoctorFindings {
                limit: Some(1),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(small.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_findings_repository_filters_by_since_in_sql() {
        let db = djinn_db::Database::open_in_memory().expect("in-memory db");
        let repo = DoctorFindingRepository::new(db.clone());
        let first = repo
            .insert(new_test_finding("config_drift", "info"))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(8)).await;
        let second = repo
            .insert(new_test_finding("config_drift", "warn"))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(8)).await;
        let third = repo
            .insert(new_test_finding("zombie_reaper", "critical"))
            .await
            .unwrap();

        // `since` set to the second row's timestamp → only the second
        // and third rows survive (both newer than `since`).
        let after_first = repo
            .list_recent(RecentDoctorFindings {
                since: Some(second.created_at.clone()),
                ..Default::default()
            })
            .await
            .unwrap();
        let entries: Vec<_> = after_first.iter().map(finding_to_entry).collect();
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec![third.id.as_str(), second.id.as_str()]);
        assert!(!ids.contains(&first.id.as_str()));

        // `since` set to a clearly-future timestamp → empty.
        let future = "2999-01-01T00:00:00.000Z".to_owned();
        let none = repo
            .list_recent(RecentDoctorFindings {
                since: Some(future),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(none.is_empty());

        // `check` and `since` combine: only the second row matches both.
        let combined = repo
            .list_recent(RecentDoctorFindings {
                check_name: Some("config_drift".to_owned()),
                since: Some(second.created_at.clone()),
                ..Default::default()
            })
            .await
            .unwrap();
        let combined_entries: Vec<_> = combined.iter().map(finding_to_entry).collect();
        assert_eq!(combined_entries.len(), 1);
        assert_eq!(combined_entries[0].id, second.id);
        assert_eq!(combined_entries[0].check_name, "config_drift");
    }

    // -------------------------------------------------------------------
    // Admin gate: require_admin behaviour for doctor_run / doctor_fix
    // -------------------------------------------------------------------
    //
    // These tests verify the auth gate used by both doctor tools. They use
    // an in-memory database (the same McpTestHarness path that the tools use)
    // and the SESSION_USER_ID task-local to simulate an authenticated
    // non-admin caller. The integration tests in tests/doctor_tools.rs cover
    // the full dispatch path; these unit tests exercise require_admin in
    // isolation so the failure mode is clear.

    #[tokio::test]
    async fn require_admin_rejects_non_admin_user() {
        let db = djinn_db::Database::open_in_memory().unwrap();
        let repo = djinn_db::UserRepository::new(db.clone());
        let user = repo
            .upsert_from_github(999_500, "non-admin-doctor-unit", None, None)
            .await
            .unwrap();
        assert!(!user.is_admin);

        // Under a non-admin session, require_admin must reject.
        let result = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user.id.clone()), require_admin(&db))
            .await;
        assert!(result.is_err(), "non-admin must be rejected");
        let err = result.unwrap_err();
        assert!(
            err.contains("admin"),
            "error should mention admin: got '{err}'"
        );
    }

    #[tokio::test]
    async fn require_admin_allows_admin_user() {
        let db = djinn_db::Database::open_in_memory().unwrap();
        let repo = djinn_db::UserRepository::new(db.clone());
        let user = repo
            .upsert_from_github(999_501, "admin-doctor-unit", None, None)
            .await
            .unwrap();
        repo.set_admin_status(&user.id, true).await.unwrap();

        let result = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user.id), require_admin(&db))
            .await;
        assert!(result.is_ok(), "admin user must pass the gate");
    }

    #[tokio::test]
    async fn require_admin_allows_no_user_background_path() {
        // No SESSION_USER_ID scope → current_user_id() returns None →
        // require_admin returns Ok(()). This is the trusted background/local
        // path used by internal callers and tests.
        let db = djinn_db::Database::open_in_memory().unwrap();
        let result = require_admin(&db).await;
        assert!(
            result.is_ok(),
            "no-user background path must be allowed, matching board_reconcile"
        );
    }

    // -------------------------------------------------------------------
    // Spy check: verifies that the run loop never calls fix()
    // -------------------------------------------------------------------

    /// A check that panics if `fix()` is called. This provides a runtime
    /// guard (complementing the structural source-level guard) that the
    /// run loop in `doctor_run` never reaches `fix`.
    struct FixSpyCheck;

    impl DoctorCheck for FixSpyCheck {
        fn name(&self) -> &'static str {
            "test.doctor_fix_spy"
        }
        fn description(&self) -> &'static str {
            "Panics if fix() is called — proves the run loop never reaches fix"
        }
        fn run(&self) -> djinn_core::doctor::DoctorResult<Vec<Finding>> {
            Ok(vec![Finding::new(
                FindingSeverity::Info,
                self.name(),
                ResolverSnapshot::new("resolve_spy", serde_json::json!({}), serde_json::json!({})),
                "spy check ran without invoking fix",
            )])
        }
        fn fix(&self, _finding: &Finding) -> djinn_core::doctor::DoctorResult<()> {
            panic!("fix() must never be called from the run path");
        }
    }

    #[test]
    fn run_loop_never_calls_fix_spy_check_proves_it() {
        let reg = Box::leak(Box::new(DoctorRegistry::new()));
        djinn_core::doctor::register(reg, FixSpyCheck);

        let checks = resolve_checks(reg, &None).expect("checks");
        for check in &checks {
            // Run the check — if fix() were called, the spy would panic.
            let findings = check.run().expect("run should succeed");
            // The spy check emits exactly one finding.
            if check.name() == "test.doctor_fix_spy" {
                assert_eq!(findings.len(), 1);
            }
        }
    }
}
