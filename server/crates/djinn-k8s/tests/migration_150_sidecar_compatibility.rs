//! Live-Postgres regression proving migration-150 wrapper identity values are
//! inert when an ordinary service preset is resolved and rendered as a sidecar.

use std::collections::BTreeMap;

use djinn_core::events::EventBus;
use djinn_db::test_support::with_migration_150_fixture;
use djinn_db::{ImageRepository, ProjectRepository};
use djinn_k8s::config::KubernetesConfig;
use djinn_k8s::sidecar::{
    SIDECAR_DSHM_VOLUME, resolve_image_services_with_metadata, sidecar_conn_env, sidecar_container,
};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

const PROJECT_ID: &str = "00000000-0000-7000-8000-000000000151";
const IMAGE_ID: &str = "migration-150-sidecar-image";
const FORBIDDEN_WRAPPER_MARKERS: [&str; 3] = [
    "CATALOG_CONTROL_SOCKET",
    "svc-control",
    "/var/run/djinn/service-control",
];

#[tokio::test]
async fn ordinary_sidecar_ignores_populated_migration_150_wrapper_values() {
    with_migration_150_fixture(|fixture| async move {
        ProjectRepository::new(fixture.database.clone(), EventBus::noop())
            .create_with_id(
                PROJECT_ID,
                "migration-150-sidecar-compatibility",
                "djinn-test",
                "migration-150-sidecar-compatibility",
            )
            .await?;

        let images = ImageRepository::new(fixture.database.clone());
        images
            .create(IMAGE_ID, "Migration 150 sidecar image", None, "{}")
            .await?;
        images
            .set_service_presets(IMAGE_ID, &[fixture.preset_id.to_owned()])
            .await?;
        images.set_project_image(PROJECT_ID, Some(IMAGE_ID)).await?;

        let resolution = resolve_image_services_with_metadata(&fixture.database, PROJECT_ID).await;
        assert!(resolution.lookup_error.is_none());
        assert!(resolution.skipped.is_empty());
        assert_eq!(resolution.requested_preset_ids, [fixture.preset_id]);
        assert_eq!(resolution.injected.len(), 1);
        assert_eq!(resolution.services.len(), 1);

        let injected = &resolution.injected[0];
        assert_eq!(injected.preset_id, fixture.preset_id);
        assert_eq!(injected.service_type, fixture.ordinary_preset.service_type);
        assert_eq!(injected.port, fixture.ordinary_preset.port);
        assert_eq!(injected.conn_env_var, fixture.ordinary_preset.conn_env_var);

        let service = &resolution.services[0];
        assert_eq!(service.image, fixture.ordinary_preset.image);
        assert_ne!(service.image, fixture.historical_wrapper.wrapper_image);
        assert_ne!(
            service.image,
            format!(
                "{}@{}",
                fixture.historical_wrapper.wrapper_image, fixture.historical_wrapper.image_digest
            )
        );
        assert_eq!(service.service_type, fixture.ordinary_preset.service_type);
        assert_eq!(service.port, fixture.ordinary_preset.port);
        assert_eq!(service.conn_template, fixture.ordinary_preset.conn_template);
        assert_eq!(service.conn_env_var, fixture.ordinary_preset.conn_env_var);

        let expected_env: BTreeMap<String, String> =
            serde_json::from_str(fixture.ordinary_preset.env).expect("fixture env is valid JSON");
        let actual_env: BTreeMap<_, _> = service.env.iter().cloned().collect();
        assert_eq!(actual_env, expected_env);

        let expected_resources: serde_json::Value =
            serde_json::from_str(fixture.ordinary_preset.resources)
                .expect("fixture resources are valid JSON");
        assert_eq!(
            service.cpu_request,
            expected_resources["cpu_request"].as_str().unwrap()
        );
        assert_eq!(
            service.memory_request,
            expected_resources["memory_request"].as_str().unwrap()
        );
        assert_eq!(
            service.cpu_limit,
            expected_resources["cpu_limit"].as_str().unwrap()
        );
        assert_eq!(
            service.memory_limit,
            expected_resources["memory_limit"].as_str().unwrap()
        );

        let connection_env = sidecar_conn_env(service);
        assert_eq!(connection_env.len(), 2);
        assert_eq!(connection_env[0].name, "DATABASE_URL");
        assert_eq!(connection_env[1].name, "TEST_POSTGRES_URL");
        let expected_connection = fixture
            .ordinary_preset
            .conn_template
            .replace("{host}", "127.0.0.1")
            .replace("{port}", &fixture.ordinary_preset.port.to_string());
        assert!(
            connection_env
                .iter()
                .all(|env| env.value.as_deref() == Some(expected_connection.as_str()))
        );

        let container = sidecar_container(&KubernetesConfig::for_testing(), service);
        assert_eq!(
            container.image.as_deref(),
            Some(fixture.ordinary_preset.image)
        );
        assert_eq!(container.restart_policy.as_deref(), Some("Always"));

        let container_env: BTreeMap<_, _> = container
            .env
            .as_ref()
            .expect("preset env is rendered")
            .iter()
            .map(|env| {
                (
                    env.name.clone(),
                    env.value.clone().expect("preset env has a literal value"),
                )
            })
            .collect();
        assert_eq!(container_env, expected_env);

        for probe in [
            container.startup_probe.as_ref().expect("startup probe"),
            container.readiness_probe.as_ref().expect("readiness probe"),
        ] {
            assert_eq!(
                probe.tcp_socket.as_ref().expect("TCP probe").port,
                IntOrString::Int(fixture.ordinary_preset.port)
            );
        }

        let resources = container.resources.as_ref().expect("sidecar resources");
        let requests = resources.requests.as_ref().expect("resource requests");
        let limits = resources.limits.as_ref().expect("resource limits");
        assert_eq!(requests["cpu"].0, service.cpu_request);
        assert_eq!(requests["memory"].0, service.memory_request);
        assert_eq!(limits["cpu"].0, service.cpu_limit);
        assert_eq!(limits["memory"].0, service.memory_limit);

        let mounts = container.volume_mounts.as_ref().expect("sidecar mounts");
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].name, SIDECAR_DSHM_VOLUME);
        assert_eq!(mounts[0].mount_path, "/dev/shm");

        // The fixture only returns after migration 150's historical columns
        // have been populated and the database has upgraded through HEAD. Keep
        // those deliberately non-null values visible in this cross-layer test
        // without making djinn-k8s a direct SQL owner.
        assert!(!fixture.historical_wrapper.wrapper_image.is_empty());
        assert!(!fixture.historical_wrapper.image_digest.is_empty());
        assert!(fixture.historical_wrapper.verification_protocol_revision > 0);

        let rendered = serde_json::to_string(&container).expect("container serializes");
        for marker in FORBIDDEN_WRAPPER_MARKERS {
            assert!(
                !rendered.contains(marker),
                "ordinary sidecar unexpectedly contains retired wrapper marker {marker}"
            );
        }

        Ok(())
    })
    .await
    .expect("migration-150 fixture callback succeeds");
}
