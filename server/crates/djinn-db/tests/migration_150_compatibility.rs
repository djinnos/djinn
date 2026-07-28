//! Proves a populated migration-150 database upgrades through HEAD while the
//! application-facing preset reader continues to expose only ordinary fields.

use djinn_db::test_support::with_migration_150_fixture;
use djinn_db::ServicePresetRepository;

#[tokio::test]
async fn migration_150_wrapper_values_survive_upgrade_and_are_ignored_by_preset_reads() {
    with_migration_150_fixture(|fixture| async move {
        let (success, checksum): (bool, Vec<u8>) = sqlx::query_as(
            "SELECT success, checksum FROM _sqlx_migrations WHERE version = 150",
        )
        .fetch_one(fixture.database.pool())
        .await?;
        assert!(success, "migration 150 must be recorded as successful");
        assert!(
            !checksum.is_empty(),
            "migration 150 must retain its real sqlx checksum"
        );

        let (wrapper_image, image_digest, verification_protocol_revision): (
            Option<String>,
            Option<String>,
            Option<i32>,
        ) = sqlx::query_as(
            "SELECT wrapper_image, image_digest, verification_protocol_revision \
             FROM service_presets WHERE id = $1",
        )
        .bind(fixture.preset_id)
        .fetch_one(fixture.database.pool())
        .await?;
        assert_eq!(
            wrapper_image.as_deref(),
            Some(fixture.historical_wrapper.wrapper_image)
        );
        assert_eq!(
            image_digest.as_deref(),
            Some(fixture.historical_wrapper.image_digest)
        );
        assert_eq!(
            verification_protocol_revision,
            Some(fixture.historical_wrapper.verification_protocol_revision)
        );

        let preset = ServicePresetRepository::new(fixture.database.clone())
            .get(fixture.preset_id)
            .await?
            .expect("the selected migration-150 preset remains readable");
        assert_eq!(preset.id, fixture.preset_id);
        assert_eq!(preset.name, "Postgres 18");
        assert_eq!(preset.service_type, fixture.ordinary_preset.service_type);
        assert_eq!(preset.image, fixture.ordinary_preset.image);
        assert_eq!(preset.port, fixture.ordinary_preset.port);
        assert_eq!(preset.env, fixture.ordinary_preset.env);
        assert_eq!(preset.resources, fixture.ordinary_preset.resources);
        assert_eq!(
            preset.conn_template,
            fixture.ordinary_preset.conn_template
        );
        assert_eq!(preset.conn_env_var, fixture.ordinary_preset.conn_env_var);
        assert_eq!(preset.client_package.as_deref(), Some("postgresql-client"));

        Ok(())
    })
    .await
    .expect("migration-150 fixture callback succeeds");
}
