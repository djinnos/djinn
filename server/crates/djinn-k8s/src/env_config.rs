//! Per-project `environment_config` ConfigMap helpers.
//!
//! The environment-config ConfigMap carries the JSON blob stored in
//! `projects.environment_config`, mounted read-only at
//! [`ENV_CONFIG_MOUNT_FILE`] on warm + task-run Pods. The
//! `djinn-agent-worker` lifecycle runner reads from that path at Pod
//! start (see `djinn-agent-worker/src/lifecycle.rs::load_environment_config`).
//!
//! ## Lifecycle
//!
//! * The CM is not automatically created by this crate. Until P6 wires
//!   the MCP tool `project_environment_config_set` to upsert it (via
//!   [`build_env_config_config_map`]), the volume mount references a
//!   CM that doesn't exist — hence [`ENV_CONFIG_VOLUME_OPTIONAL`] is
//!   `true`. Kubelet tolerates the absence and the mount resolves to
//!   an empty dir; the worker's load path handles that gracefully.
//! * After P5 wires the boot reseed hook and P6 wires the MCP writer,
//!   every project has a CM and the mount is populated.
//! * A CM update is picked up by kubelet on the normal cadence; the
//!   in-flight warm/task-run Pod does not rotate, but the next
//!   scheduled Pod sees the new content. That's the intended semantics —
//!   environment_config changes null `image_hash` anyway, so a rebuild
//!   is about to happen.

use k8s_openapi::api::core::v1::{ConfigMap, ConfigMapVolumeSource, Volume, VolumeMount};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use std::collections::BTreeMap;

use crate::warm_job::sanitize_id;

/// Key inside the `djinn-env-<project_id>` ConfigMap that carries the
/// JSON blob.
pub const ENV_CONFIG_KEY: &str = "environment.json";
/// Mount directory inside the warm / task-run Pod.
pub const ENV_CONFIG_MOUNT_DIR: &str = "/etc/djinn";
/// Full path of the JSON file inside the Pod. The lifecycle runner
/// reads from here.
pub const ENV_CONFIG_MOUNT_FILE: &str = "/etc/djinn/environment.json";
/// Volume name used for the ConfigMap mount.
pub const VOLUME_ENV_CONFIG: &str = "env-config";
/// `optional: true` on the volume so Pods tolerate a missing CM
/// pre-cut-over. The worker treats an empty /etc/djinn as "no config".
pub const ENV_CONFIG_VOLUME_OPTIONAL: bool = true;

/// Canonical ConfigMap name for a project. Sanitised to DNS-label form
/// to match Pod label conventions elsewhere in this crate.
pub fn env_config_config_map_name(project_id: &str) -> String {
    format!("djinn-env-{}", sanitize_id(project_id))
}

/// The volume-mount to attach to a container that wants to read the
/// project's environment config at start.
pub fn env_config_volume_mount() -> VolumeMount {
    VolumeMount {
        name: VOLUME_ENV_CONFIG.to_string(),
        mount_path: ENV_CONFIG_MOUNT_DIR.to_string(),
        read_only: Some(true),
        ..VolumeMount::default()
    }
}

/// The pod-level Volume declaration that references the per-project
/// CM. `optional: true` — see [`ENV_CONFIG_VOLUME_OPTIONAL`].
pub fn env_config_volume(project_id: &str) -> Volume {
    Volume {
        name: VOLUME_ENV_CONFIG.to_string(),
        config_map: Some(ConfigMapVolumeSource {
            name: env_config_config_map_name(project_id),
            optional: Some(ENV_CONFIG_VOLUME_OPTIONAL),
            ..ConfigMapVolumeSource::default()
        }),
        ..Volume::default()
    }
}

/// Build the ConfigMap manifest for a project. The caller takes the
/// serialized `EnvironmentConfig` JSON string (typically via
/// `serde_json::to_string(&cfg)`) and passes it here; writing to
/// the cluster is the caller's responsibility (P6 MCP tool upserts
/// via kube-rs when `project_environment_config_set` fires).
pub fn build_env_config_config_map(
    namespace: &str,
    project_id: &str,
    environment_config_json: &str,
) -> ConfigMap {
    let mut data = BTreeMap::new();
    data.insert(ENV_CONFIG_KEY.to_string(), environment_config_json.to_string());

    let labels = env_config_labels(project_id);

    ConfigMap {
        metadata: ObjectMeta {
            name: Some(env_config_config_map_name(project_id)),
            namespace: Some(namespace.to_string()),
            labels: Some(labels),
            ..ObjectMeta::default()
        },
        data: Some(data),
        ..ConfigMap::default()
    }
}

fn env_config_labels(project_id: &str) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    labels.insert(
        crate::warm_job::LABEL_PROJECT_ID.to_string(),
        sanitize_id(project_id),
    );
    labels.insert(
        crate::warm_job::LABEL_COMPONENT.to_string(),
        "environment-config".to_string(),
    );
    labels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_map_name_follows_sanitised_pattern() {
        assert_eq!(
            env_config_config_map_name("Proj_ABC/xyz"),
            "djinn-env-proj-abc-xyz"
        );
    }

    #[test]
    fn volume_points_at_optional_config_map() {
        let v = env_config_volume("proj-xyz");
        assert_eq!(v.name, VOLUME_ENV_CONFIG);
        let cm = v.config_map.expect("config_map source");
        assert_eq!(cm.name, "djinn-env-proj-xyz");
        assert_eq!(cm.optional, Some(true));
    }

    #[test]
    fn volume_mount_targets_etc_djinn_readonly() {
        let m = env_config_volume_mount();
        assert_eq!(m.name, VOLUME_ENV_CONFIG);
        assert_eq!(m.mount_path, ENV_CONFIG_MOUNT_DIR);
        assert_eq!(m.read_only, Some(true));
    }

    #[test]
    fn build_config_map_carries_single_json_key() {
        let cm = build_env_config_config_map(
            "djinn",
            "proj-xyz",
            r#"{"schema_version":1,"source":"auto-detected"}"#,
        );
        let meta = &cm.metadata;
        assert_eq!(meta.name.as_deref(), Some("djinn-env-proj-xyz"));
        assert_eq!(meta.namespace.as_deref(), Some("djinn"));
        let labels = meta.labels.as_ref().expect("labels");
        assert_eq!(
            labels.get("djinn.app/project-id").map(String::as_str),
            Some("proj-xyz")
        );
        let data = cm.data.expect("data");
        assert_eq!(
            data.get(ENV_CONFIG_KEY).map(String::as_str),
            Some(r#"{"schema_version":1,"source":"auto-detected"}"#)
        );
    }
}
