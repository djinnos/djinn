//! Executable contract for the checked production state-sampler inventory.
//!
//! This deliberately parses stable sampler IDs, headings, and source/symbol
//! anchors rather than source line numbers. The audit is reporting-only: an
//! unsafe row is valid when it carries evidence and a separately scoped stable
//! follow-up; an unsafe row is not a reason to hide inventory coverage.

use std::collections::{HashMap, HashSet};

const AUDIT: &str = include_str!("../../../../docs/STATE_SAMPLER_AUDIT.md");

const HEADERS: [&str; 9] = [
    "sampler and production entry point",
    "sampled state",
    "legitimate transit state",
    "authoritative state-entry time",
    "positive exact-owner absence proof",
    "bound and source",
    "safe/unsafe verdict",
    "action/effect",
    "regression or code evidence",
];

/// Production age/staleness guards introduced or modified by proposal `5mzy`.
///
/// The proposal's audit is reporting-only for pre-existing samplers. Keep this
/// allowlist deliberately narrow: a newly listed guard must be validated by the
/// transition-evidence seam below rather than treating the inventory's existing
/// unsafe rows as same-landing remediation.
const PROPOSAL_CHANGED_AGE_STALENESS_GUARDS: [&str; 0] = [];
const NO_CHANGED_GUARDS_SENTINEL: &str = "none";

struct DeclaredEntryPoint {
    sampler_id: &'static str,
    source_anchor: &'static str,
}

// The declarations are intentionally one-to-one with the current inventory
// rows. Adding a production entry point requires adding its stable sampler ID
// and source/symbol anchor here rather than silently widening the audit.
const DECLARED_ENTRY_POINTS: [DeclaredEntryPoint; 13] = [
    DeclaredEntryPoint {
        sampler_id: "liveness-exit-classification",
        source_anchor: "dispatch/liveness.rs",
    },
    DeclaredEntryPoint {
        sampler_id: "stuck-zombie-session-reap",
        source_anchor: "dispatch/session_recovery.rs",
    },
    DeclaredEntryPoint {
        sampler_id: "idle-chat-session-reap",
        source_anchor: "dispatch/session_recovery.rs",
    },
    DeclaredEntryPoint {
        sampler_id: "periodic-stale-task-run-reap",
        source_anchor: "health.rs",
    },
    DeclaredEntryPoint {
        sampler_id: "startup-stage-a-session-interrupt",
        source_anchor: "server/src/server/state/mod.rs",
    },
    DeclaredEntryPoint {
        sampler_id: "startup-stage-b-task-run-reap",
        source_anchor: "health.rs",
    },
    DeclaredEntryPoint {
        sampler_id: "periodic-orphaned-pending-attempt-reap",
        source_anchor: "health.rs",
    },
    DeclaredEntryPoint {
        sampler_id: "startup-stage-c-pending-attempt-reap",
        source_anchor: "health.rs",
    },
    DeclaredEntryPoint {
        sampler_id: "refinement-exact-run-reap",
        source_anchor: "refinement_run.rs:reap_and_admit",
    },
    DeclaredEntryPoint {
        sampler_id: "slot-pool-handle-observation",
        source_anchor: "pool/handle.rs",
    },
    DeclaredEntryPoint {
        sampler_id: "slot-pool-eviction",
        source_anchor: "handle.rs",
    },
    DeclaredEntryPoint {
        sampler_id: "slot-pool-reconcile-termination",
        source_anchor: "control-plane/tools/execution_tools.rs",
    },
    DeclaredEntryPoint {
        sampler_id: "shared-task-run-job-retention",
        source_anchor: "djinn-core/src/job_retention.rs",
    },
];

fn table_cells(line: &str) -> Option<Vec<&str>> {
    let line = line.trim();
    line.strip_prefix('|')?
        .strip_suffix('|')
        .map(|body| body.split('|').map(str::trim).collect())
}

fn follow_up_id(verdict: &str) -> Option<&str> {
    let start = verdict.find("FOLLOW-UP-")?;
    let suffix = &verdict[start..];
    let end = suffix
        .find(|character: char| {
            !(character.is_ascii_uppercase() || character.is_ascii_digit() || character == '-')
        })
        .unwrap_or(suffix.len());
    let candidate = &suffix[..end];
    stable_follow_up_id(candidate).then_some(candidate)
}

fn stable_follow_up_id(candidate: &str) -> bool {
    candidate.strip_prefix("FOLLOW-UP-").is_some_and(|id| {
        !id.is_empty()
            && id.split('-').all(|token| {
                !token.is_empty()
                    && token.chars().all(|character| {
                        character.is_ascii_uppercase() || character.is_ascii_digit()
                    })
            })
    })
}

fn audit_rows(document: &str) -> Result<Vec<Vec<&str>>, String> {
    let inventory = document
        .split_once("## Inventory\n")
        .ok_or_else(|| "missing `## Inventory` heading".to_owned())?
        .1;
    let mut table_lines = inventory.lines().filter(|line| line.starts_with('|'));
    let header = table_lines
        .next()
        .and_then(table_cells)
        .ok_or_else(|| "inventory has no Markdown header row".to_owned())?;
    if header != HEADERS {
        return Err(format!("inventory headers drifted: {header:?}"));
    }
    let separator = table_lines
        .next()
        .and_then(table_cells)
        .ok_or_else(|| "inventory has no Markdown separator row".to_owned())?;
    if separator.len() != HEADERS.len() || separator.iter().any(|cell| *cell != "---") {
        return Err("inventory separator does not match the nine-column schema".to_owned());
    }
    let rows: Vec<_> = table_lines
        .map(|line| table_cells(line).ok_or_else(|| format!("malformed inventory row: {line}")))
        .collect::<Result<_, _>>()?;
    if rows.is_empty() {
        return Err("inventory contains no sampler rows".to_owned());
    }
    Ok(rows)
}

fn proposal_changed_guards(document: &str) -> Result<Vec<&str>, String> {
    let section = document
        .split_once("## Proposal 5mzy changed age/staleness guard allowlist\n")
        .ok_or_else(|| "missing proposal changed-guard allowlist heading".to_owned())?
        .1
        .split_once("\n## ")
        .map_or_else(
            || Err("proposal changed-guard allowlist is missing its next section".to_owned()),
            |(section, _)| Ok(section),
        )?;

    let guards = section
        .lines()
        .filter_map(|line| line.trim().strip_prefix("- `"))
        .filter_map(|line| line.split_once('`').map(|(guard, _)| guard))
        .collect::<Vec<_>>();
    if guards.is_empty() {
        return Err("proposal changed-guard allowlist contains no declaration".to_owned());
    }
    Ok(guards)
}

fn validate_proposal_changed_guard_allowlist(document: &str) -> Result<(), String> {
    let declared = proposal_changed_guards(document)?;
    if PROPOSAL_CHANGED_AGE_STALENESS_GUARDS.is_empty() {
        if declared != [NO_CHANGED_GUARDS_SENTINEL] {
            return Err(format!(
                "expected only `{NO_CHANGED_GUARDS_SENTINEL}` in the empty changed-guard allowlist, got {declared:?}"
            ));
        }
        return Ok(());
    }

    if declared.contains(&NO_CHANGED_GUARDS_SENTINEL) {
        return Err("a non-empty changed-guard allowlist cannot contain `none`".to_owned());
    }
    let declared: HashSet<_> = declared.into_iter().collect();
    let expected: HashSet<_> = PROPOSAL_CHANGED_AGE_STALENESS_GUARDS.into_iter().collect();
    if declared != expected {
        return Err(format!(
            "proposal changed-guard allowlist drifted: expected {expected:?}, got {declared:?}"
        ));
    }
    Ok(())
}

/// Evidence supplied to a sampler before it emits a terminal or destructive
/// verdict about a transit-capable owner. This deliberately models the proof
/// form, rather than a source layout, so controlled fixture mutations exercise
/// the same fail-closed policy for every changed guard.
#[derive(Clone, Copy, Debug)]
enum TransitVerdictEvidence {
    AuthoritativeTerminal,
    StateEntryTime {
        advances_on_every_relevant_transition: bool,
        older_than_documented_bound: bool,
    },
    CreationTime {
        older_than_documented_bound: bool,
    },
    ExactOwnerAbsence(ExactOwnerEvidence),
    EvidenceReadError,
}

#[derive(Clone, Copy, Debug)]
enum ExactOwnerEvidence {
    Present,
    Absent,
    Unknown,
    ReadError,
}

/// The only verdicts this audit contract permits a sampler to produce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransitVerdictPermission {
    Preserve,
    TerminalOrDestructive,
}

fn validate_transit_verdict_evidence(evidence: TransitVerdictEvidence) -> TransitVerdictPermission {
    match evidence {
        TransitVerdictEvidence::AuthoritativeTerminal => {
            TransitVerdictPermission::TerminalOrDestructive
        }
        TransitVerdictEvidence::StateEntryTime {
            advances_on_every_relevant_transition: true,
            older_than_documented_bound: true,
        }
        | TransitVerdictEvidence::ExactOwnerAbsence(ExactOwnerEvidence::Absent) => {
            TransitVerdictPermission::TerminalOrDestructive
        }
        TransitVerdictEvidence::CreationTime {
            older_than_documented_bound,
        } => {
            let _creation_age_is_only_context = older_than_documented_bound;
            TransitVerdictPermission::Preserve
        }
        TransitVerdictEvidence::StateEntryTime { .. }
        | TransitVerdictEvidence::ExactOwnerAbsence(
            ExactOwnerEvidence::Present
            | ExactOwnerEvidence::Unknown
            | ExactOwnerEvidence::ReadError,
        )
        | TransitVerdictEvidence::EvidenceReadError => TransitVerdictPermission::Preserve,
    }
}

fn validate_inventory(document: &str, declared: &[DeclaredEntryPoint]) -> Result<(), String> {
    let rows = audit_rows(document)?;
    let follow_up_section = document
        .split_once("## Follow-ups for unsafe findings\n")
        .map(|(_, section)| {
            section
                .split_once("\n## ")
                .map_or(section, |(section, _)| section)
        })
        .unwrap_or_default();
    let follow_ups: HashSet<_> = follow_up_section
        .lines()
        .filter_map(|line| line.trim().strip_prefix("- **FOLLOW-UP-"))
        .filter_map(|tail| {
            tail.split_once("**")
                .map(|(id, _)| format!("FOLLOW-UP-{id}"))
        })
        .filter(|id| stable_follow_up_id(id))
        .collect();
    let mut by_id = HashMap::new();

    for row in &rows {
        if row.len() != HEADERS.len() {
            return Err(format!(
                "inventory row has {} cells, expected nine",
                row.len()
            ));
        }
        if row.iter().any(|cell| cell.is_empty()) {
            return Err(format!("inventory row has an empty required cell: {row:?}"));
        }
        let sampler_id = row[0]
            .strip_prefix('`')
            .and_then(|cell| cell.split_once('`'))
            .map(|(id, _)| id)
            .ok_or_else(|| format!("row lacks a stable backticked sampler ID: {}", row[0]))?;
        if by_id.insert(sampler_id, row).is_some() {
            return Err(format!("duplicate sampler ID `{sampler_id}`"));
        }
        if row[6].contains("unsafe") {
            let reference = follow_up_id(row[6]).ok_or_else(|| {
                format!("unsafe sampler `{sampler_id}` has no stable FOLLOW-UP reference")
            })?;
            if !follow_ups.contains(reference) {
                return Err(format!(
                    "unsafe sampler `{sampler_id}` references undeclared follow-up `{reference}`"
                ));
            }
        }
    }

    let mut declared_ids = HashSet::new();
    for entry in declared {
        if !declared_ids.insert(entry.sampler_id) {
            return Err(format!(
                "duplicate declared sampler ID `{}`",
                entry.sampler_id
            ));
        }
        let row = by_id
            .get(entry.sampler_id)
            .ok_or_else(|| format!("missing mandatory sampler `{}`", entry.sampler_id))?;
        if !row[8].contains(entry.source_anchor) {
            return Err(format!(
                "sampler `{}` is unreconciled: evidence lacks source/symbol anchor `{}`",
                entry.sampler_id, entry.source_anchor
            ));
        }
    }
    if let Some(undeclared) = by_id
        .keys()
        .find(|sampler_id| !declared_ids.contains(**sampler_id))
    {
        return Err(format!(
            "inventory row `{undeclared}` has no declared production entry point"
        ));
    }
    Ok(())
}

#[test]
fn state_sampler_audit_inventory_is_complete() {
    validate_inventory(AUDIT, &DECLARED_ENTRY_POINTS)
        .unwrap_or_else(|error| panic!("state sampler audit contract failed: {error}"));
    validate_proposal_changed_guard_allowlist(AUDIT)
        .unwrap_or_else(|error| panic!("changed state-sampler guard contract failed: {error}"));
}

// ─── Startup-row resolution contract (task `cv5r`) ───────────────────────────
//
// Cell non-emptiness is not truth. Before this, the three `startup-stage-*`
// rows named `AppState::initialize_agents` as Stage A's production entry point
// (it passes `None` and never reaches Stage A) and cited regression evidence at
// `server/tests/startup_reconnectability.rs` (no such file), and the audit
// stayed green on all of it. These helpers resolve what a startup row claims
// against the checked-out repository: every cited source path must exist, every
// symbol in the documented entry-point chain must be defined in one of those
// files, and each consecutive pair of the chain must be a real call.
//
// Scoped to the startup rows, which are the rows this contract is required to
// hold for; the older rows use abbreviated crate-relative anchors that the
// `DECLARED_ENTRY_POINTS` reconciliation above already pins.

const REPO_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../..");

const SOURCE_EXTENSIONS: [&str; 4] = [".rs", ".md", ".toml", ".sh"];

/// Every backticked token in a cell, in document order.
fn backticked(cell: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut rest = cell;
    while let Some((_, tail)) = rest.split_once('`') {
        match tail.split_once('`') {
            Some((token, remainder)) => {
                tokens.push(token);
                rest = remainder;
            }
            None => break,
        }
    }
    tokens
}

/// The repository path a backticked anchor cites, if it cites one. Anchors are
/// `path[:symbol-or-range]`, so the path is the part before the first colon.
fn cited_path(token: &str) -> Option<&str> {
    let path = token.split(':').next()?;
    SOURCE_EXTENSIONS
        .iter()
        .any(|extension| path.ends_with(extension))
        .then_some(path)
}

fn resolves_in_repo(relative: &str) -> bool {
    std::path::Path::new(REPO_ROOT).join(relative).is_file()
}

fn read_repo_file(relative: &str) -> Option<String> {
    std::fs::read_to_string(std::path::Path::new(REPO_ROOT).join(relative)).ok()
}

/// The documented call chain: every backticked token after the sampler ID,
/// reduced to its final path segment (`health::reap_x` and `AppState::become_leader`
/// both name a function definition, not a module or a type).
fn entry_point_chain(cell: &str) -> Vec<&str> {
    backticked(cell)
        .into_iter()
        .skip(1)
        .map(|symbol| symbol.rsplit("::").next().unwrap_or(symbol))
        .collect()
}

/// Slice a function definition's text: from its `fn <name>(` to the next item
/// declared at file or `impl` scope.
fn function_body<'a>(source: &'a str, name: &str) -> Option<&'a str> {
    const ITEM_STARTS: [&str; 20] = [
        "\nfn ",
        "\npub fn ",
        "\nasync fn ",
        "\npub async fn ",
        "\npub(crate) fn ",
        "\npub(crate) async fn ",
        "\npub(super) fn ",
        "\npub(super) async fn ",
        "\nstruct ",
        "\npub struct ",
        "\nenum ",
        "\npub enum ",
        "\nimpl ",
        "\n    fn ",
        "\n    pub fn ",
        "\n    async fn ",
        "\n    pub async fn ",
        "\n    pub(crate) fn ",
        "\n    pub(crate) async fn ",
        "\n    pub(super) async fn ",
    ];
    let start = source.find(&format!("fn {name}("))?;
    let tail = &source[start..];
    let end = ITEM_STARTS
        .iter()
        .filter_map(|marker| tail[1..].find(marker).map(|offset| offset + 1))
        .min()
        .unwrap_or(tail.len());
    Some(&tail[..end])
}

/// Resolve one startup row against the repository.
fn validate_startup_row_resolves(row: &[&str]) -> Result<(), String> {
    let sampler_id = row[0]
        .strip_prefix('`')
        .and_then(|cell| cell.split_once('`'))
        .map(|(id, _)| id)
        .ok_or_else(|| format!("row lacks a stable backticked sampler ID: {}", row[0]))?;

    // 1. Every path any cell of the row cites must exist in the repository.
    let mut cited_sources = Vec::new();
    for cell in row {
        for path in backticked(cell).into_iter().filter_map(cited_path) {
            if !resolves_in_repo(path) {
                return Err(format!(
                    "startup sampler `{sampler_id}` cites `{path}`, which does not exist in the repository"
                ));
            }
            if path.ends_with(".rs") && !cited_sources.contains(&path) {
                cited_sources.push(path);
            }
        }
    }

    // 2. Every symbol in the documented entry-point chain must be defined in
    //    one of the sources the same row cites as evidence.
    let chain = entry_point_chain(row[0]);
    if chain.len() < 2 {
        return Err(format!(
            "startup sampler `{sampler_id}` documents no production call chain: {}",
            row[0]
        ));
    }
    let mut defining_source = Vec::new();
    for symbol in &chain {
        let owner = cited_sources
            .iter()
            .find(|path| {
                read_repo_file(path).is_some_and(|source| source.contains(&format!("fn {symbol}(")))
            })
            .ok_or_else(|| {
                format!(
                    "startup sampler `{sampler_id}` names `{symbol}`, which is defined in none of \
                     the sources the row cites: {cited_sources:?}"
                )
            })?;
        defining_source.push(*owner);
    }

    // 3. Each consecutive pair must be a real call, not documented adjacency.
    for (index, pair) in chain.windows(2).enumerate() {
        let (caller, callee) = (pair[0], pair[1]);
        let source = read_repo_file(defining_source[index])
            .ok_or_else(|| format!("cannot read `{}`", defining_source[index]))?;
        let body = function_body(&source, caller)
            .ok_or_else(|| format!("cannot slice the body of `{caller}`"))?;
        if !body.contains(&format!("{callee}(")) {
            return Err(format!(
                "startup sampler `{sampler_id}` documents `{caller}` → `{callee}`, but the body of \
                 `{caller}` in `{}` contains no call to `{callee}`",
                defining_source[index]
            ));
        }
    }
    Ok(())
}

/// A follow-up this epic delivered must not still read as an open finding, and
/// no inventory row may cite it as one.
#[test]
fn delivered_follow_ups_are_not_cited_as_open_findings() {
    let delivered_section = AUDIT
        .split_once("## Delivered follow-ups\n")
        .map(|(_, section)| {
            section
                .split_once("\n## ")
                .map_or(section, |(section, _)| section)
        })
        .expect("audit declares a delivered follow-up section");
    let delivered: HashSet<_> = delivered_section
        .lines()
        .filter_map(|line| line.trim().strip_prefix("- **FOLLOW-UP-"))
        .filter_map(|tail| {
            tail.split_once("**")
                .map(|(id, _)| format!("FOLLOW-UP-{id}"))
        })
        .filter(|id| stable_follow_up_id(id))
        .collect();
    assert!(
        delivered.contains("FOLLOW-UP-STALE-TASK-RUN-STARTUP"),
        "epic `43ww` delivered the startup stale-task-run follow-up; it must be recorded as \
         delivered rather than left in the open list"
    );

    let open_section = AUDIT
        .split_once("## Follow-ups for unsafe findings\n")
        .map(|(_, section)| {
            section
                .split_once("\n## ")
                .map_or(section, |(section, _)| section)
        })
        .expect("audit declares an open follow-up section");
    for id in &delivered {
        assert!(
            !open_section.contains(id.as_str()),
            "`{id}` is delivered but still listed as an open follow-up"
        );
    }
    for row in audit_rows(AUDIT).expect("inventory parses") {
        if let Some(reference) = follow_up_id(row[6]) {
            assert!(
                !delivered.contains(reference),
                "inventory row `{}` cites delivered follow-up `{reference}` as an open finding",
                row[0]
            );
        }
    }
}

#[test]
fn startup_sampler_audit_rows_are_present() {
    let rows = audit_rows(AUDIT).expect("startup rows retain the nine-column audit shape");
    let startup_ids = rows
        .iter()
        .filter_map(|row| row[0].strip_prefix('`'))
        .filter_map(|cell| cell.split_once('`').map(|(id, _)| id))
        .filter(|id| id.starts_with("startup-stage-"))
        .collect::<Vec<_>>();
    let expected = [
        "startup-stage-a-session-interrupt",
        "startup-stage-b-task-run-reap",
        "startup-stage-c-pending-attempt-reap",
    ];

    assert_eq!(
        startup_ids.len(),
        expected.len(),
        "exactly A/B/C startup rows"
    );
    assert_eq!(
        startup_ids.iter().collect::<HashSet<_>>().len(),
        expected.len()
    );
    for id in expected {
        assert!(
            startup_ids.contains(&id),
            "missing stable startup sampler `{id}`"
        );
    }
    assert!(
        rows.iter()
            .filter(|row| row[0].contains("startup-stage-"))
            .all(|row| row.len() == HEADERS.len() && row.iter().all(|cell| !cell.is_empty()))
    );

    // Presence and non-emptiness are not truth. Every startup row must also
    // resolve against the checked-out repository: the paths it cites must
    // exist, the symbols in its documented entry-point chain must be defined
    // in those files, and each link of that chain must be a real call.
    for row in rows.iter().filter(|row| row[0].contains("startup-stage-")) {
        validate_startup_row_resolves(row)
            .unwrap_or_else(|error| panic!("startup sampler audit row failed to resolve: {error}"));
    }
}

#[test]
fn state_sampler_transition_timestamp_mutations() {
    // `5mzy` changed no production age/staleness guard: the checked sentinel
    // proves that pre-existing inventory rows were not silently swept into this
    // landing. The synthetic fixtures below retain coverage of the reusable
    // validator for the next changed guard as well.
    assert!(PROPOSAL_CHANGED_AGE_STALENESS_GUARDS.is_empty());
    validate_proposal_changed_guard_allowlist(AUDIT)
        .unwrap_or_else(|error| panic!("changed state-sampler guard contract failed: {error}"));

    assert_eq!(
        validate_transit_verdict_evidence(TransitVerdictEvidence::AuthoritativeTerminal),
        TransitVerdictPermission::TerminalOrDestructive
    );
    assert_eq!(
        validate_transit_verdict_evidence(TransitVerdictEvidence::StateEntryTime {
            advances_on_every_relevant_transition: true,
            older_than_documented_bound: true,
        }),
        TransitVerdictPermission::TerminalOrDestructive
    );
    assert_eq!(
        validate_transit_verdict_evidence(TransitVerdictEvidence::ExactOwnerAbsence(
            ExactOwnerEvidence::Absent,
        )),
        TransitVerdictPermission::TerminalOrDestructive
    );

    let unsafe_mutations = [
        // A creation clock can be old, but it never proves entry into the
        // current transit-capable state.
        TransitVerdictEvidence::CreationTime {
            older_than_documented_bound: true,
        },
        // The time field must advance on every transition relevant to this
        // verdict, not merely exist on the record.
        TransitVerdictEvidence::StateEntryTime {
            advances_on_every_relevant_transition: false,
            older_than_documented_bound: true,
        },
        // Unknown or failed exact-owner observations cannot convict.
        TransitVerdictEvidence::ExactOwnerAbsence(ExactOwnerEvidence::Unknown),
        TransitVerdictEvidence::ExactOwnerAbsence(ExactOwnerEvidence::ReadError),
        TransitVerdictEvidence::ExactOwnerAbsence(ExactOwnerEvidence::Present),
        // Any other evidence-acquisition error is equally fail-closed.
        TransitVerdictEvidence::EvidenceReadError,
    ];
    for mutation in unsafe_mutations {
        assert_eq!(
            validate_transit_verdict_evidence(mutation),
            TransitVerdictPermission::Preserve,
            "mutation {mutation:?} must not authorize a terminal/destructive verdict"
        );
    }
}

#[test]
fn state_sampler_audit_allows_referenced_unsafe_rows_and_rejects_bad_ones() {
    const FIXTURE: &str = concat!(
        "## Inventory\n\n",
        "| sampler and production entry point | sampled state | legitimate transit state | authoritative state-entry time | positive exact-owner absence proof | bound and source | safe/unsafe verdict | action/effect | regression or code evidence |\n",
        "|---|---|---|---|---|---|---|---|---|\n",
        "| `fixture-sampler` — entry | state | transit | transition_at | absent | bound | **unsafe — `FOLLOW-UP-FIXTURE`.** | preserve/report | `source::symbol` |\n",
        "\n## Follow-ups for unsafe findings\n\n",
        "- **FOLLOW-UP-FIXTURE** — independently scoped fixture remediation.\n"
    );
    const DECLARED: [DeclaredEntryPoint; 1] = [DeclaredEntryPoint {
        sampler_id: "fixture-sampler",
        source_anchor: "source::symbol",
    }];

    assert!(validate_inventory(FIXTURE, &DECLARED).is_ok());
    let unreferenced = FIXTURE.replace("- **FOLLOW-UP-FIXTURE**", "- **FOLLOW-UP-OTHER**");
    assert!(validate_inventory(&unreferenced, &DECLARED).is_err());
    let malformed_follow_up = FIXTURE.replace("FOLLOW-UP-FIXTURE", "FOLLOW-UP-");
    assert!(validate_inventory(&malformed_follow_up, &DECLARED).is_err());
    let malformed = FIXTURE.replace(
        "| `fixture-sampler` — entry |",
        "| fixture-sampler — entry |",
    );
    assert!(validate_inventory(&malformed, &DECLARED).is_err());
    let undeclared = FIXTURE.replace(
        "| `fixture-sampler` — entry | state | transit | transition_at | absent | bound | **unsafe — `FOLLOW-UP-FIXTURE`.** | preserve/report | `source::symbol` |\n",
        concat!(
            "| `fixture-sampler` — entry | state | transit | transition_at | absent | bound | **unsafe — `FOLLOW-UP-FIXTURE`.** | preserve/report | `source::symbol` |\n",
            "| `undeclared-sampler` — entry | state | transit | transition_at | absent | bound | **safe.** | preserve/report | `other::symbol` |\n"
        ),
    );
    assert!(validate_inventory(&undeclared, &DECLARED).is_err());
}

// ─── Documented-rule ↔ code binding (task `03im`) ────────────────────────────
//
// `validate_startup_row_resolves` above checks paths, symbol definitions and
// call adjacency. It does not read a row's decision rule, which is how commit
// `5df6e3425` changed the Stage A and Stage C authorization rules and left the
// audit green while both rows stated the opposite of the code — one commit
// after `cv5r` was created to stop exactly that drift.
//
// The rule is therefore no longer only English. Each startup row that gates on
// a closed enum names its variants as a set, and the test below compares that
// set against the set the production predicate actually authorizes, over every
// variant the enum declares. Both sides can move; either move reddens the test.
// Stage A's gate lives in `djinn-server`, which depends on this crate, so its
// half of the binding is `server/src/server/tests/state_sampler_stage_a_binding.rs`.

/// The variant names an `enum <name> { .. }` block declares in `source`.
///
/// Read from the source text rather than hand-listed so that adding a variant
/// to the production enum cannot silently escape the documented set: the new
/// name appears here, matches nothing in the document, and the comparison fails.
fn declared_variants(source: &str, enum_name: &str) -> Vec<String> {
    let header = format!("enum {enum_name} {{");
    let start = source
        .find(&header)
        .unwrap_or_else(|| panic!("`{enum_name}` is not declared in the cited source"));
    let body = &source[start + header.len()..];
    let end = body
        .find("\n}")
        .unwrap_or_else(|| panic!("`{enum_name}` has no closing brace at file scope"));
    body[..end]
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("//") && !line.starts_with('#'))
        .map(|line| {
            let variant = line.trim_end_matches(',');
            assert!(
                !variant.is_empty()
                    && variant
                        .chars()
                        .all(|character| character.is_alphanumeric() || character == '_'),
                "`{enum_name}` declares `{variant}`, which this contract cannot read as a plain \
                 unit variant"
            );
            variant.to_owned()
        })
        .collect()
}

/// The `<key> = {A, B, C}` set an audit cell names, in the document's own words.
fn documented_variant_set(cell: &str, key: &str) -> HashSet<String> {
    let opening = format!("{key} = {{");
    let start = cell
        .find(&opening)
        .unwrap_or_else(|| panic!("audit cell names no `{key}` variant set: {cell}"));
    let body = &cell[start + opening.len()..];
    let end = body
        .find('}')
        .unwrap_or_else(|| panic!("`{key}` variant set is not closed in the audit cell"));
    let named: Vec<_> = body[..end]
        .split(',')
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .collect();
    assert!(!named.is_empty(), "`{key}` names no variant at all");
    let unique: HashSet<_> = named.iter().cloned().collect();
    assert_eq!(
        unique.len(),
        named.len(),
        "`{key}` names a variant twice: {named:?}"
    );
    unique
}

/// The nine cells of the inventory row carrying `sampler_id`.
fn audit_row(sampler_id: &str) -> Vec<&'static str> {
    audit_rows(AUDIT)
        .expect("inventory parses")
        .into_iter()
        .find(|row| row[0].starts_with(&format!("`{sampler_id}`")))
        .unwrap_or_else(|| panic!("audit has no `{sampler_id}` row"))
}

/// Stage C: the projections the document admits to the age/owner classifier
/// must be exactly the projections `startup_attempt_classification_authorized`
/// admits, and the two documented sets together must cover the whole enum.
#[test]
fn startup_audit_stage_c_admitted_set_matches_the_code() {
    use crate::startup_census::TaskCensusProjection;

    // Exhaustive by construction: adding a variant stops this compiling.
    fn name(projection: TaskCensusProjection) -> &'static str {
        match projection {
            TaskCensusProjection::Live => "Live",
            TaskCensusProjection::CreationTransit => "CreationTransit",
            TaskCensusProjection::Unknown => "Unknown",
            TaskCensusProjection::DestructivelyGone => "DestructivelyGone",
            TaskCensusProjection::NotApplicable => "NotApplicable",
        }
    }
    const EVERY_PROJECTION: [TaskCensusProjection; 5] = [
        TaskCensusProjection::Live,
        TaskCensusProjection::CreationTransit,
        TaskCensusProjection::Unknown,
        TaskCensusProjection::DestructivelyGone,
        TaskCensusProjection::NotApplicable,
    ];

    let declared: HashSet<_> =
        declared_variants(include_str!("startup_census.rs"), "TaskCensusProjection")
            .into_iter()
            .collect();
    let enumerated: HashSet<_> = EVERY_PROJECTION
        .iter()
        .map(|projection| name(*projection).to_owned())
        .collect();
    assert_eq!(
        enumerated, declared,
        "this contract does not cover every `TaskCensusProjection` variant the code declares"
    );

    let authorized: HashSet<String> = EVERY_PROJECTION
        .iter()
        .filter(|projection| {
            crate::health::startup_attempt_classification_authorized(Some(**projection))
        })
        .map(|projection| name(*projection).to_owned())
        .collect();
    let deferred: HashSet<String> = EVERY_PROJECTION
        .iter()
        .filter(|projection| {
            !crate::health::startup_attempt_classification_authorized(Some(**projection))
        })
        .map(|projection| name(*projection).to_owned())
        .collect();

    let cell = audit_row("startup-stage-c-pending-attempt-reap")[4];
    assert_eq!(
        documented_variant_set(cell, "stage_c_admitted"),
        authorized,
        "the Stage C audit row's admitted set does not match the projections \
         `startup_attempt_classification_authorized` admits"
    );
    assert_eq!(
        documented_variant_set(cell, "stage_c_deferred"),
        deferred,
        "the Stage C audit row's deferred set does not match the projections \
         `startup_attempt_classification_authorized` refuses"
    );

    // The row also claims a missing projection is never admitted. That is the
    // configured-but-unavailable census, and it is not a variant of the enum.
    assert!(
        !crate::health::startup_attempt_classification_authorized(None),
        "the Stage C row claims a missing projection is never admitted"
    );
}
