//! Migration 141 — task creator contract.
//!
//! Verifies the transactional backfill migration that fills every NULL
//! `tasks.created_by_user_id` using deterministic typed precedence and then
//! contracts the column to NOT NULL. The migration requires an explicit
//! designated operator validated against `users` before any data change.
//!
//! Required fixture cases (see the research spike
//! `mandatory-designated-operator-for-the-postgresql-creator-contract-migration`):
//! - Unset and blank designated operator input → fails before writes, column
//!   remains nullable, no migration row.
//! - Invalid designated operator → fails even with zero residue / zero tasks.
//! - Valid disabled retained designated user → accepted.
//! - Source-task creator wins over epic / proposal / designated.
//! - Source creator NULL, source user missing, dangling link, ambiguous
//!   multiple typed links all advance.
//! - Epic creator wins when source is unavailable; missing/deleted epic
//!   advances.
//! - Proposal build owner wins over author; missing/dangling build owner
//!   advances to valid author; invalid proposal link advances.
//! - Creator-less chain lands on the exact validated designated operator.
//! - Existing non-NULL creators (including disabled retained users) unchanged.
//! - Rollback: forced assertion failure restores the nullable column.
//! - Idempotent data step.
//! - Catalog non-nullability and direct SQL NULL rejection.
//! - Deleting a referenced user fails under NOT NULL + FK.

#[path = "support/migrations_task_creator_contract.rs"]
mod migrations_task_creator_contract_support;

use djinn_db::migrations::MigrationContext;
use migrations_task_creator_contract_support::*;
use sqlx::postgres::PgConnection;
use sqlx::{Connection, Executor};
// ═══════════════════════════════════════════════════════════════════════════
// Preflight failure cases
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn unset_operator_aborts_before_writes_with_zero_residue() {
    with_temp_database("unset_op", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect fresh database");
        apply_prior_migrations(&mut conn).await;
        clear_operator(&mut conn).await;

        let sql = migration_sql();
        let err = conn
            .execute(sql.as_str())
            .await
            .expect_err("unset operator must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("creator_contract_designated_operator_unset"),
            "expected unset marker, got: {msg}"
        );
        assert!(
            column_is_nullable(&mut conn).await,
            "column should remain nullable after preflight failure"
        );
        conn.close().await.expect("close");
    })
    .await;
}

#[tokio::test]
async fn invalid_operator_aborts_before_writes_even_with_zero_tasks() {
    with_temp_database("invalid_op", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect fresh database");
        apply_prior_migrations(&mut conn).await;
        set_operator(&mut conn, "nonexistent-user-id").await;

        let sql = migration_sql();
        let err = conn
            .execute(sql.as_str())
            .await
            .expect_err("invalid operator must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("creator_contract_designated_operator_invalid"),
            "expected invalid marker, got: {msg}"
        );
        assert!(
            column_is_nullable(&mut conn).await,
            "column should remain nullable after invalid operator"
        );
        conn.close().await.expect("close");
    })
    .await;
}

// ═══════════════════════════════════════════════════════════════════════════
// Disabled retained designated operator
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn disabled_retained_designated_operator_is_accepted() {
    with_temp_database("disabled_op", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect fresh database");
        apply_prior_migrations(&mut conn).await;

        seed_project(&mut conn, "project-1").await;
        seed_user(&mut conn, DESIGNATED, true).await;
        seed_task(&mut conn, "task-1", "project-1", None).await;

        set_operator(&mut conn, DESIGNATED).await;
        apply_contract_migration(&mut conn).await;

        assert_eq!(
            get_task_creator(&mut conn, "task-1").await,
            Some(DESIGNATED.to_owned()),
            "disabled retained operator should be accepted as residue"
        );
        assert!(
            !column_is_nullable(&mut conn).await,
            "column should be NOT NULL after migration"
        );
        conn.close().await.expect("close");
    })
    .await;
}

// ═══════════════════════════════════════════════════════════════════════════
// Source-task precedence
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn source_task_creator_wins_over_epic_proposal_designated() {
    with_temp_database("source_wins", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect fresh database");
        apply_prior_migrations(&mut conn).await;

        seed_project(&mut conn, "project-1").await;
        seed_user(&mut conn, "user-source", false).await;
        seed_user(&mut conn, "user-epic", false).await;
        seed_user(&mut conn, "user-build-owner", false).await;
        seed_user(&mut conn, DESIGNATED, false).await;

        seed_task(&mut conn, "task-source", "project-1", Some("user-source")).await;
        seed_proposal(&mut conn, "prop-1", Some("user-build-owner"), None).await;
        seed_epic_with_proposal(
            &mut conn,
            "epic-1",
            "project-1",
            Some("prop-1"),
            Some("user-epic"),
        )
        .await;
        seed_task_with_epic(
            &mut conn,
            "task-target",
            "project-1",
            Some("epic-1"),
            None,
            None,
        )
        .await;
        seed_audit_source_link(&mut conn, "task-target", "task-source", "1").await;

        set_operator(&mut conn, DESIGNATED).await;
        apply_contract_migration(&mut conn).await;

        assert_eq!(
            get_task_creator(&mut conn, "task-target").await,
            Some("user-source".to_owned()),
            "source-task creator should win over all other tiers"
        );
        conn.close().await.expect("close");
    })
    .await;
}

#[tokio::test]
async fn source_task_creator_null_advances_to_epic() {
    with_temp_database("src_null", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect fresh database");
        apply_prior_migrations(&mut conn).await;

        seed_project(&mut conn, "project-1").await;
        seed_user(&mut conn, "user-epic", false).await;
        seed_user(&mut conn, DESIGNATED, false).await;

        seed_task(&mut conn, "task-source", "project-1", None).await;
        seed_epic(&mut conn, "epic-1", "project-1", Some("user-epic")).await;
        seed_task_with_epic(
            &mut conn,
            "task-target",
            "project-1",
            Some("epic-1"),
            None,
            None,
        )
        .await;
        seed_audit_source_link(&mut conn, "task-target", "task-source", "1").await;

        set_operator(&mut conn, DESIGNATED).await;
        apply_contract_migration(&mut conn).await;

        assert_eq!(
            get_task_creator(&mut conn, "task-target").await,
            Some("user-epic".to_owned()),
            "NULL source creator should advance to epic creator"
        );
        conn.close().await.expect("close");
    })
    .await;
}

#[tokio::test]
async fn source_user_missing_advances_to_epic() {
    with_temp_database("src_missing", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect fresh database");
        apply_prior_migrations(&mut conn).await;

        seed_project(&mut conn, "project-1").await;
        seed_user(&mut conn, "user-epic", false).await;
        seed_user(&mut conn, DESIGNATED, false).await;
        seed_user(&mut conn, "user-source-orig", false).await;

        // Source task with a valid creator initially.
        seed_task(
            &mut conn,
            "task-source",
            "project-1",
            Some("user-source-orig"),
        )
        .await;
        seed_epic(&mut conn, "epic-1", "project-1", Some("user-epic")).await;
        seed_task_with_epic(
            &mut conn,
            "task-target",
            "project-1",
            Some("epic-1"),
            None,
            None,
        )
        .await;
        seed_audit_source_link(&mut conn, "task-target", "task-source", "1").await;

        // Delete the source user — FK ON DELETE SET NULL nulls the source creator.
        sqlx::query("DELETE FROM users WHERE id = 'user-source-orig'")
            .execute(&mut conn)
            .await
            .expect("delete source user");

        set_operator(&mut conn, DESIGNATED).await;
        apply_contract_migration(&mut conn).await;

        assert_eq!(
            get_task_creator(&mut conn, "task-target").await,
            Some("user-epic".to_owned()),
            "missing source user should advance to epic creator"
        );
        conn.close().await.expect("close");
    })
    .await;
}

#[tokio::test]
async fn dangling_audit_source_link_advances() {
    with_temp_database("src_dangling", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect fresh database");
        apply_prior_migrations(&mut conn).await;

        seed_project(&mut conn, "project-1").await;
        seed_user(&mut conn, DESIGNATED, false).await;

        seed_task(&mut conn, "task-target", "project-1", None).await;
        seed_audit_source_link(&mut conn, "task-target", "task-deleted", "1").await;

        set_operator(&mut conn, DESIGNATED).await;
        apply_contract_migration(&mut conn).await;

        assert_eq!(
            get_task_creator(&mut conn, "task-target").await,
            Some(DESIGNATED.to_owned()),
            "dangling audit link should advance to designated operator"
        );
        conn.close().await.expect("close");
    })
    .await;
}

#[tokio::test]
async fn ambiguous_source_links_advance() {
    with_temp_database("src_ambiguous", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect fresh database");
        apply_prior_migrations(&mut conn).await;

        seed_project(&mut conn, "project-1").await;
        seed_user(&mut conn, DESIGNATED, false).await;
        seed_user(&mut conn, "user-a", false).await;
        seed_user(&mut conn, "user-b", false).await;

        seed_task(&mut conn, "task-source-a", "project-1", Some("user-a")).await;
        seed_task(&mut conn, "task-source-b", "project-1", Some("user-b")).await;
        seed_task(&mut conn, "task-target", "project-1", None).await;

        seed_audit_source_link(&mut conn, "task-target", "task-source-a", "a").await;
        seed_audit_source_link(&mut conn, "task-target", "task-source-b", "b").await;

        set_operator(&mut conn, DESIGNATED).await;
        apply_contract_migration(&mut conn).await;

        assert_eq!(
            get_task_creator(&mut conn, "task-target").await,
            Some(DESIGNATED.to_owned()),
            "ambiguous source links should advance, not guess"
        );
        conn.close().await.expect("close");
    })
    .await;
}

#[tokio::test]
async fn valid_source_survives_alongside_dangling_source_link() {
    with_temp_database("src_valid_dangling", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect fresh database");
        apply_prior_migrations(&mut conn).await;

        seed_project(&mut conn, "project-1").await;
        seed_user(&mut conn, "user-source-valid", false).await;
        seed_user(&mut conn, "user-epic", false).await;
        seed_user(&mut conn, DESIGNATED, false).await;

        // A valid source task with a valid creator.
        seed_task(
            &mut conn,
            "task-source-valid",
            "project-1",
            Some("user-source-valid"),
        )
        .await;
        seed_epic(&mut conn, "epic-1", "project-1", Some("user-epic")).await;
        seed_task_with_epic(
            &mut conn,
            "task-target",
            "project-1",
            Some("epic-1"),
            None,
            None,
        )
        .await;

        // Valid audit link.
        seed_audit_source_link(&mut conn, "task-target", "task-source-valid", "v").await;
        // Dangling audit link — source task does not exist.
        seed_audit_source_link(&mut conn, "task-target", "task-source-deleted", "d").await;

        set_operator(&mut conn, DESIGNATED).await;
        apply_contract_migration(&mut conn).await;

        assert_eq!(
            get_task_creator(&mut conn, "task-target").await,
            Some("user-source-valid".to_owned()),
            "valid source creator must win even when a dangling source link is present"
        );
        conn.close().await.expect("close");
    })
    .await;
}

#[tokio::test]
async fn valid_source_survives_alongside_null_creator_source_link() {
    with_temp_database("src_valid_null", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect fresh database");
        apply_prior_migrations(&mut conn).await;

        seed_project(&mut conn, "project-1").await;
        seed_user(&mut conn, "user-source-valid", false).await;
        seed_user(&mut conn, "user-epic", false).await;
        seed_user(&mut conn, DESIGNATED, false).await;

        // Valid source task with a valid creator.
        seed_task(
            &mut conn,
            "task-source-valid",
            "project-1",
            Some("user-source-valid"),
        )
        .await;
        // Source task that exists but has a NULL creator.
        seed_task(&mut conn, "task-source-null", "project-1", None).await;
        seed_epic(&mut conn, "epic-1", "project-1", Some("user-epic")).await;
        seed_task_with_epic(
            &mut conn,
            "task-target",
            "project-1",
            Some("epic-1"),
            None,
            None,
        )
        .await;

        // Valid audit link.
        seed_audit_source_link(&mut conn, "task-target", "task-source-valid", "v").await;
        // Audit link to a source task that exists but has NULL creator.
        seed_audit_source_link(&mut conn, "task-target", "task-source-null", "n").await;

        set_operator(&mut conn, DESIGNATED).await;
        apply_contract_migration(&mut conn).await;

        assert_eq!(
            get_task_creator(&mut conn, "task-target").await,
            Some("user-source-valid".to_owned()),
            "valid source creator must win even when a second source has NULL creator"
        );
        conn.close().await.expect("close");
    })
    .await;
}

#[tokio::test]
async fn remediation_hold_source_link_wins() {
    with_temp_database("hold_src", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect fresh database");
        apply_prior_migrations(&mut conn).await;

        seed_project(&mut conn, "project-1").await;
        seed_user(&mut conn, "user-source", false).await;
        seed_user(&mut conn, DESIGNATED, false).await;

        seed_task(&mut conn, "task-source", "project-1", Some("user-source")).await;
        seed_task_with_epic(
            &mut conn,
            "task-target",
            "project-1",
            None,
            None,
            Some(r#"["human-review-hold"]"#),
        )
        .await;
        seed_hold_blocker_link(&mut conn, "task-target", "task-source").await;

        set_operator(&mut conn, DESIGNATED).await;
        apply_contract_migration(&mut conn).await;

        assert_eq!(
            get_task_creator(&mut conn, "task-target").await,
            Some("user-source".to_owned()),
            "human-review-hold blocker source creator should win"
        );
        conn.close().await.expect("close");
    })
    .await;
}

#[tokio::test]
async fn planner_park_escalation_source_link_wins() {
    with_temp_database("park_src", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect fresh database");
        apply_prior_migrations(&mut conn).await;

        seed_project(&mut conn, "project-1").await;
        seed_user(&mut conn, "user-source", false).await;
        seed_user(&mut conn, DESIGNATED, false).await;

        seed_task(&mut conn, "task-source", "project-1", Some("user-source")).await;
        seed_task_with_epic(
            &mut conn,
            "task-target",
            "project-1",
            None,
            None,
            Some(r#"["planner-park-escalation"]"#),
        )
        .await;
        seed_hold_blocker_link(&mut conn, "task-target", "task-source").await;

        set_operator(&mut conn, DESIGNATED).await;
        apply_contract_migration(&mut conn).await;

        assert_eq!(
            get_task_creator(&mut conn, "task-target").await,
            Some("user-source".to_owned()),
            "planner-park-escalation blocker source creator should win"
        );
        conn.close().await.expect("close");
    })
    .await;
}

#[tokio::test]
async fn non_hold_blocker_edge_is_not_source() {
    with_temp_database("nonhold_blk", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect fresh database");
        apply_prior_migrations(&mut conn).await;

        seed_project(&mut conn, "project-1").await;
        seed_user(&mut conn, "user-source", false).await;
        seed_user(&mut conn, DESIGNATED, false).await;

        seed_task(&mut conn, "task-source", "project-1", Some("user-source")).await;
        seed_task(&mut conn, "task-target", "project-1", None).await;
        seed_hold_blocker_link(&mut conn, "task-target", "task-source").await;

        set_operator(&mut conn, DESIGNATED).await;
        apply_contract_migration(&mut conn).await;

        assert_eq!(
            get_task_creator(&mut conn, "task-target").await,
            Some(DESIGNATED.to_owned()),
            "non-hold blocker edge should not be source provenance"
        );
        conn.close().await.expect("close");
    })
    .await;
}

// ═══════════════════════════════════════════════════════════════════════════
// Epic precedence
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn epic_creator_wins_when_source_unavailable() {
    with_temp_database("epic_wins", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect fresh database");
        apply_prior_migrations(&mut conn).await;

        seed_project(&mut conn, "project-1").await;
        seed_user(&mut conn, "user-epic", false).await;
        seed_user(&mut conn, DESIGNATED, false).await;

        seed_epic(&mut conn, "epic-1", "project-1", Some("user-epic")).await;
        seed_task_with_epic(
            &mut conn,
            "task-target",
            "project-1",
            Some("epic-1"),
            None,
            None,
        )
        .await;

        set_operator(&mut conn, DESIGNATED).await;
        apply_contract_migration(&mut conn).await;

        assert_eq!(
            get_task_creator(&mut conn, "task-target").await,
            Some("user-epic".to_owned()),
            "epic creator should win when source is unavailable"
        );
        conn.close().await.expect("close");
    })
    .await;
}

#[tokio::test]
async fn missing_epic_advances_to_designated() {
    with_temp_database("epic_missing", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect fresh database");
        apply_prior_migrations(&mut conn).await;

        seed_project(&mut conn, "project-1").await;
        seed_user(&mut conn, DESIGNATED, false).await;
        seed_task(&mut conn, "task-target", "project-1", None).await;

        set_operator(&mut conn, DESIGNATED).await;
        apply_contract_migration(&mut conn).await;

        assert_eq!(
            get_task_creator(&mut conn, "task-target").await,
            Some(DESIGNATED.to_owned()),
            "missing epic should advance to designated operator"
        );
        conn.close().await.expect("close");
    })
    .await;
}

#[tokio::test]
async fn epic_creator_deleted_advances_to_designated() {
    with_temp_database("epic_del", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect fresh database");
        apply_prior_migrations(&mut conn).await;

        seed_project(&mut conn, "project-1").await;
        seed_user(&mut conn, DESIGNATED, false).await;
        seed_user(&mut conn, "user-epic-orig", false).await;

        // Epic with a valid creator initially.
        seed_epic(&mut conn, "epic-1", "project-1", Some("user-epic-orig")).await;
        seed_task_with_epic(
            &mut conn,
            "task-target",
            "project-1",
            Some("epic-1"),
            None,
            None,
        )
        .await;

        // Delete the epic creator — FK ON DELETE SET NULL nulls the epic creator.
        sqlx::query("DELETE FROM users WHERE id = 'user-epic-orig'")
            .execute(&mut conn)
            .await
            .expect("delete epic user");

        set_operator(&mut conn, DESIGNATED).await;
        apply_contract_migration(&mut conn).await;

        assert_eq!(
            get_task_creator(&mut conn, "task-target").await,
            Some(DESIGNATED.to_owned()),
            "deleted epic creator should advance to designated operator"
        );
        conn.close().await.expect("close");
    })
    .await;
}

// ═══════════════════════════════════════════════════════════════════════════
// Proposal precedence
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn proposal_build_owner_wins_over_author() {
    with_temp_database("build_wins", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect fresh database");
        apply_prior_migrations(&mut conn).await;

        seed_project(&mut conn, "project-1").await;
        seed_user(&mut conn, "user-build-owner", false).await;
        seed_user(&mut conn, "user-author", false).await;
        seed_user(&mut conn, DESIGNATED, false).await;

        seed_proposal(
            &mut conn,
            "prop-1",
            Some("user-build-owner"),
            Some("user-author"),
        )
        .await;
        seed_epic_with_proposal(&mut conn, "epic-1", "project-1", Some("prop-1"), None).await;
        seed_task_with_epic(
            &mut conn,
            "task-target",
            "project-1",
            Some("epic-1"),
            None,
            None,
        )
        .await;

        set_operator(&mut conn, DESIGNATED).await;
        apply_contract_migration(&mut conn).await;

        assert_eq!(
            get_task_creator(&mut conn, "task-target").await,
            Some("user-build-owner".to_owned()),
            "proposal build owner should win over author"
        );
        conn.close().await.expect("close");
    })
    .await;
}

#[tokio::test]
async fn missing_build_owner_advances_to_author() {
    with_temp_database("author_fb", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect fresh database");
        apply_prior_migrations(&mut conn).await;

        seed_project(&mut conn, "project-1").await;
        seed_user(&mut conn, "user-author", false).await;
        seed_user(&mut conn, DESIGNATED, false).await;

        seed_proposal(
            &mut conn,
            "prop-1",
            Some("user-ghost-build"),
            Some("user-author"),
        )
        .await;
        seed_epic_with_proposal(&mut conn, "epic-1", "project-1", Some("prop-1"), None).await;
        seed_task_with_epic(
            &mut conn,
            "task-target",
            "project-1",
            Some("epic-1"),
            None,
            None,
        )
        .await;

        set_operator(&mut conn, DESIGNATED).await;
        apply_contract_migration(&mut conn).await;

        assert_eq!(
            get_task_creator(&mut conn, "task-target").await,
            Some("user-author".to_owned()),
            "missing build owner should advance to proposal author"
        );
        conn.close().await.expect("close");
    })
    .await;
}

#[tokio::test]
async fn invalid_proposal_link_advances_to_designated() {
    with_temp_database("inv_prop", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect fresh database");
        apply_prior_migrations(&mut conn).await;

        seed_project(&mut conn, "project-1").await;
        seed_user(&mut conn, DESIGNATED, false).await;

        // build_owner has no FK so a non-existent value is allowed directly.
        // author_user_id has an FK (migration 65) so it must be NULL or valid.
        seed_proposal(&mut conn, "prop-1", Some("user-ghost-build"), None).await;
        seed_epic_with_proposal(&mut conn, "epic-1", "project-1", Some("prop-1"), None).await;
        seed_task_with_epic(
            &mut conn,
            "task-target",
            "project-1",
            Some("epic-1"),
            None,
            None,
        )
        .await;

        set_operator(&mut conn, DESIGNATED).await;
        apply_contract_migration(&mut conn).await;

        assert_eq!(
            get_task_creator(&mut conn, "task-target").await,
            Some(DESIGNATED.to_owned()),
            "invalid proposal users should advance to designated"
        );
        conn.close().await.expect("close");
    })
    .await;
}

// ═══════════════════════════════════════════════════════════════════════════
// Residue and existing creators
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn creatorless_chain_lands_on_designated_operator() {
    with_temp_database("residue", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect fresh database");
        apply_prior_migrations(&mut conn).await;

        seed_project(&mut conn, "project-1").await;
        seed_user(&mut conn, DESIGNATED, false).await;
        seed_task(&mut conn, "task-residue", "project-1", None).await;

        set_operator(&mut conn, DESIGNATED).await;
        apply_contract_migration(&mut conn).await;

        assert_eq!(
            get_task_creator(&mut conn, "task-residue").await,
            Some(DESIGNATED.to_owned()),
            "residue task should land on designated operator"
        );
        conn.close().await.expect("close");
    })
    .await;
}

#[tokio::test]
async fn existing_nonnull_creator_is_unchanged() {
    with_temp_database("exist_unchg", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect fresh database");
        apply_prior_migrations(&mut conn).await;

        seed_project(&mut conn, "project-1").await;
        seed_user(&mut conn, "user-existing", false).await;
        seed_user(&mut conn, DESIGNATED, false).await;
        seed_task(
            &mut conn,
            "task-existing",
            "project-1",
            Some("user-existing"),
        )
        .await;

        set_operator(&mut conn, DESIGNATED).await;
        apply_contract_migration(&mut conn).await;

        assert_eq!(
            get_task_creator(&mut conn, "task-existing").await,
            Some("user-existing".to_owned()),
            "existing non-NULL creator must be preserved"
        );
        conn.close().await.expect("close");
    })
    .await;
}

#[tokio::test]
async fn disabled_retained_existing_creator_is_unchanged() {
    with_temp_database("dis_unchg", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect fresh database");
        apply_prior_migrations(&mut conn).await;

        seed_project(&mut conn, "project-1").await;
        seed_user(&mut conn, "user-disabled-existing", true).await;
        seed_user(&mut conn, DESIGNATED, false).await;
        seed_task(
            &mut conn,
            "task-disabled",
            "project-1",
            Some("user-disabled-existing"),
        )
        .await;

        set_operator(&mut conn, DESIGNATED).await;
        apply_contract_migration(&mut conn).await;

        assert_eq!(
            get_task_creator(&mut conn, "task-disabled").await,
            Some("user-disabled-existing".to_owned()),
            "disabled retained existing creator must be preserved"
        );
        conn.close().await.expect("close");
    })
    .await;
}

// ═══════════════════════════════════════════════════════════════════════════
// Rollback, assertion ordering, and direct SQL NULL rejection
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn rollback_on_preflight_failure_leaves_column_nullable() {
    with_temp_database("rollback", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect fresh database");
        apply_prior_migrations(&mut conn).await;

        // Zero tasks — purely a preflight failure.
        clear_operator(&mut conn).await;

        let sql = migration_sql();
        let err = conn
            .execute(sql.as_str())
            .await
            .expect_err("unset operator must fail");
        assert!(
            err.to_string()
                .contains("creator_contract_designated_operator_unset"),
            "expected unset marker"
        );
        assert!(
            column_is_nullable(&mut conn).await,
            "column should remain nullable after preflight failure"
        );
        conn.close().await.expect("close");
    })
    .await;
}

#[tokio::test]
async fn forced_post_update_failure_rolls_back_data_and_schema() {
    with_temp_database("rb_forced", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect fresh database");
        apply_prior_migrations(&mut conn).await;

        seed_project(&mut conn, "project-1").await;
        seed_user(&mut conn, DESIGNATED, false).await;
        // task-rb starts NULL so the data step will update it.
        seed_task(&mut conn, "task-rb", "project-1", None).await;

        set_operator(&mut conn, DESIGNATED).await;

        // Build a migration variant that runs the real preflight, the real
        // data step, then a forced failure BEFORE the contraction step.
        // This proves the data UPDATE is rolled back together with the
        // schema change on any failure between them.
        let full_sql = migration_sql();
        let data_step = migration_data_step_sql();
        // The preflight DO block is everything before the data step.
        let preflight_end = full_sql
            .find("WITH valid_source AS (")
            .expect("find data step start");
        let preflight_sql = full_sql[..preflight_end].trim();

        let mut tx = conn.begin().await.expect("begin transaction");

        // Preflight.
        tx.execute(preflight_sql).await.expect("preflight succeeds");
        // Real data step.
        tx.execute(data_step.as_str())
            .await
            .expect("data step succeeds");
        // Verify the data step updated the task inside the tx.
        let updated: Option<String> =
            sqlx::query_scalar("SELECT created_by_user_id FROM tasks WHERE id = 'task-rb'")
                .fetch_one(&mut *tx)
                .await
                .expect("check updated value");
        assert_eq!(
            updated,
            Some(DESIGNATED.to_owned()),
            "data step should have filled the task within the transaction"
        );
        // Forced failure: an invalid statement.
        let forced = tx.execute("SELECT 1 / 0").await;
        assert!(forced.is_err(), "forced failure should raise an error");

        // Roll back the transaction.
        drop(tx);

        // After rollback: task value restored to NULL, column still nullable.
        assert_eq!(
            get_task_creator(&mut conn, "task-rb").await,
            None,
            "task value should be restored to NULL after rollback"
        );
        assert!(
            column_is_nullable(&mut conn).await,
            "column should remain nullable after rollback"
        );
        // No temp CHECK constraint leaked.
        let constraint_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_constraint \
             WHERE conname = 'tasks_created_by_user_id_not_null_check'",
        )
        .fetch_one(&mut conn)
        .await
        .expect("check temp constraint absent");
        assert_eq!(
            constraint_count, 0,
            "no temp CHECK constraint should remain after rollback"
        );

        // A corrected run must apply the complete real migration after the
        // failed transaction: it refills the restored NULL and contracts the
        // catalog. This is the retry surface SQLx uses after a failed file.
        apply_contract_migration(&mut conn).await;
        assert_eq!(
            get_task_creator(&mut conn, "task-rb").await,
            Some(DESIGNATED.to_owned()),
            "successful retry should refill the creator after rollback"
        );
        assert!(
            !column_is_nullable(&mut conn).await,
            "successful retry should contract the creator column"
        );

        conn.close().await.expect("close");
    })
    .await;
}

#[tokio::test]
async fn zero_null_assertion_precedes_schema_contraction_and_rejects_null() {
    with_temp_database("assert_order", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect fresh database");
        apply_prior_migrations(&mut conn).await;

        seed_project(&mut conn, "project-1").await;
        seed_user(&mut conn, DESIGNATED, false).await;
        seed_task(&mut conn, "task-1", "project-1", None).await;

        set_operator(&mut conn, DESIGNATED).await;
        apply_contract_migration(&mut conn).await;

        assert!(
            !column_is_nullable(&mut conn).await,
            "column should be NOT NULL after successful migration"
        );
        assert_eq!(
            get_task_creator(&mut conn, "task-1").await,
            Some(DESIGNATED.to_owned())
        );

        // Direct SQL INSERT with NULL must fail.
        let insert_err = sqlx::query(
            "INSERT INTO tasks \
             (id, project_id, short_id, title, description, design, \
              labels, acceptance_criteria, memory_refs, created_by_user_id) \
             VALUES ('task-null', 'project-1', 'sn', 't', 'd', 'dd', \
                     '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, NULL)",
        )
        .execute(&mut conn)
        .await;
        assert!(
            insert_err.is_err(),
            "direct SQL INSERT with NULL creator must fail under NOT NULL"
        );

        // Direct SQL UPDATE to NULL must fail.
        let update_err =
            sqlx::query("UPDATE tasks SET created_by_user_id = NULL WHERE id = 'task-1'")
                .execute(&mut conn)
                .await;
        assert!(
            update_err.is_err(),
            "direct SQL UPDATE to NULL creator must fail under NOT NULL"
        );

        conn.close().await.expect("close");
    })
    .await;
}

// ═══════════════════════════════════════════════════════════════════════════
// Idempotence
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn data_step_is_idempotent() {
    with_temp_database("idem", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect fresh database");
        apply_prior_migrations(&mut conn).await;

        seed_project(&mut conn, "project-1").await;
        seed_user(&mut conn, DESIGNATED, false).await;
        seed_task(&mut conn, "task-1", "project-1", None).await;

        set_operator(&mut conn, DESIGNATED).await;
        apply_contract_migration(&mut conn).await;

        let creator_after_first = get_task_creator(&mut conn, "task-1").await;
        assert_eq!(
            creator_after_first,
            Some(DESIGNATED.to_owned()),
            "task should be filled after first migration run"
        );

        // Rerun the REAL data step CTE+UPDATE from the migration file (not a
        // synthetic substitute). Because there are no NULL creators left,
        // it must affect zero rows and leave the value unchanged.
        let data_step = migration_data_step_sql();
        let affected: i64 = conn
            .execute(data_step.as_str())
            .await
            .map(|r| r.rows_affected() as i64)
            .expect("data step rerun should succeed");
        assert_eq!(
            affected, 0,
            "re-running the real data step should affect zero rows (idempotent)"
        );

        assert_eq!(
            get_task_creator(&mut conn, "task-1").await,
            creator_after_first,
            "creator must be unchanged after idempotent rerun"
        );

        conn.close().await.expect("close");
    })
    .await;
}

// ═══════════════════════════════════════════════════════════════════════════
// Catalog non-nullability
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn catalog_columns_is_not_null() {
    with_temp_database("catalog", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect fresh database");
        apply_prior_migrations(&mut conn).await;

        seed_project(&mut conn, "project-1").await;
        seed_user(&mut conn, DESIGNATED, false).await;
        seed_task(&mut conn, "task-1", "project-1", None).await;

        set_operator(&mut conn, DESIGNATED).await;
        apply_contract_migration(&mut conn).await;

        // information_schema.columns
        let is_nullable: String = sqlx::query_scalar(
            "SELECT is_nullable FROM information_schema.columns \
             WHERE table_name = 'tasks' AND column_name = 'created_by_user_id'",
        )
        .fetch_one(&mut conn)
        .await
        .expect("check information_schema");
        assert_eq!(
            is_nullable, "NO",
            "information_schema should report NOT NULL"
        );

        // pg_attribute.attnotnull
        let attnotnull: bool = sqlx::query_scalar(
            "SELECT a.attnotnull FROM pg_attribute a \
             JOIN pg_class c ON c.oid = a.attrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relname = 'tasks' AND a.attname = 'created_by_user_id'",
        )
        .fetch_one(&mut conn)
        .await
        .expect("check pg_attribute");
        assert!(attnotnull, "pg_attribute.attnotnull should be true");

        conn.close().await.expect("close");
    })
    .await;
}

// ═══════════════════════════════════════════════════════════════════════════
// Full runner integration (repository-owned migration runner)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn full_runner_applies_contract_migration_with_operator() {
    with_temp_database("full_runner", |db_url| async move {
        // Use the production bootstrap + runner path, exactly like the template
        // bootstrap: provision the operator, then run the full migrator.
        djinn_db::migrations::bootstrap_designated_operator(
            &db_url,
            &djinn_db::migrations::DesignatedOperatorBootstrap {
                user_id: DESIGNATED.to_owned(),
                github_id: 9_000_000_099,
                github_login: "contract-test-operator".to_owned(),
                github_name: Some("Contract test operator".to_owned()),
                github_avatar_url: None,
            },
        )
        .await
        .expect("bootstrap designated operator");

        djinn_db::migrations::run_postgres_migrations(
            &db_url,
            &MigrationContext {
                designated_operator_user_id: Some(DESIGNATED.to_owned()),
            },
        )
        .await
        .expect("full migration runner should succeed with operator");

        let pool = sqlx::PgPool::connect(&db_url)
            .await
            .expect("connect to verify");

        let applied: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM _sqlx_migrations WHERE version = 141 AND success = TRUE",
        )
        .fetch_one(&pool)
        .await
        .expect("check migration recorded");
        assert!(
            applied == 1,
            "migration 141 should be recorded in _sqlx_migrations"
        );

        let is_nullable: String = sqlx::query_scalar(
            "SELECT is_nullable FROM information_schema.columns \
             WHERE table_name = 'tasks' AND column_name = 'created_by_user_id'",
        )
        .fetch_one(&pool)
        .await
        .expect("check nullability");
        assert_eq!(
            is_nullable, "NO",
            "column should be NOT NULL after full runner"
        );

        pool.close().await;
    })
    .await;
}

// ═══════════════════════════════════════════════════════════════════════════
// Delete referenced user fails under NOT NULL + FK
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn deleting_referenced_user_fails_after_contract() {
    with_temp_database("del_user", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect fresh database");
        apply_prior_migrations(&mut conn).await;

        seed_project(&mut conn, "project-1").await;
        seed_user(&mut conn, "user-referenced", false).await;
        seed_user(&mut conn, DESIGNATED, false).await;
        seed_task(&mut conn, "task-1", "project-1", Some("user-referenced")).await;

        set_operator(&mut conn, DESIGNATED).await;
        apply_contract_migration(&mut conn).await;

        let err = sqlx::query("DELETE FROM users WHERE id = 'user-referenced'")
            .execute(&mut conn)
            .await;
        assert!(
            err.is_err(),
            "deleting a referenced user must fail under NOT NULL + FK SET NULL"
        );

        assert_eq!(
            get_task_creator(&mut conn, "task-1").await,
            Some("user-referenced".to_owned()),
            "referenced creator must remain after failed delete"
        );

        conn.close().await.expect("close");
    })
    .await;
}
