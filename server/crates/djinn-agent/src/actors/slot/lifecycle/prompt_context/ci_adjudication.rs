//! The CI evidence bundle a **Lead** session adjudicates (proposal `nafu`).
//!
//! # The gap this closes
//!
//! Wave 5 gave the supervisor two closed corpora to grade a Lead result
//! against: `evidence_references`, which `is_grounded` requires the directive
//! to quote as an exact substring, and `repository_commands`, which
//! `command_is_repository_valid` requires the `verification_command` to equal
//! after whitespace normalization. Both ride the arbitration row's `ci_route`
//! block.
//!
//! Nothing rendered either one into Lead's prompt. The `ci_route` block reaches
//! only `role_receives_arbiter_directive`, which is hard-gated to `"worker"`,
//! and the sibling `ci_dossier` payload — commented "the evidence dossier the
//! Lead prompt renders" — is written to the `dossier` column and read by the
//! park fallback, never by a prompt. `lead.md` meanwhile told Lead it was given
//! "the repository commands already present in task/project context".
//!
//! So Lead was graded on exact equality against a list it had never seen. On
//! 2026-08-08 that showed up as 5 of 7 adjudications degraded to diagnoses —
//! four of them `verification_command_not_repository_valid`, every one a
//! *semantically correct* command that had been decorated (`cd server && …`,
//! `SQLX_OFFLINE=true …`) or narrowed to a single test path.
//!
//! This module renders the block Lead reads. It is the delivery half of that
//! fix; `lead.md`'s CI section is the obligation half, and
//! `djinn-roles`'s `lead_ci_routing` tests pin the obligation to this format.

/// Render the CI evidence bundle for a Lead prompt.
///
/// `directive` is the arbitration row's structured `directive` column. Returns
/// `None` when it carries no `ci_route` block — an ordinary (non-CI) Lead
/// intervention, or an older coordinator — in which case `lead.md`'s CI section
/// tells Lead the section does not apply.
///
/// Deliberately total over a malformed block: every field degrades to a
/// placeholder rather than suppressing the whole bundle. Suppressing it would
/// silently restore the exact failure this exists to fix, and the supervisor's
/// own `read_arbiter_directive` — which *does* fail closed on a bad field —
/// already refuses to apply anything against such a row.
pub(crate) fn build_ci_adjudication_bundle(
    directive: Option<&serde_json::Value>,
) -> Option<String> {
    let route = directive?.get("ci_route")?;

    let text = |key: &str| {
        route
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let list = |key: &str| -> Vec<&str> {
        route
            .get(key)
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    };

    // `diagnose_only` is the coordinator's explicit flag, not an inference from
    // a null `run_id`. Absent (an older block) reads as "repair is available",
    // which matches what the supervisor does with the same row.
    let diagnose_only = route
        .get("diagnose_only")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let references = list("evidence_references");
    let commands = list("repository_commands");

    let mut out = String::new();
    out.push_str("### CI Evidence Bundle For This Route\n\n");
    out.push_str(&format!(
        "**Lane:** `{}`\\\n\
         **Tier-2 reason:** `{}`\\\n\
         **Origin board state:** `{}`\\\n\
         **PR:** #{}\\\n\
         **PR head SHA:** `{}`\\\n\
         **Run head SHA:** `{}`\\\n\
         **Workflow run ID:** {}\n\n",
        text("lane").unwrap_or("unknown"),
        text("tier2_reason").unwrap_or("unknown"),
        text("origin_state").unwrap_or("unknown"),
        route
            .get("pr_number")
            .and_then(serde_json::Value::as_i64)
            .map_or_else(|| "unknown".to_owned(), |n| n.to_string()),
        text("pr_head_sha").unwrap_or("unknown"),
        text("run_head_sha").unwrap_or("unknown"),
        route
            .get("run_id")
            .and_then(serde_json::Value::as_i64)
            .map_or_else(
                || "none — this route names no run".to_owned(),
                |n| n.to_string()
            ),
    ));
    if let Some(dequeue) = text("dequeue_id") {
        out.push_str(&format!("**Dequeue identity:** `{dequeue}`\n\n"));
    }

    // ── Evidence references ────────────────────────────────────────────────
    out.push_str(
        "**Evidence references — your `directive` must contain at least one of these strings \
         exactly, copied and pasted:**\n\n",
    );
    if references.is_empty() {
        out.push_str(
            "- *(none supplied — this block is malformed; every reopen on this route will be \
             recorded as ungrounded)*\n\n",
        );
    } else {
        for reference in &references {
            out.push_str(&format!("- `{reference}`\n"));
        }
        out.push('\n');
    }

    // ── The command corpus ─────────────────────────────────────────────────
    //
    // The three states are spelled out separately because they have three
    // different correct actions, and collapsing them is what leaves the model
    // to guess. Notably an EMPTY corpus is not "try your best" — it is a
    // decided fact that no repair is possible on this route.
    if diagnose_only {
        out.push_str(
            "**This route is marked `diagnose_only`: the repair plan is unavailable.** No run was \
             named and no blocking check was enumerated, so there is no corpus and no finding for \
             a command to verify. Submit `reopen` with `diagnostic_reason` `evidence_incomplete`, \
             or park a cited platform dead-end.\n",
        );
    } else if commands.is_empty() {
        out.push_str(
            "**No repository-valid verification command exists for this route.** The corpus is \
             empty: no blocking check on this run was reproducible, so there is nothing you could \
             copy. Any `verification_command` you write will be rejected as invented. Submit \
             `reopen` with `diagnostic_reason` `no_repository_command`, put the command you would \
             have run in the `directive` as prose, and leave `verification_command` unset.\n",
        );
    } else {
        out.push_str(&format!(
            "**Repository-valid verification commands — this is the complete corpus ({} \
             total).** A repair's `verification_command` must equal one of these exactly. Copy \
             the whole line; do not prefix it with `cd`, do not prefix it with environment \
             variables, do not narrow it with `-p`/`--lib`/a test filter, and do not chain two of \
             them together. If none of them is the command you want, you do not have a \
             repository-valid command — diagnose with `no_repository_command` instead of \
             approximating.\n\n",
            commands.len()
        ));
        for command in &commands {
            out.push_str(&format!("- `{command}`\n"));
        }
    }

    Some(out)
}

/// Only the Lead role adjudicates a CI route, so only Lead receives the bundle.
///
/// A separate predicate rather than an inline `==` so the gate is greppable
/// next to `role_receives_arbiter_directive`, which is the *opposite* gate on
/// the same database column: the worker gets `directive.directive`, Lead gets
/// `directive.ci_route`, and neither may see the other's.
pub(crate) fn role_receives_ci_adjudication_bundle(role_name: &str) -> bool {
    role_name == "lead"
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The four commands and one reference taken from the 2026-08-08 route
    /// report, so the fixture is the shape production actually produced.
    fn route(commands: serde_json::Value, diagnose_only: bool) -> serde_json::Value {
        json!({
            "kind": "ci_evidence_routing",
            "ci_route": {
                "lane": "pr_head",
                "origin_state": "pr_draft",
                "tier2_reason": "causal_failure",
                "pr_number": 3128,
                "pr_head_sha": "1674a77ade30778c8495db5757aec8c717ffd3ce",
                "run_head_sha": "1674a77ade30778c8495db5757aec8c717ffd3ce",
                "run_id": 31235016600i64,
                "diagnose_only": diagnose_only,
                "evidence_references": ["31235016600", "Server Clippy"],
                "repository_commands": commands,
            }
        })
    }

    /// The corpus the supervisor grades against must appear in the bundle
    /// **verbatim**, because the grading is string equality.
    ///
    /// # The mutation this kills
    ///
    /// Summarising the corpus — "run the repository's Server Clippy command",
    /// which is what `lead.md` alone amounted to — leaves the model to
    /// reconstruct the string. On 2026-08-08 that reconstruction produced
    /// `cd server && cargo clippy …` four times out of five, each rejected as
    /// `verification_command_not_repository_valid`. Drop the loop that writes
    /// each command, or wrap it in prose, and this fails.
    #[test]
    fn the_bundle_reproduces_the_command_corpus_verbatim() {
        let directive = route(
            json!([
                "cargo clippy --workspace --all-targets --features qdrant --keep-going -- -D warnings",
                "cargo test -p djinn-control-plane --lib",
            ]),
            false,
        );
        let bundle = build_ci_adjudication_bundle(Some(&directive)).expect("route present");

        for command in [
            "cargo clippy --workspace --all-targets --features qdrant --keep-going -- -D warnings",
            "cargo test -p djinn-control-plane --lib",
        ] {
            assert!(
                bundle.contains(command),
                "the bundle must carry `{command}` verbatim; got:\n{bundle}"
            );
        }
        // Both grounding handles, likewise verbatim — `is_grounded` is an exact
        // substring test, so a truncated SHA grounds nothing.
        for reference in ["31235016600", "Server Clippy"] {
            assert!(
                bundle.contains(reference),
                "the bundle must carry the evidence reference `{reference}` verbatim"
            );
        }
        // The corpus is declared closed and complete, or "these are examples"
        // is a live reading.
        assert!(
            bundle.contains("this is the complete corpus (2 total)"),
            "the bundle must state the corpus is complete and how large it is"
        );
        // And the decorations that actually occurred are named at the point of
        // delivery, not only in the static prompt.
        assert!(
            bundle.contains("do not prefix it with `cd`"),
            "the bundle must forbid the `cd` prefix beside the corpus it applies to"
        );
    }

    /// An empty corpus is a decided fact — no repair is possible — not an
    /// invitation to improvise. Saying nothing here is what produced a repair
    /// with an invented command instead of a `no_repository_command` diagnosis.
    #[test]
    fn an_empty_corpus_names_the_diagnostic_reason_to_use_instead() {
        let bundle =
            build_ci_adjudication_bundle(Some(&route(json!([]), false))).expect("route present");

        assert!(
            bundle.contains("No repository-valid verification command exists for this route."),
            "an empty corpus must be stated as a fact, not left blank"
        );
        assert!(
            bundle.contains("`no_repository_command`"),
            "an empty corpus must name the diagnostic reason to submit instead"
        );
        assert!(
            !bundle.contains("complete corpus"),
            "an empty corpus must not render a command list header"
        );
    }

    /// A `diagnose_only` route must say so in the bundle: the supervisor
    /// rejects a repair on it regardless, so an inference that went the other
    /// way costs a whole Lead session.
    #[test]
    fn a_diagnose_only_route_states_that_repair_is_unavailable() {
        let bundle = build_ci_adjudication_bundle(Some(&route(json!(["cargo test"]), true)))
            .expect("route present");

        assert!(
            bundle.contains("marked `diagnose_only`: the repair plan is unavailable"),
            "a diagnose_only route must say repair is unavailable"
        );
        assert!(
            bundle.contains("`evidence_incomplete`"),
            "a diagnose_only route must name its diagnostic reason"
        );
        // No corpus is offered, because no repair may use one.
        assert!(
            !bundle.contains("complete corpus"),
            "a diagnose_only route must not offer a command corpus"
        );
    }

    /// No `ci_route` block is an ordinary arbiter intervention: no bundle, and
    /// therefore no CI section content contradicting the general matrix.
    #[test]
    fn a_directive_without_a_ci_route_yields_no_bundle() {
        assert!(build_ci_adjudication_bundle(None).is_none());
        assert!(build_ci_adjudication_bundle(Some(&json!({}))).is_none());
        assert!(
            build_ci_adjudication_bundle(Some(&json!({
                "decision": "reopen",
                "directive": "fix the thing",
            })))
            .is_none(),
            "a plain monitored-reopen directive is not a CI route"
        );
    }

    /// Lead adjudicates; nobody else sees the route corpus.
    ///
    /// The worker's own gate (`role_receives_arbiter_directive`) reads the
    /// sibling key on the same column, so a gate that answered `true` for both
    /// roles would hand a worker the adjudication corpus and Lead the worker's
    /// one-shot directive.
    #[test]
    fn only_lead_receives_the_ci_adjudication_bundle() {
        assert!(role_receives_ci_adjudication_bundle("lead"));
        for other in ["worker", "planner", "reviewer", "architect", "verification"] {
            assert!(
                !role_receives_ci_adjudication_bundle(other),
                "{other} must not receive the CI adjudication bundle"
            );
        }
    }

    /// The bundle renders no `## ` heading. One would split `lead.md`'s CI
    /// Failure Adjudication section at render time and silently truncate the
    /// contract that follows the injection point — including the atomic-guard
    /// paragraph. `djinn-roles`'s `ci_section` slices on exactly that token.
    #[test]
    fn the_bundle_cannot_split_the_ci_section() {
        let bundle =
            build_ci_adjudication_bundle(Some(&route(json!(["cargo test -p djinn-db"]), false)))
                .expect("route present");

        assert!(
            !bundle.contains("\n## "),
            "the bundle must use sub-headings only; a `## ` would truncate the CI section"
        );
        assert!(
            bundle.starts_with("### "),
            "the bundle must open with a sub-heading"
        );
    }
}
