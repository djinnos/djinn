//! The one discriminator that means "this task keeps the legacy task-PR
//! delivery route".
//!
//! `pr_url` is **not** that discriminator. It is a nullable field written by
//! whichever PR-open body ran last, and a stray value on a canonically
//! direct-owned task must never re-route it back onto the legacy path — doing
//! so releases dependents on work no ledger generation ever applied.
//!
//! The coordinator's `admit_direct_delivery` already routes on the explicit
//! label; these constants are the SQL-side form of the same predicate so the
//! two sides cannot disagree about what "legacy" means.

/// Label a task carries when it is pinned to the legacy task-PR route.
///
/// `djinn-coordinator::direct_delivery::LEGACY_DELIVERY_LABEL` re-exports this
/// constant rather than declaring its own, so there is exactly one definition.
pub const LEGACY_DELIVERY_LABEL: &str = "direct-delivery-legacy";

/// SQL fragment form of [`LEGACY_DELIVERY_LABEL`], written without a table
/// alias so each call site prefixes its own (`bt.`, `t.`, `tasks.`).
///
/// `@>` rather than `?` deliberately: the `merged` classification is spliced
/// into a dynamically numbered `$n` clause list, and a bare `?` in that context
/// reads as a placeholder to anything that later counts them.
/// Referenced only by the structural guard below — the three query strings
/// embed the fragment literally so they stay plain `const` SQL.
#[cfg(test)]
pub(crate) const EXPLICITLY_LEGACY_LABELS_SQL: &str =
    r#"labels @> '["direct-delivery-legacy"]'::jsonb"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// The SQL fragment must name the same label the coordinator routes on. A
    /// typo here would silently classify every task as direct.
    #[test]
    fn the_sql_fragment_names_the_canonical_legacy_label() {
        assert_eq!(
            EXPLICITLY_LEGACY_LABELS_SQL,
            format!(r#"labels @> '["{LEGACY_DELIVERY_LABEL}"]'::jsonb"#)
        );
    }

    /// Structural guard for the three ledger-side consumers of the
    /// discriminator. Each must route on the label and none may reintroduce
    /// `pr_url IS NULL`, which is the divergence that let a direct task with a
    /// stray PR URL release its dependents.
    #[test]
    fn no_ledger_side_consumer_routes_on_a_nullable_pr_url() {
        for (name, sql) in [
            (
                "emit_unblocked_tasks",
                crate::repositories::task::blockers::EMIT_UNBLOCKED_TASKS_SQL,
            ),
            (
                "board_health direct section",
                crate::repositories::task::board_health::DIRECT_DELIVERY_SECTION_SQL,
            ),
            (
                "merged classification",
                crate::repositories::task::queries::MERGED_PSEUDO_STATUS_SQL,
            ),
        ] {
            assert!(
                sql.contains(EXPLICITLY_LEGACY_LABELS_SQL),
                "{name} must route on the explicit legacy label"
            );
            assert!(
                !sql.contains("pr_url IS NULL"),
                "{name} must not reintroduce the nullable-pr_url discriminator"
            );
        }
    }
}
