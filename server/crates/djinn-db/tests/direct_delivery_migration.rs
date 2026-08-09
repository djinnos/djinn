//! Compatibility proof for the dormant direct-delivery schema.

use djinn_db::{Database, DirectDeliveryCapabilityRepository, DirectDeliverySchemaCapability};

#[tokio::test]
async fn disabled_epoch_preserves_legacy_task_pr_delivery_and_probe_is_read_only() {
    let db = Database::ephemeral().await.unwrap();
    sqlx::query(
        "INSERT INTO projects (id, name) VALUES ('direct-delivery-project', 'direct-delivery-project')",
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO tasks (id, project_id, short_id, title, description, design, labels, acceptance_criteria, memory_refs, pr_url) \
         VALUES ('legacy-delivery-task', 'direct-delivery-project', 'legacy-delivery', 'title', 'description', 'design', '[]', '[]', '[]', 'https://example.test/legacy/pr/7')",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let before: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM tasks), (SELECT count(*) FROM proposal_build_attempts), (SELECT count(*) FROM task_deliveries)",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();

    assert!(matches!(
        DirectDeliveryCapabilityRepository::new(db.clone()).probe().await.unwrap(),
        DirectDeliverySchemaCapability::SupportedDisabled { ref epoch }
            if epoch.generation == 0 && !epoch.permits_direct_delivery()
    ));
    let after: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM tasks), (SELECT count(*) FROM proposal_build_attempts), (SELECT count(*) FROM task_deliveries)",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(
        after, before,
        "the capability probe must not mutate delivery state"
    );
    let pr_url: Option<String> =
        sqlx::query_scalar("SELECT pr_url FROM tasks WHERE id = 'legacy-delivery-task'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(pr_url.as_deref(), Some("https://example.test/legacy/pr/7"));
}

#[tokio::test]
async fn probe_distinguishes_missing_schema_epoch_and_unknown_epoch() {
    let missing = Database::ephemeral().await.unwrap();
    sqlx::query("DROP TABLE direct_delivery_leases")
        .execute(missing.pool())
        .await
        .unwrap();
    assert!(matches!(
        DirectDeliveryCapabilityRepository::new(missing).probe().await.unwrap(),
        DirectDeliverySchemaCapability::MissingSchema { missing_relations }
            if missing_relations == ["direct_delivery_leases"]
    ));

    let absent = Database::ephemeral().await.unwrap();
    sqlx::query("DELETE FROM direct_delivery_epochs")
        .execute(absent.pool())
        .await
        .unwrap();
    assert!(matches!(
        DirectDeliveryCapabilityRepository::new(absent)
            .probe()
            .await
            .unwrap(),
        DirectDeliverySchemaCapability::MissingEpoch
    ));

    let unknown = Database::ephemeral().await.unwrap();
    sqlx::query(
        "ALTER TABLE direct_delivery_epochs DROP CONSTRAINT direct_delivery_epochs_state_check; \
         UPDATE direct_delivery_epochs SET state = 'unknown'",
    )
    .execute(unknown.pool())
    .await
    .unwrap();
    assert!(matches!(
        DirectDeliveryCapabilityRepository::new(unknown).probe().await.unwrap(),
        DirectDeliverySchemaCapability::UnknownEpochState { state, generation: 0 }
            if state == "unknown"
    ));
}
