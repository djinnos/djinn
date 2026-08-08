//! Lead CI-adjudication prompt contract (proposal `nafu`, wave 4).
//!
//! Wave 2 gave the coordinator an immutable evidence identity and a closed
//! route classifier. Wave 4 gives the *adjudicator* its contract: what Lead
//! receives, which of its four existing results are legal for a non-passing
//! CI input, and which of the two mutually exclusive `reopen` plans it must
//! choose. This module pins that contract in `lead.md` so it cannot be
//! silently deleted.
//!
//! # Why these tests are shaped this way
//!
//! A prompt assertion that still passes when the prompt text is deleted
//! proves nothing. Every clause below therefore goes through one shared
//! [`ci_contract_violations`] checker, and the module asserts the checker
//! from *both* directions:
//!
//! * [`lead_prompt_states_the_full_ci_adjudication_contract`] — the real
//!   rendered prompt has zero violations.
//! * [`ci_contract_check_fails_when_the_section_is_deleted`] — the same
//!   checker run against the rendered prompt *with the section cut out*
//!   reports every clause as missing.
//!
//! The second test is what makes the first one evidence. Without it, an
//! assertion set that had drifted into checking words which appear elsewhere
//! in `base.md` would stay green forever.
//!
//! Clauses are additionally required to live *inside* the CI section rather
//! than merely somewhere in the prompt, so relocating the contract into an
//! unrelated section is also a failure.

use super::*;
use crate::AgentType;
use crate::prompts::tests::{make_ctx, make_task, render_prompt};

/// Heading that opens the contract section in `lead.md`.
const SECTION_HEADING: &str = "## CI Failure Adjudication";

/// One required clause of the contract: a stable id and the exact text that
/// must appear inside the CI adjudication section.
struct Clause {
    id: &'static str,
    text: &'static str,
}

const fn clause(id: &'static str, text: &'static str) -> Clause {
    Clause { id, text }
}

/// The complete contract, in the order the acceptance matrix names it:
/// complete evidence bundle, passing-only approve, no retry result, mutually
/// exclusive repair/diagnose requirements, narrow cited park, and no
/// dependency mutation.
const REQUIRED_CLAUSES: &[Clause] = &[
    // ── Complete evidence bundle ────────────────────────────────────────
    clause("bundle.lane", "the lane (`pr_head` or `merge_group`)"),
    clause("bundle.origin_state", "the origin board state"),
    clause("bundle.pr_head_sha", "the PR-head SHA"),
    clause(
        "bundle.run_identity",
        "the workflow run ID and run head SHA",
    ),
    clause(
        "bundle.dequeue",
        "the dequeue identity and dequeue reason when the lane is `merge_group`",
    ),
    clause(
        "bundle.ranked_checks",
        "the ranked blocking checks and their causal subset",
    ),
    clause("bundle.annotations", "check annotations"),
    clause(
        "bundle.log_refs",
        "bounded failure sections and log references",
    ),
    clause(
        "bundle.fingerprints",
        "the current and prior transient fingerprints",
    ),
    clause("bundle.budget_counts", "the charged retry-budget counts"),
    clause("bundle.action_phase", "the action phase"),
    clause("bundle.provider_error", "the provider error envelope"),
    clause(
        "bundle.repo_context",
        "the repository commands already present in task/project context",
    ),
    clause(
        "bundle.exclusions",
        "Project configuration and memory-derived remedy hints are deliberately excluded.",
    ),
    clause(
        "bundle.absent_annotation",
        "An annotation's absence is not proof that a failure was transient.",
    ),
    // ── No retry result, no provider-action edge ────────────────────────
    clause(
        "no_retry.results",
        "There is **no `retrigger` decision and no `requeue` decision.**",
    ),
    clause(
        "no_retry.rerun_failed_jobs",
        "You never call `rerun_failed_jobs`",
    ),
    clause("no_retry.enable_auto_merge", "`enable_auto_merge`"),
    clause(
        "no_retry.directive_channel",
        "or by asking for one in a directive or rationale",
    ),
    // ── Passing-only approve ────────────────────────────────────────────
    clause("approve.invalid_here", "**`approve` is invalid here.**"),
    clause(
        "approve.passing_only",
        "It stays reserved for the existing passing-CI path.",
    ),
    clause(
        "approve.never_non_passing",
        "Non-passing CI can never be approved",
    ),
    // ── Mutually exclusive repair / diagnose ────────────────────────────
    clause(
        "reopen.exclusive",
        "exactly one of two mutually exclusive plans. Providing both, or neither, is invalid.",
    ),
    clause(
        "repair.required",
        "Required: non-empty evidence references, an evidence-grounded `directive`, and a \
         repository-valid `verification_command`.",
    ),
    clause("repair.forbidden", "Forbidden: `diagnostic_reason`."),
    clause(
        "diagnose.required_directive",
        "a `directive` that names precisely **what remains unknown**",
    ),
    clause(
        "diagnose.reason.evidence_incomplete",
        "`evidence_incomplete`",
    ),
    clause(
        "diagnose.reason.provider_action_failed",
        "`provider_action_failed`",
    ),
    clause("diagnose.reason.no_grounded_remedy", "`no_grounded_remedy`"),
    clause(
        "diagnose.reason.no_repository_command",
        "`no_repository_command`",
    ),
    clause(
        "diagnose.forbidden",
        "Forbidden: `verification_command`, and any claim of a known fix.",
    ),
    clause(
        "command.repository_valid",
        "repository-valid only when you copied it from repository/task context, or CI evidence \
         exposed it directly as a command",
    ),
    clause(
        "command.no_invention",
        "You may **not** invent a command from a job name",
    ),
    clause(
        "command.fallback_is_diagnose",
        "If either the remedy or a valid command is unavailable, the repair plan is invalid and \
         you must use the diagnose plan.",
    ),
    // ── Exactness of the two corpora (payload-quality fix) ───────────────
    //
    // The rule alone was already here and was already being obeyed *in
    // spirit*: on 2026-08-08 four of five rejected adjudications proposed a
    // semantically correct command that had been decorated or narrowed. What
    // was missing is that the comparison is string equality, so "the same
    // command" is not a defence. See `lead_prompt_requires_the_two_corpora_to
    // _be_copied_verbatim`.
    clause(
        "corpora.delivered",
        "reproduced verbatim in the *CI Evidence Bundle For This Route* block below",
    ),
    clause(
        "corpora.reference_is_substring",
        "Your `directive` is grounded only if it contains one of the listed strings **as an exact \
         substring**",
    ),
    clause(
        "corpora.command_is_equality",
        "A repair's `verification_command` must **equal** one of the listed commands.",
    ),
    clause(
        "exact.headline",
        "**Copy the command character for character. Editing it makes it an invented command.**",
    ),
    clause(
        "exact.equality_rule",
        "string equality against the listed corpus after collapsing runs of whitespace",
    ),
    clause("exact.no_cd", "`cd server && <command>` is not `<command>`"),
    clause(
        "exact.no_env_prefix",
        "`SQLX_OFFLINE=true <command>` or `UPDATE_FIXTURE=1 <command>` is not `<command>`",
    ),
    clause(
        "exact.no_narrowing",
        "replacing a workspace-wide run with `-p <crate>`, `--lib`, or a single test-path filter",
    ),
    clause(
        "exact.no_flag_edits",
        "adding, removing, or reordering any flag, and appending `-- --nocapture`",
    ),
    clause(
        "exact.no_chaining",
        "merging two listed commands into one `&&` chain",
    ),
    clause(
        "exact.escape_hatch",
        "reopen with the diagnose plan and `no_repository_command`, put the command you would \
         have run in the `directive` as prose",
    ),
    clause(
        "exact.reference_verbatim",
        "paste one listed evidence reference into it verbatim",
    ),
    // ── The run-absent, diagnose-only route (revision 58) ────────────────
    //
    // Keyed on the coordinator's explicit `diagnose_only` flag rather than on
    // the absent `run_id`. "Infer the restriction from a field that is not
    // there" is not a rule a model can be held to, and the supervisor rejects
    // the repair either way — so an inference that went the other way costs a
    // whole Lead session to discover. The flag's spelling is pinned on the
    // producing side by `tier2_dispatch`'s own tests; this is the consuming
    // side of the same name.
    clause(
        "run_absent.no_repair",
        "**When the bundle is marked `diagnose_only`, the repair plan is unavailable to you.**",
    ),
    clause(
        "run_absent.why",
        "No command you could give could have been copied from CI evidence, because this route \
         captured none",
    ),
    clause(
        "run_absent.exits",
        "Diagnose with `evidence_incomplete`, naming exactly which evidence is missing, or park \
         if the capture failure is itself a cited platform dead-end.",
    ),
    // ── Narrow, cited park ──────────────────────────────────────────────
    clause(
        "park.cited_dead_end",
        "**cited infrastructure dead-end that no rerun and no code change could clear**",
    ),
    clause("park.not_uncertainty", "uncertainty about the cause"),
    clause("park.not_exhausted_budget", "an exhausted retry budget"),
    clause(
        "park.not_provider_error",
        "a provider error or a failed provider action",
    ),
    clause(
        "park.not_stale_artifact",
        "a stale or colliding repository artifact — reopen with a grounded directive to rebase \
         and re-derive it",
    ),
    // ── No dependency mutation ──────────────────────────────────────────
    clause("graph.heading", "### Do not mutate the dependency graph"),
    clause(
        "graph.no_edges",
        "Do **not** call `blocked_by_add`, do not create blocker edges",
    ),
    clause(
        "graph.no_attribution",
        "do not attribute the failure to a competing task",
    ),
    // ── The atomic guard the supervisor applies ─────────────────────────
    clause(
        "guard.same_transaction",
        "**in the same transaction** that applies your decision",
    ),
    clause(
        "guard.discarded",
        "your result is discarded as superseded — no reopen, no worker, no park, no supersede, \
         no board change",
    ),
];

/// Slice out the CI adjudication section: from its heading up to the next
/// top-level `##` heading (or end of prompt).
///
/// Returning `None` — rather than falling back to the whole prompt — is what
/// makes a deleted section report *every* clause as missing instead of
/// accidentally matching text elsewhere in `base.md`.
fn ci_section(prompt: &str) -> Option<&str> {
    let start = prompt.find(SECTION_HEADING)?;
    let body = &prompt[start + SECTION_HEADING.len()..];
    let end = body.find("\n## ").map_or(body.len(), |i| i + 1);
    Some(&body[..end])
}

/// Ids of every contract clause missing from `prompt`'s CI section.
///
/// A missing section yields every id, which is precisely what
/// [`ci_contract_check_fails_when_the_section_is_deleted`] asserts.
fn ci_contract_violations(prompt: &str) -> Vec<&'static str> {
    let Some(section) = ci_section(prompt) else {
        return REQUIRED_CLAUSES.iter().map(|c| c.id).collect();
    };
    REQUIRED_CLAUSES
        .iter()
        .filter(|c| !section.contains(c.text))
        .map(|c| c.id)
        .collect()
}

/// Render the Lead prompt exactly as the dispatcher would.
fn lead_prompt() -> String {
    render_prompt(AgentType::Lead, &make_task(), &make_ctx())
}

/// Remove the CI adjudication section, leaving the rest of the prompt intact.
fn without_ci_section(prompt: &str) -> String {
    let start = prompt.find(SECTION_HEADING).expect("section present");
    let tail = &prompt[start + SECTION_HEADING.len()..];
    let end = tail.find("\n## ").map_or(tail.len(), |i| i + 1);
    let mut out = String::with_capacity(prompt.len());
    out.push_str(&prompt[..start]);
    out.push_str(&tail[end..]);
    out
}

// ── The contract itself ────────────────────────────────────────────────────

/// Every clause of the CI adjudication contract is present, and present
/// *inside* the CI section rather than merely somewhere in the prompt.
#[test]
fn lead_prompt_states_the_full_ci_adjudication_contract() {
    let violations = ci_contract_violations(&lead_prompt());
    assert!(
        violations.is_empty(),
        "lead.md is missing CI adjudication contract clauses: {violations:?}"
    );
}

/// The load-bearing half: deleting the section must make the checker fail for
/// every clause. Without this, the assertions above could have drifted into
/// matching boilerplate that lives elsewhere in the rendered prompt.
#[test]
fn ci_contract_check_fails_when_the_section_is_deleted() {
    let full = lead_prompt();
    let stripped = without_ci_section(&full);

    assert!(
        stripped.len() < full.len(),
        "the strip helper must actually remove text"
    );
    assert!(
        ci_section(&stripped).is_none(),
        "the CI section must be gone after stripping"
    );
    // The rest of the Lead prompt survives, so this is a section deletion and
    // not a whole-prompt deletion.
    assert!(
        stripped.contains("Forensic Arbiter"),
        "stripping must leave the rest of the Lead prompt intact"
    );

    let violations = ci_contract_violations(&stripped);
    assert_eq!(
        violations.len(),
        REQUIRED_CLAUSES.len(),
        "deleting the CI section must invalidate EVERY clause; still satisfied: {:?}",
        REQUIRED_CLAUSES
            .iter()
            .map(|c| c.id)
            .filter(|id| !violations.contains(id))
            .collect::<Vec<_>>()
    );
}

// ── Passing-only approve ───────────────────────────────────────────────────

/// `approve` must be scoped to the existing passing-CI path and explicitly
/// invalid for non-passing CI adjudication.
#[test]
fn lead_prompt_forbids_approving_non_passing_ci() {
    let prompt = lead_prompt();
    let section = ci_section(&prompt).expect("CI section present");

    assert!(
        section.contains("**`approve` is invalid here.**"),
        "the CI section must declare approve invalid for CI adjudication"
    );
    assert!(
        section.contains(
            "however transient the failure looks and however many budgets are \
                          exhausted"
        ),
        "the CI section must close the 'it was only transient' and 'budget is spent' escapes"
    );
}

// ── No retry result ────────────────────────────────────────────────────────

/// The decision surface must not grow a retry rung. `retrigger` and `requeue`
/// may appear only in the sentence that forbids them.
#[test]
fn lead_prompt_offers_no_retry_decision() {
    let prompt = lead_prompt();

    // Still exactly the five existing decisions.
    assert!(
        prompt.contains("five possible decisions"),
        "the decision matrix must still declare five decisions"
    );
    for banned in ["decision=\"retrigger\"", "decision=\"requeue\""] {
        assert!(
            !prompt.contains(banned),
            "lead prompt must not offer {banned} as a decision"
        );
    }

    // Both words occur exactly once each — in the prohibition.
    for word in ["retrigger", "requeue"] {
        let occurrences = prompt.matches(word).count();
        assert_eq!(
            occurrences, 1,
            "'{word}' must appear exactly once (the prohibition), found {occurrences}"
        );
    }

    let section = ci_section(&prompt).expect("CI section present");
    assert!(
        section.contains("re-running a run is the poller's decision and yours cannot authorize it"),
        "the CI section must place retry authority with the poller, not with Lead"
    );
    assert!(
        section.contains("that is not a decision you can render"),
        "the CI section must tell Lead what to do INSTEAD of asking for a re-run"
    );
}

/// Lead's finalize surface is unchanged: one finalize tool, no retry tool.
#[test]
fn lead_config_exposes_no_retry_finalize_tool() {
    use crate::config::LEAD_CONFIG;

    assert_eq!(
        LEAD_CONFIG.finalize_tool_names,
        &["submit_decision"],
        "CI routing must not add a finalize tool to Lead"
    );
}

// ── Mutually exclusive repair / diagnose ───────────────────────────────────

/// Each reopen plan must state both its required and its forbidden fields, so
/// the exclusivity is expressed in the prompt and not only in the validator.
#[test]
fn lead_prompt_states_mutually_exclusive_reopen_plans() {
    let prompt = lead_prompt();
    let section = ci_section(&prompt).expect("CI section present");

    assert!(
        section.contains("**Repair plan**") && section.contains("**Diagnose plan**"),
        "both reopen plans must be named"
    );
    // Two required/forbidden pairs, one per plan.
    assert_eq!(
        section.matches("- Required:").count(),
        2,
        "each reopen plan must state its required fields"
    );
    assert_eq!(
        section.matches("- Forbidden:").count(),
        2,
        "each reopen plan must state its forbidden fields"
    );
    // Diagnose must be presented as a legitimate outcome, or the model will
    // manufacture a repair to avoid what reads like a failure.
    assert!(
        section.contains(
            "A diagnostic reopen is the correct, expected, non-failure outcome of an unclear \
             failure"
        ),
        "the CI section must legitimise the diagnostic reopen"
    );
}

/// The closed diagnostic-reason set must match `djinn_db::CiDiagnosticReason`
/// exactly — no extra reason, no missing reason.
///
/// `djinn-roles` cannot depend on `djinn-db`, so the durable enum's four
/// values are restated here; `djinn-agent`'s
/// `ci_routing::diagnostic_reason_set_matches_the_durable_enum` pins the same
/// four against the real type, which is what keeps the two in step.
#[test]
fn lead_prompt_lists_exactly_the_closed_diagnostic_reason_set() {
    let prompt = lead_prompt();
    let section = ci_section(&prompt).expect("CI section present");

    let closed_set = [
        "evidence_incomplete",
        "provider_action_failed",
        "no_grounded_remedy",
        "no_repository_command",
    ];
    let sentence = section
        .lines()
        .find(|l| l.contains("`diagnostic_reason` from this closed set"))
        .expect("the diagnose plan must name the closed reason set");
    let (_, listed) = sentence
        .split_once("closed set:")
        .expect("the closed set must be introduced by 'closed set:'");
    for reason in closed_set {
        assert!(
            listed.contains(reason),
            "closed reason '{reason}' missing from the diagnose plan"
        );
    }
    // The set is *closed*: exactly four backticked names follow, so a fifth
    // reason cannot be smuggled in alongside the four this test knows about.
    assert_eq!(
        listed.matches('`').count(),
        closed_set.len() * 2,
        "the diagnose plan must list exactly four reasons, got: {listed}"
    );
}

// ── The run-absent, diagnose-only route ────────────────────────────────────

/// Revision 58's run-absent route: `repair` is unavailable, and the prompt must
/// say **why**, or the model reads the restriction as arbitrary and argues with
/// it.
///
/// The supervisor's `ci_routing::run_absent_route_rejects_repair_and_approve`
/// enforces the same rule on the delivered result. This test is what stops the
/// two from drifting: a validator that silently refuses a plan the prompt still
/// invites costs a whole Lead session per occurrence.
#[test]
fn lead_prompt_makes_repair_unavailable_on_a_run_absent_route() {
    let prompt = lead_prompt();
    let section = ci_section(&prompt).expect("CI section present");

    // Keyed on the flag, not on the absent field. Lead is told which routes
    // this applies to by a marker it can see, because "notice that `run_id` is
    // missing" is not an instruction a model can be held to.
    assert!(
        section.contains("marked `diagnose_only`"),
        "the CI section must name the flag the coordinator sets on these routes"
    );
    assert!(
        section.contains("the repair plan is unavailable to you"),
        "the CI section must tell Lead that repair is unavailable on a route \
         that names no run"
    );
    // The reason, not just the rule. Both halves of it: no run was named, and
    // no check set was enumerated.
    assert!(
        section.contains(
            "evidence capture never completed: its `run_id` is null, nothing was ever \
             enumerated, and nothing was attributed to a run"
        ),
        "the CI section must say why repair is unavailable, not merely that it is"
    );
    assert!(
        section.contains(
            "could have been copied from CI evidence, because this route captured \
                          none"
        ),
        "the CI section must connect the unavailability to the verification command"
    );
    // And it must leave the two legal exits open, or the model has been told
    // what it cannot do and not what it should.
    assert!(
        section.contains("Diagnose with `evidence_incomplete`"),
        "the CI section must name the diagnostic reason this route takes"
    );
    assert!(
        section.contains("or park if the capture failure is itself a cited platform dead-end"),
        "the CI section must leave the cited park open as the other legal exit"
    );
}

// ── The two corpora: delivery, and exactness ───────────────────────────────

/// The supervisor grades a Lead result by **string comparison** against two
/// closed corpora — `evidence_references` (exact substring) and
/// `repository_commands` (exact equality after whitespace normalization). The
/// prompt must say that, in those terms.
///
/// # The failure this pins
///
/// On 2026-08-08, 5 of 7 CI adjudications degraded to diagnoses. Four were
/// `verification_command_not_repository_valid`, and not one of them was a
/// hallucinated command: they were `cd server && cargo clippy …`,
/// `SQLX_OFFLINE=true cargo test -p djinn-agent --lib <one test>`, and
/// `UPDATE_DJINN_MCP_SERVER_FIXTURE=1 cargo test -p … -- --nocapture`. Every
/// one names the right verification; every one is a different *string* from
/// anything the corpus could hold.
///
/// "Do not invent a command" — which the prompt already said, and which
/// `command.no_invention` above still pins — does not forbid any of those,
/// because their authors were not inventing. The clause that forbids them is
/// that the comparison is equality, so a decorated command is a new command.
/// This test is what keeps that clause from being edited back out as
/// redundant.
#[test]
fn lead_prompt_requires_the_two_corpora_to_be_copied_verbatim() {
    let prompt = lead_prompt();
    let section = ci_section(&prompt).expect("CI section present");

    // The comparison must be named as string equality. A prompt that only says
    // "repository-valid" leaves "same command, better spelled" open.
    assert!(
        section.contains(
            "string equality against the listed corpus after collapsing runs of \
                          whitespace"
        ),
        "the CI section must state that the command check is string equality, not judgement"
    );
    assert!(
        section.contains("Copy the command character for character"),
        "the CI section must require a character-for-character copy"
    );

    // Each of the four shapes that actually occurred in production must be
    // named. Naming the general rule is not enough: every one of these was
    // written by a Lead that believed it was complying with the general rule.
    for (label, decoration) in [
        (
            "working-directory prefix",
            "`cd server && <command>` is not `<command>`",
        ),
        (
            "environment-variable prefix",
            "`SQLX_OFFLINE=true <command>` or `UPDATE_FIXTURE=1 <command>` is not `<command>`",
        ),
        (
            "narrowed test filter",
            "replacing a workspace-wide run with `-p <crate>`, `--lib`, or a single test-path \
             filter",
        ),
        (
            "flag edits",
            "adding, removing, or reordering any flag, and appending `-- --nocapture`",
        ),
    ] {
        assert!(
            section.contains(decoration),
            "the CI section must name the '{label}' decoration as an invented command"
        );
    }

    // And it must leave a correct exit, or the model picks the closest command
    // rather than the diagnose plan — which is the whole degradation.
    assert!(
        section.contains(
            "reopen with the diagnose plan and `no_repository_command`, put the command you \
             would have run in the `directive` as prose"
        ),
        "the CI section must route an unavailable command to the diagnose plan, with the \
         intended command preserved as prose"
    );

    // Grounding is the same class of check and failed the same way once
    // (`directive_not_grounded`), so it carries the same instruction.
    assert!(
        section.contains("paste one listed evidence reference into it verbatim"),
        "the CI section must require the directive to quote a reference verbatim"
    );
}

/// The corpora must actually **reach** the model.
///
/// # The bug this kills
///
/// Before this fix the two corpora existed only in the arbitration row's
/// `ci_route` block. That block reaches `load_arbiter_directive`, which is
/// hard-gated to the worker role, and its sibling `ci_dossier` — commented "the
/// evidence dossier the Lead prompt renders" — was written to a column no
/// prompt reads. Lead was graded on exact equality against a list it had never
/// been shown, and `lead.md` told it otherwise.
///
/// Every prompt-contract test in this module stayed green throughout, because
/// each one asserts what `lead.md` *says*. This test asserts what the renderer
/// *does*: delete the `{{ci_adjudication_section}}` placeholder from `lead.md`,
/// or the `out.replace` for it in `render_prompt_for_role`, and it fails.
#[test]
fn lead_prompt_renders_the_delivered_ci_evidence_bundle() {
    const BUNDLE: &str = "### CI Evidence Bundle For This Route\n\n\
                          **Evidence references …**\n\n\
                          - `31235016600`\n\n\
                          - `cargo clippy --workspace -- -D warnings`\n";

    let ctx = TaskContext {
        ci_adjudication_bundle: Some(BUNDLE.to_owned()),
        ..make_ctx()
    };
    let prompt = render_prompt(AgentType::Lead, &make_task(), &ctx);

    // The placeholder is substituted, not left as literal template text.
    assert!(
        !prompt.contains("{{ci_adjudication_section}}"),
        "the CI bundle placeholder must be substituted in the rendered prompt"
    );
    // The corpus strings the supervisor compares against are present verbatim.
    for delivered in ["31235016600", "cargo clippy --workspace -- -D warnings"] {
        assert!(
            prompt.contains(delivered),
            "the rendered prompt must carry the corpus entry '{delivered}' verbatim"
        );
    }
    // And inside the CI section, next to the rule they satisfy — a corpus
    // rendered into some other part of the prompt is a corpus the contract
    // paragraph does not point at.
    let section = ci_section(&prompt).expect("CI section present");
    assert!(
        section.contains("### CI Evidence Bundle For This Route"),
        "the bundle must render inside the CI adjudication section"
    );
    assert!(
        section.contains("cargo clippy --workspace -- -D warnings"),
        "the command corpus must render inside the CI adjudication section"
    );

    // The contract paragraph promises this block by name; a rename on either
    // side silently breaks the pointer.
    assert!(
        section.contains("*CI Evidence Bundle For This Route*"),
        "the contract paragraph must name the bundle heading it points Lead at"
    );

    // A Lead session with no CI route renders no bundle and no stray
    // placeholder — the section still has to read cleanly for the ordinary
    // arbiter path.
    let no_route = render_prompt(AgentType::Lead, &make_task(), &make_ctx());
    assert!(
        !no_route.contains("{{ci_adjudication_section}}"),
        "the placeholder must vanish when there is no CI route"
    );
    assert!(
        !no_route.contains("### CI Evidence Bundle For This Route"),
        "an un-routed Lead session must not be shown an empty bundle heading"
    );
    assert!(
        ci_section(&no_route).is_some_and(|s| s.contains("no board change")),
        "the CI section must survive intact when no bundle is delivered"
    );
}

// ── Narrow, cited park ─────────────────────────────────────────────────────

/// Park must be cited and must be denied to the four reopen-instead cases.
#[test]
fn lead_prompt_narrows_park_to_cited_infrastructure_dead_ends() {
    let prompt = lead_prompt();
    let section = ci_section(&prompt).expect("CI section present");

    assert!(
        section.contains("cite the specific evidence in the dossier"),
        "park must require a citation"
    );
    assert!(
        section.contains("These reopen, they do **not** park:"),
        "park must enumerate the cases that reopen instead"
    );
    for reopen_instead in [
        "uncertainty about the cause",
        "an exhausted retry budget",
        "a provider error or a failed provider action",
        "a stale or colliding repository artifact",
    ] {
        assert!(
            section.contains(reopen_instead),
            "'{reopen_instead}' must be listed as reopen-not-park"
        );
    }
}

// ── No dependency mutation ─────────────────────────────────────────────────

/// The general Blocker Discipline section still tells Lead to add blockers on
/// an ordinary reopen. The CI section must carve itself out of that, or the
/// two instructions contradict and the model picks one at random.
#[test]
fn lead_prompt_excludes_ci_adjudication_from_blocker_discipline() {
    let prompt = lead_prompt();
    let section = ci_section(&prompt).expect("CI section present");

    assert!(
        prompt.contains("## Blocker Discipline"),
        "the general blocker-discipline section must still exist"
    );
    assert!(
        section.contains("CI adjudication never adds prerequisite edges."),
        "the CI section must forbid prerequisite edges"
    );
    assert!(
        section.contains(
            "The Blocker Discipline section above applies to ordinary interventions; here, the \
             remedy for a colliding artifact is a reopen directive, not an edge."
        ),
        "the CI section must resolve the conflict with Blocker Discipline explicitly"
    );
}

// ── The section actually reaches the model ─────────────────────────────────

/// The rendered prompt is capped at [`MAX_SYSTEM_PROMPT_CHARS`]; a contract
/// that lands past the cap is a contract the model never reads. The `10qg`
/// planner truncation bug was exactly this, so assert the tail survives.
#[test]
fn lead_ci_contract_survives_the_system_prompt_cap() {
    let prompt = lead_prompt();

    assert!(
        prompt.len() <= MAX_SYSTEM_PROMPT_CHARS,
        "rendered lead prompt is {} chars, over the {MAX_SYSTEM_PROMPT_CHARS} cap",
        prompt.len()
    );
    // The last clause of the whole template, after the CI section.
    assert!(
        prompt.contains("Do not repeat prior strategies."),
        "the tail of lead.md must survive rendering"
    );
    assert!(
        ci_section(&prompt).is_some_and(|s| s.contains("no board change")),
        "the CI section's own tail must survive rendering"
    );
}
