// djinn:allow-oversize — wrapper-service control wiring plus launcher mount
// isolation tipped this just past the 50KB byte guard; split the sidecar/wrapper
// halves when this module is touched substantively.
//! Native-sidecar injection for backing services (Postgres / Redis / RabbitMQ).
//!
//! A project's selected catalog image declares which service presets every
//! task-run Pod should provide (the `image_service_presets`
//! junction, migration 66). Each declared service is injected as a *native
//! sidecar*: an init container with `restartPolicy: Always`. A native sidecar
//!   * shares the Pod network namespace, so the worker reaches it on
//!     `127.0.0.1:<port>` — no ClusterIP Service, DNS name, or NetworkPolicy;
//!   * has its startup gated before the worker container starts (via a startup
//!     probe), so the DB is already accepting connections when the worker runs;
//!   * is terminated automatically by the kubelet when the worker container
//!     exits, so the Job still reaches Completed. A plain extra container would
//!     run forever and the Job would never finish — that's the whole reason a
//!     *native* sidecar (initContainer + `restartPolicy: Always`) is required.
//!
//! The connection string is exported to the worker container as the preset's
//! env var (e.g. `TEST_POSTGRES_URL`) so the agent just reads it and uses it —
//! no MCP round-trip, no explicit "start a database" step.
//!
//! [`sidecar_container`] / [`sidecar_conn_env`] are pure manifest builders;
//! [`resolve_image_services`] is the impure resolver that reads the catalog for
//! a project's image (used by the task-run dispatch paths).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use k8s_openapi::api::core::v1::{
    Container, ContainerPort, EmptyDirVolumeSource, EnvVar, Probe, ResourceRequirements,
    SecurityContext, TCPSocketAction, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

use djinn_db::{Database, ImageRepository, ServicePreset, ServicePresetRepository};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::config::KubernetesConfig;

/// Pod-level Memory `emptyDir` mounted into each sidecar at `/dev/shm`. The K8s
/// default `/dev/shm` (64Mi) is too small for some service defaults (Postgres
/// dynamic shared memory); harmless for Redis/RabbitMQ. The Pod adds the volume
/// only when at least one service is injected.
pub const SIDECAR_DSHM_VOLUME: &str = "svc-dshm";

/// Pod volume name for the private service-control `emptyDir`. Mounted only into
/// the worker container and the wrapper sidecars — never a canonical command
/// container. The wrapper sidecars bind their protocol-v1 control sockets here
/// and the worker adapter connects to them.
pub const CONTROL_VOLUME: &str = "svc-control";

/// In-Pod mount path of the private service-control `emptyDir`. Each wrapper
/// sidecar binds `<CONTROL_SOCKET_DIR>/<socket>` and the worker adapter connects
/// to the identical path. This directory is deliberately absent from every
/// canonical-command Landlock grant, so verification commands cannot reach it.
pub const CONTROL_SOCKET_DIR: &str = "/var/run/djinn/service-control";

/// Environment variable a wrapper sidecar reads to learn where to bind its
/// control socket. Consumed by the `djinn-*-wrapper` binaries.
pub const CONTROL_SOCKET_ENV: &str = "CATALOG_CONTROL_SOCKET";

/// Deterministic, sanitized socket file name for a preset. Every byte that is
/// not `[a-z0-9_-]` collapses to `_`; the result is lowercased and bounded so a
/// preset id can never escape the control directory or produce a path-unsafe
/// name. Distinct presets whose ids sanitize to the same stem collide on
/// purpose — strict resolution rejects that collision rather than silently
/// serving two services from one socket.
pub fn control_socket_file_name(preset_id: &str) -> String {
    let mut stem: String = preset_id
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    stem.truncate(64);
    if stem.is_empty() {
        stem.push('_');
    }
    format!("{stem}.sock")
}

/// Absolute in-Pod path of a preset's control socket. The worker adapter and
/// the wrapper sidecar derive the identical path from this function so a client
/// socket always names the socket its wrapper server binds.
pub fn control_socket_path(preset_id: &str) -> PathBuf {
    PathBuf::from(CONTROL_SOCKET_DIR).join(control_socket_file_name(preset_id))
}

/// Absolute in-Pod path for an already-sanitized socket file name.
pub fn control_socket_path_from_name(socket_name: &str) -> PathBuf {
    PathBuf::from(CONTROL_SOCKET_DIR).join(socket_name)
}

/// The private control `emptyDir` volume. Added to the Pod only when at least
/// one wrapper sidecar is injected.
pub fn control_volume() -> Volume {
    Volume {
        name: CONTROL_VOLUME.to_string(),
        empty_dir: Some(EmptyDirVolumeSource::default()),
        ..Volume::default()
    }
}

/// The control-socket mount shared by the worker and every wrapper sidecar.
pub fn control_volume_mount() -> VolumeMount {
    VolumeMount {
        name: CONTROL_VOLUME.to_string(),
        mount_path: CONTROL_SOCKET_DIR.to_string(),
        ..VolumeMount::default()
    }
}

/// The admin-URL environment variable each wrapper binary reads to reach its
/// co-located stock daemon on Pod loopback. Returns `None` for a service type
/// that has no catalog wrapper.
fn wrapper_admin_env_var(service_type: &str) -> Option<&'static str> {
    match service_type {
        "postgres" => Some("POSTGRES_WRAPPER_ADMIN_URL"),
        "redis" => Some("REDIS_WRAPPER_ADMIN_URL"),
        "rabbitmq" => Some("RABBITMQ_WRAPPER_AMQP_URL"),
        _ => None,
    }
}

/// The exported-environment-names variable each wrapper binary reads to learn
/// which env names a lease URL is returned under. Returns `None` for a service
/// type that has no catalog wrapper.
fn wrapper_env_names_var(service_type: &str) -> Option<&'static str> {
    match service_type {
        "postgres" => Some("CATALOG_POSTGRES_ENV_NAMES"),
        "redis" => Some("CATALOG_REDIS_ENV_NAMES"),
        "rabbitmq" => Some("CATALOG_RABBITMQ_ENV_NAMES"),
        _ => None,
    }
}

/// Everything the sidecar builder needs from a `service_presets` row. The
/// caller maps it so this module never reaches into DB row shape.
#[derive(Clone, Debug)]
pub struct BackingServiceSpec {
    pub service_type: String,
    /// The container image the sidecar runs. When a wrapper artifact is
    /// resolved this is the digest-pinned wrapper reference
    /// (`{wrapper_image}@{digest}`); legacy presets without a wrapper keep the
    /// stock service image for dispatch compatibility.
    pub image: String,
    /// `true` when the sidecar runs a catalog wrapper (stock daemon + protocol
    /// control server). Drives the private control mount, the socket env, and
    /// the wrapper admin env. `false` keeps the pre-wrapper dispatch shape.
    pub is_wrapper: bool,
    /// Sanitized per-preset control socket file name (see
    /// [`control_socket_file_name`]).
    pub control_socket_name: String,
    /// Extra environment the wrapper binary needs (admin URL + exported env
    /// names). Empty for non-wrapper sidecars.
    pub wrapper_env: Vec<(String, String)>,
    pub port: i32,
    pub env: Vec<(String, String)>,
    pub cpu_request: String,
    pub memory_request: String,
    pub cpu_limit: String,
    pub memory_limit: String,
    pub conn_template: String,
    pub conn_env_var: String,
}

fn valid_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Resolve one immutable image catalog strictly for canonical verification.
/// Dispatch continues to use the fail-open resolver below.
pub async fn resolve_image_services_strict(
    db: &Database,
    image_id: &str,
) -> Result<Vec<ResolvedCatalogService>, CatalogServiceResolutionError> {
    let images = ImageRepository::new(db.clone());
    let image = images
        .get(image_id)
        .await
        .map_err(|_| CatalogServiceResolutionError::LookupFailed)?
        .ok_or(CatalogServiceResolutionError::MissingImage)?;
    if image.status != djinn_db::ImageStatus::READY {
        return Err(CatalogServiceResolutionError::ImageNotReady);
    }
    if !image.registry_digest.as_deref().is_some_and(valid_digest) {
        return Err(CatalogServiceResolutionError::MalformedImageDigest);
    }
    let mut preset_ids = images
        .list_service_presets(image_id)
        .await
        .map_err(|_| CatalogServiceResolutionError::LookupFailed)?;
    preset_ids.sort();
    let presets = ServicePresetRepository::new(db.clone());
    let mut rows = Vec::with_capacity(preset_ids.len());
    for preset_id in preset_ids {
        let preset = presets
            .get(&preset_id)
            .await
            .map_err(|_| CatalogServiceResolutionError::LookupFailed)?
            .ok_or_else(|| CatalogServiceResolutionError::MissingPreset {
                preset_id: preset_id.clone(),
            })?;
        rows.push((preset_id, preset));
    }
    strict_catalog_from_presets(rows)
}

/// Strictly validate already-loaded catalog rows. Kept separate from database
/// lookup so every fail-closed catalog invariant has a deterministic unit test.
fn strict_catalog_from_presets(
    mut rows: Vec<(String, ServicePreset)>,
) -> Result<Vec<ResolvedCatalogService>, CatalogServiceResolutionError> {
    // Database ordering is not part of the catalog contract. Canonicalize at
    // this validation boundary so every caller receives the same identity
    // material even when rows were loaded in a different order.
    rows.sort_by(|left, right| left.0.cmp(&right.0));
    let (mut ids, mut types, mut ports, mut env_names, mut socket_names) = (
        BTreeSet::new(),
        BTreeSet::new(),
        BTreeSet::new(),
        BTreeSet::new(),
        BTreeSet::new(),
    );
    let mut result = Vec::with_capacity(rows.len());
    for (preset_id, p) in rows {
        if !ids.insert(preset_id.clone()) {
            return Err(CatalogServiceResolutionError::Duplicate {
                kind: "preset ID",
                value: preset_id,
            });
        }
        // A wrapper artifact reference is mandatory: a bare stock image has no
        // protocol control server, so it can never satisfy strict verification.
        let wrapper_image = p.wrapper_image.clone().filter(|w| !w.trim().is_empty());
        let wrapper_image = match wrapper_image {
            Some(w) => w,
            None => {
                return Err(CatalogServiceResolutionError::StockImageWithoutWrapper { preset_id });
            }
        };
        // The wrapper reference must be a plain repository ref; the digest is the
        // sole immutable pin. A digest-suffixed (mutable/ambiguous) ref fails.
        if wrapper_image.contains('@') {
            return Err(CatalogServiceResolutionError::MutableWrapperReference { preset_id });
        }
        let digest = p
            .image_digest
            .clone()
            .filter(|d| valid_digest(d))
            .ok_or_else(|| CatalogServiceResolutionError::MalformedServiceDigest {
                preset_id: p.id.clone(),
            })?;
        let revision = p.verification_protocol_revision.unwrap_or_default();
        if revision != 1 {
            return Err(CatalogServiceResolutionError::UnsupportedProtocol {
                preset_id: p.id,
                revision,
            });
        }
        if p.image.trim().is_empty()
            || p.image.contains('@')
            || p.port <= 0
            || serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&p.env).is_err()
            || serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&p.resources)
                .is_err()
        {
            return Err(CatalogServiceResolutionError::MalformedConfiguration { preset_id });
        }
        if !types.insert(p.service_type.clone()) {
            return Err(CatalogServiceResolutionError::Duplicate {
                kind: "service type",
                value: p.service_type,
            });
        }
        if !ports.insert(p.port) {
            return Err(CatalogServiceResolutionError::Duplicate {
                kind: "port",
                value: p.port.to_string(),
            });
        }
        let mut names: Vec<String> = p
            .conn_env_var
            .split(',')
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        names.sort();
        // Do not deduplicate before checking: duplicate exports are ambiguous
        // catalog material and canonical verification must fail closed.
        if names.is_empty() {
            return Err(CatalogServiceResolutionError::MalformedConfiguration { preset_id });
        }
        for name in &names {
            if !env_names.insert(name.clone()) {
                return Err(CatalogServiceResolutionError::Duplicate {
                    kind: "exported environment",
                    value: name.clone(),
                });
            }
        }
        // The socket the worker adapter connects to is derived deterministically
        // from the preset id. Two presets that sanitize to the same socket name
        // would share one control socket ambiguously — reject that collision.
        let control_socket_name = control_socket_file_name(&preset_id);
        if !socket_names.insert(control_socket_name.clone()) {
            return Err(CatalogServiceResolutionError::Duplicate {
                kind: "control socket identity",
                value: control_socket_name,
            });
        }
        let effective = serde_json::json!({"env": serde_json::from_str::<serde_json::Value>(&p.env).unwrap(), "resources": serde_json::from_str::<serde_json::Value>(&p.resources).unwrap(), "conn_template": p.conn_template, "port": p.port, "protocol": revision, "wrapper_image": wrapper_image});
        result.push(ResolvedCatalogService {
            preset_id: preset_id.clone(),
            service_type: p.service_type,
            image_reference: p.image,
            wrapper_reference: format!("{wrapper_image}@{digest}"),
            image_digest: digest,
            port: p.port,
            control_socket_name,
            exported_environment_names: names,
            verification_protocol_revision: revision as u32,
            effective_configuration_digest: format!(
                "sha256:{:x}",
                Sha256::digest(serde_json::to_vec(&effective).expect("serializable"))
            ),
        });
    }
    Ok(result)
}

/// Secret-free immutable material authorized by one catalog image.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedCatalogService {
    pub preset_id: String,
    pub service_type: String,
    /// The stock service image reference (retained for identity/observability).
    pub image_reference: String,
    /// The digest-pinned wrapper artifact reference the sidecar actually runs:
    /// `{wrapper_image}@{image_digest}`.
    pub wrapper_reference: String,
    pub image_digest: String,
    pub port: i32,
    /// Deterministic sanitized control socket file name the worker adapter
    /// connects to and the wrapper sidecar binds.
    pub control_socket_name: String,
    pub exported_environment_names: Vec<String>,
    pub verification_protocol_revision: u32,
    pub effective_configuration_digest: String,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum CatalogServiceResolutionError {
    #[error("catalog image is missing")]
    MissingImage,
    #[error("catalog image is not ready")]
    ImageNotReady,
    #[error("catalog image digest is missing or malformed")]
    MalformedImageDigest,
    #[error("service preset {preset_id} is missing")]
    MissingPreset { preset_id: String },
    #[error("service preset {preset_id} has a missing or malformed immutable image digest")]
    MalformedServiceDigest { preset_id: String },
    #[error("service preset {preset_id} has no catalog wrapper image (stock image only)")]
    StockImageWithoutWrapper { preset_id: String },
    #[error("service preset {preset_id} wrapper reference is mutable (carries a digest suffix)")]
    MutableWrapperReference { preset_id: String },
    #[error("duplicate {kind}: {value}")]
    Duplicate { kind: &'static str, value: String },
    #[error("service preset {preset_id} has malformed configuration")]
    MalformedConfiguration { preset_id: String },
    #[error("service preset {preset_id} uses unsupported protocol revision {revision}")]
    UnsupportedProtocol { preset_id: String, revision: i32 },
    #[error("catalog lookup failed")]
    LookupFailed,
}

/// Catalog image selected for a project while resolving native sidecars.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedImageMetadata {
    pub id: String,
    pub name: String,
    pub tag: Option<String>,
}

/// A service preset that resolved to an injected sidecar spec.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InjectedServiceMetadata {
    pub preset_id: String,
    pub service_type: String,
    pub port: i32,
    pub conn_env_var: String,
}

/// A declared preset that could not be converted into a sidecar spec.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedServicePreset {
    pub preset_id: String,
    pub reason: String,
}

/// Full resolution result used for task-run observability.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ImageServiceResolution {
    pub image: Option<ResolvedImageMetadata>,
    pub requested_preset_ids: Vec<String>,
    pub injected: Vec<InjectedServiceMetadata>,
    pub skipped: Vec<SkippedServicePreset>,
    pub lookup_error: Option<String>,
    #[serde(skip)]
    pub services: Vec<BackingServiceSpec>,
}

/// The pod-level `/dev/shm` Memory `emptyDir` referenced by the sidecar mounts.
/// Add it to the Pod's volumes only when at least one service is injected.
pub fn sidecar_dshm_volume() -> Volume {
    Volume {
        name: SIDECAR_DSHM_VOLUME.to_string(),
        empty_dir: Some(EmptyDirVolumeSource {
            medium: Some("Memory".to_string()),
            size_limit: Some(Quantity("256Mi".to_string())),
        }),
        ..Volume::default()
    }
}

/// Render a preset's connection template for a pod-local sidecar: `{host}` →
/// `127.0.0.1`, `{port}` → the service port.
pub fn render_local_conn(conn_template: &str, port: i32) -> String {
    conn_template
        .replace("{host}", "127.0.0.1")
        .replace("{port}", &port.to_string())
}

/// The env var(s) the worker container exports for this service's connection.
///
/// `conn_env_var` is a comma-separated LIST of names (e.g.
/// `DATABASE_URL,TEST_POSTGRES_URL`); the same rendered connection string is
/// emitted under each so a project can reach for the conventional name
/// (`DATABASE_URL`) or a bespoke one. Empty / whitespace-only entries are
/// skipped.
pub fn sidecar_conn_env(spec: &BackingServiceSpec) -> Vec<EnvVar> {
    let value = render_local_conn(&spec.conn_template, spec.port);
    spec.conn_env_var
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| EnvVar {
            name: name.to_string(),
            value: Some(value.clone()),
            ..EnvVar::default()
        })
        .collect()
}

/// Build the native-sidecar `Container` for one backing service. The caller
/// appends it to the Pod's `initContainers` (NOT `containers`); the
/// `restartPolicy: Always` is what makes the kubelet treat it as a sidecar.
pub fn sidecar_container(config: &KubernetesConfig, spec: &BackingServiceSpec) -> Container {
    let mut env: Vec<EnvVar> = spec
        .env
        .iter()
        .map(|(k, v)| EnvVar {
            name: k.clone(),
            value: Some(v.clone()),
            ..EnvVar::default()
        })
        .collect();
    // A wrapper sidecar additionally learns where to bind its control socket and
    // how to reach its co-located stock daemon (wrapper_env).
    if spec.is_wrapper {
        env.push(EnvVar {
            name: CONTROL_SOCKET_ENV.to_string(),
            value: Some(
                control_socket_path_from_name(&spec.control_socket_name)
                    .to_string_lossy()
                    .into_owned(),
            ),
            ..EnvVar::default()
        });
        env.extend(spec.wrapper_env.iter().map(|(k, v)| EnvVar {
            name: k.clone(),
            value: Some(v.clone()),
            ..EnvVar::default()
        }));
    }

    let mut volume_mounts = vec![VolumeMount {
        name: SIDECAR_DSHM_VOLUME.to_string(),
        mount_path: "/dev/shm".to_string(),
        ..VolumeMount::default()
    }];
    if spec.is_wrapper {
        volume_mounts.push(control_volume_mount());
    }

    let probe = || Probe {
        tcp_socket: Some(TCPSocketAction {
            port: IntOrString::Int(spec.port),
            ..TCPSocketAction::default()
        }),
        initial_delay_seconds: Some(1),
        period_seconds: Some(2),
        // ~60s budget: Postgres initdb on alpine is a few seconds; generous so
        // a slow first pull/init doesn't wedge the worker start.
        failure_threshold: Some(30),
        ..Probe::default()
    };

    Container {
        name: format!("svc-{}", spec.service_type),
        image: Some(spec.image.clone()),
        image_pull_policy: Some(config.image_pull_policy.clone()),
        // Native sidecar: an init container that keeps running for the life of
        // the worker container and is auto-terminated when the worker exits.
        restart_policy: Some("Always".to_string()),
        env: Some(env),
        ports: Some(vec![ContainerPort {
            container_port: spec.port,
            ..ContainerPort::default()
        }]),
        volume_mounts: Some(volume_mounts),
        // The worker container does not start until this startup probe passes —
        // so the service is accepting connections before any test runs.
        startup_probe: Some(probe()),
        readiness_probe: Some(probe()),
        // Override the Pod-level securityContext (which forces uid 10001 so the
        // worker can write the shared /mirror PVC). Stock service images expect
        // to start as root and drop to their own service user in their
        // entrypoint (e.g. Postgres chowns PGDATA then `su`s to `postgres`);
        // forcing uid 10001 breaks initdb. The sidecar touches no shared PVC, so
        // running it as the image default is safe — the Pod boundary is the
        // perimeter.
        security_context: Some(SecurityContext {
            run_as_user: Some(0),
            run_as_group: Some(0),
            run_as_non_root: Some(false),
            ..SecurityContext::default()
        }),
        resources: Some(ResourceRequirements {
            requests: Some(BTreeMap::from([
                ("cpu".to_string(), Quantity(spec.cpu_request.clone())),
                ("memory".to_string(), Quantity(spec.memory_request.clone())),
            ])),
            limits: Some(BTreeMap::from([
                ("cpu".to_string(), Quantity(spec.cpu_limit.clone())),
                ("memory".to_string(), Quantity(spec.memory_limit.clone())),
            ])),
            ..ResourceRequirements::default()
        }),
        ..Container::default()
    }
}

fn parse_env(json: &str) -> Vec<(String, String)> {
    match serde_json::from_str::<serde_json::Value>(json) {
        Ok(serde_json::Value::Object(map)) => map
            .into_iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
            .collect(),
        _ => Vec::new(),
    }
}

fn parse_resources(json: &str) -> (String, String, String, String) {
    let v: serde_json::Value = serde_json::from_str(json).unwrap_or(serde_json::Value::Null);
    let g = |k: &str, d: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or(d).to_string();
    (
        g("cpu_request", "100m"),
        g("memory_request", "256Mi"),
        g("cpu_limit", "500m"),
        g("memory_limit", "512Mi"),
    )
}

fn append_preset_resolution(
    project_id: &str,
    preset_id: &str,
    preset: Result<Option<ServicePreset>, String>,
    specs: &mut Vec<BackingServiceSpec>,
    injected: &mut Vec<InjectedServiceMetadata>,
    skipped: &mut Vec<SkippedServicePreset>,
) {
    match preset {
        Ok(Some(p)) => {
            let (cpu_request, memory_request, cpu_limit, memory_limit) =
                parse_resources(&p.resources);
            injected.push(InjectedServiceMetadata {
                preset_id: preset_id.to_string(),
                service_type: p.service_type.clone(),
                port: p.port,
                conn_env_var: p.conn_env_var.clone(),
            });
            // A preset with a wrapper repository AND a valid immutable digest
            // runs the digest-pinned wrapper artifact (stock daemon + control
            // server). Legacy presets missing either keep the stock image so
            // ordinary dispatch stays byte-compatible with the pre-wrapper shape.
            let wrapper = p
                .wrapper_image
                .as_deref()
                .map(str::trim)
                .filter(|w| !w.is_empty())
                .zip(p.image_digest.as_deref().filter(|d| valid_digest(d)));
            let (is_wrapper, image, wrapper_env) = match wrapper {
                Some((wrapper_image, digest)) => {
                    let mut wrapper_env = Vec::new();
                    let admin = render_local_conn(&p.conn_template, p.port);
                    if let Some(name) = wrapper_admin_env_var(&p.service_type) {
                        wrapper_env.push((name.to_string(), admin));
                    }
                    if let Some(name) = wrapper_env_names_var(&p.service_type) {
                        wrapper_env.push((name.to_string(), p.conn_env_var.clone()));
                    }
                    (true, format!("{wrapper_image}@{digest}"), wrapper_env)
                }
                None => (false, p.image.clone(), Vec::new()),
            };
            specs.push(BackingServiceSpec {
                control_socket_name: control_socket_file_name(preset_id),
                is_wrapper,
                wrapper_env,
                service_type: p.service_type,
                image,
                port: p.port,
                env: parse_env(&p.env),
                cpu_request,
                memory_request,
                cpu_limit,
                memory_limit,
                conn_template: p.conn_template,
                conn_env_var: p.conn_env_var,
            });
        }
        Ok(None) => {
            skipped.push(SkippedServicePreset {
                preset_id: preset_id.to_string(),
                reason: "unknown service preset".to_string(),
            });
            tracing::warn!(
                project_id = %project_id,
                preset_id = %preset_id,
                "sidecar: image references an unknown service preset; skipping"
            );
        }
        Err(error) => {
            let reason = format!("service preset read failed: {error}");
            skipped.push(SkippedServicePreset {
                preset_id: preset_id.to_string(),
                reason,
            });
            tracing::warn!(
                project_id = %project_id,
                preset_id = %preset_id,
                %error,
                "sidecar: service preset read failed; skipping"
            );
        }
    }
}

/// Resolve the backing services declared on a project's selected catalog image.
///
/// Returns an empty list when the project has no catalog image, the image
/// declares no services, or any read fails — service injection is best-effort
/// and must never block task dispatch.
pub async fn resolve_image_services(db: &Database, project_id: &str) -> Vec<BackingServiceSpec> {
    resolve_image_services_with_metadata(db, project_id)
        .await
        .services
}

/// Resolve the backing services declared on a project's selected catalog image,
/// preserving enough metadata for task-run logs/activity to show requested vs.
/// actually injected services.
///
/// Like [`resolve_image_services`], this is fail-open for dispatch: lookup
/// errors are represented in the returned metadata rather than returned as
/// errors. The important invariant is that once an image declares preset ids,
/// every configured-but-not-injected preset appears in `skipped` or
/// `lookup_error` instead of disappearing silently.
pub async fn resolve_image_services_with_metadata(
    db: &Database,
    project_id: &str,
) -> ImageServiceResolution {
    let image_repo = ImageRepository::new(db.clone());
    let image = match image_repo.resolve_for_project(project_id).await {
        Ok(Some(image)) => image,
        Ok(None) => return ImageServiceResolution::default(),
        Err(error) => {
            let error = error.to_string();
            tracing::warn!(
                project_id = %project_id,
                %error,
                "sidecar: resolve_for_project failed; injecting no backing services"
            );
            return ImageServiceResolution {
                lookup_error: Some(error),
                ..ImageServiceResolution::default()
            };
        }
    };

    let image_metadata = ResolvedImageMetadata {
        id: image.id.clone(),
        name: image.name.clone(),
        tag: image.tag.clone(),
    };

    let preset_ids = match image_repo.list_service_presets(&image.id).await {
        Ok(ids) => ids,
        Err(error) => {
            let error = error.to_string();
            tracing::warn!(
                project_id = %project_id,
                image_id = %image.id,
                %error,
                "sidecar: list_service_presets failed; injecting no backing services"
            );
            return ImageServiceResolution {
                image: Some(image_metadata),
                lookup_error: Some(error),
                ..ImageServiceResolution::default()
            };
        }
    };
    if preset_ids.is_empty() {
        return ImageServiceResolution {
            image: Some(image_metadata),
            ..ImageServiceResolution::default()
        };
    }

    let preset_repo = ServicePresetRepository::new(db.clone());
    let mut specs = Vec::with_capacity(preset_ids.len());
    let mut injected = Vec::with_capacity(preset_ids.len());
    let mut skipped = Vec::new();
    for preset_id in &preset_ids {
        append_preset_resolution(
            project_id,
            preset_id,
            preset_repo
                .get(preset_id)
                .await
                .map_err(|error| error.to_string()),
            &mut specs,
            &mut injected,
            &mut skipped,
        );
    }
    ImageServiceResolution {
        image: Some(image_metadata),
        requested_preset_ids: preset_ids,
        injected,
        skipped,
        lookup_error: None,
        services: specs,
    }
}

// Exact-arithmetic 12-CPU/48Gi bin-packing fixture (task ny3o). Declared here,
// as a child of `sidecar`, so it can read the private default-resource envelope
// via `parse_resources` without adding any production surface. `#[cfg(test)]`
// only — no effect on production builds.
#[cfg(test)]
#[path = "bin_packing_fixture_tests.rs"]
mod bin_packing_fixture_tests;

#[cfg(test)]
mod tests {
    use super::*;
    fn spec() -> BackingServiceSpec {
        BackingServiceSpec {
            service_type: "postgres".into(),
            image: "postgres:18-alpine".into(),
            is_wrapper: false,
            control_socket_name: "preset-postgres-18.sock".into(),
            wrapper_env: Vec::new(),
            port: 5432,
            env: vec![("POSTGRES_PASSWORD".into(), "postgres".into())],
            cpu_request: "100m".into(),
            memory_request: "256Mi".into(),
            cpu_limit: "500m".into(),
            memory_limit: "512Mi".into(),
            conn_template: "postgres://postgres:postgres@{host}:{port}/app_test".into(),
            conn_env_var: "DATABASE_URL,TEST_POSTGRES_URL".into(),
        }
    }

    fn preset() -> ServicePreset {
        ServicePreset {
            id: "preset-postgres-18".into(),
            name: "Postgres 18".into(),
            service_type: "postgres".into(),
            image: "postgres:18-alpine".into(),
            wrapper_image: Some("ghcr.io/djinnos/djinn-postgres-wrapper".into()),
            image_digest: Some("sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into()),
            verification_protocol_revision: Some(1),
            port: 5432,
            env: r#"{"POSTGRES_PASSWORD":"postgres"}"#.into(),
            resources: r#"{"cpu_request":"100m","memory_request":"256Mi","cpu_limit":"500m","memory_limit":"512Mi"}"#.into(),
            conn_template: "postgres://postgres:postgres@{host}:{port}/app_test".into(),
            conn_env_var: "DATABASE_URL,TEST_POSTGRES_URL".into(),
            client_package: Some("postgresql-client".into()),
        }
    }

    #[test]
    fn conn_env_renders_loopback_host() {
        let envs = sidecar_conn_env(&spec());
        let expected = "postgres://postgres:postgres@127.0.0.1:5432/app_test";
        // Both the conventional DATABASE_URL and the bespoke TEST_POSTGRES_URL
        // are emitted with the same loopback connection string.
        assert_eq!(envs.len(), 2);
        assert_eq!(envs[0].name, "DATABASE_URL");
        assert_eq!(envs[0].value.as_deref(), Some(expected));
        assert_eq!(envs[1].name, "TEST_POSTGRES_URL");
        assert_eq!(envs[1].value.as_deref(), Some(expected));
    }

    #[test]
    fn conn_env_skips_empty_and_whitespace_names() {
        let mut s = spec();
        s.conn_env_var = " DATABASE_URL , ,TEST_POSTGRES_URL,".into();
        let envs = sidecar_conn_env(&s);
        let names: Vec<&str> = envs.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["DATABASE_URL", "TEST_POSTGRES_URL"]);
    }

    /// AC (qut0): backing-service sidecar resources fall back to the explicit
    /// 100m/256Mi request + 500m/512Mi limit envelope when the preset omits or
    /// malforms them, and an explicit resources JSON overrides each field.
    #[test]
    fn sidecar_resources_fall_back_and_override() {
        // Fallback: empty object and malformed JSON both yield the defaults.
        for json in ["{}", "not-json", ""] {
            assert_eq!(
                parse_resources(json),
                (
                    "100m".to_string(),
                    "256Mi".to_string(),
                    "500m".to_string(),
                    "512Mi".to_string(),
                ),
                "malformed/empty resources {json:?} must fall back to the default envelope"
            );
        }
        // Override: explicit values win per field.
        assert_eq!(
            parse_resources(
                r#"{"cpu_request":"250m","memory_request":"512Mi","cpu_limit":"1","memory_limit":"1Gi"}"#
            ),
            (
                "250m".to_string(),
                "512Mi".to_string(),
                "1".to_string(),
                "1Gi".to_string(),
            )
        );
    }

    /// AC (qut0): a backing-service sidecar has no workspace/broker access — it
    /// mounts ONLY its shared `/dev/shm` scratch, never the workspace, mirror,
    /// launcher IPC, or delegated cgroup.
    #[test]
    fn sidecar_has_no_workspace_or_broker_mounts() {
        let cfg = KubernetesConfig::for_testing();
        let c = sidecar_container(&cfg, &spec());
        let mounts = c.volume_mounts.as_ref().expect("sidecar mounts");
        assert_eq!(mounts.len(), 1, "sidecar mounts only /dev/shm");
        assert_eq!(mounts[0].name, SIDECAR_DSHM_VOLUME);
        for forbidden in [
            crate::job::VOLUME_WORKSPACE,
            crate::job::VOLUME_MIRROR,
            crate::launcher::VOLUME_LAUNCHER_IPC,
            crate::launcher::VOLUME_LAUNCHER_CGROUP,
        ] {
            assert!(
                !mounts.iter().any(|m| m.name == forbidden),
                "backing sidecar must not mount {forbidden}"
            );
        }
    }

    #[test]
    fn sidecar_is_a_native_sidecar() {
        let cfg = KubernetesConfig::for_testing();
        let c = sidecar_container(&cfg, &spec());
        // restartPolicy: Always on an initContainer == native sidecar.
        assert_eq!(c.restart_policy.as_deref(), Some("Always"));
        assert_eq!(c.name, "svc-postgres");
        assert_eq!(c.image.as_deref(), Some("postgres:18-alpine"));
        assert_eq!(c.ports.as_ref().unwrap()[0].container_port, 5432);
        // Startup probe gates the worker start on DB readiness.
        assert!(c.startup_probe.is_some());
        // Must run as the image default (root) so the service entrypoint can
        // initialise its data dir; the Pod default uid 10001 would break it.
        let sc = c
            .security_context
            .as_ref()
            .expect("container securityContext");
        assert_eq!(sc.run_as_user, Some(0));
        assert_eq!(sc.run_as_non_root, Some(false));
    }

    #[test]
    fn catalog_wrapper_digest_pins_dispatch_sidecar_image() {
        let mut services = Vec::new();
        append_preset_resolution(
            "project",
            "preset-postgres-18",
            Ok(Some(preset())),
            &mut services,
            &mut Vec::new(),
            &mut Vec::new(),
        );
        // A wrapper preset renders the digest-pinned wrapper artifact, not the
        // stock image, and is marked as a wrapper so job.rs wires the private
        // control mount + socket env.
        assert_eq!(
            services[0].image,
            "ghcr.io/djinnos/djinn-postgres-wrapper@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
        assert!(services[0].is_wrapper);
        assert_eq!(services[0].control_socket_name, "preset-postgres-18.sock");
        // The wrapper admin URL + exported env names are wired for the binary.
        assert!(
            services[0]
                .wrapper_env
                .iter()
                .any(|(k, v)| k == "POSTGRES_WRAPPER_ADMIN_URL"
                    && v == "postgres://postgres:postgres@127.0.0.1:5432/app_test")
        );
        assert!(services[0].wrapper_env.iter().any(
            |(k, v)| k == "CATALOG_POSTGRES_ENV_NAMES" && v == "DATABASE_URL,TEST_POSTGRES_URL"
        ));
    }

    /// A legacy preset with no wrapper image stays dispatch-compatible: the
    /// sidecar runs the stock image and is not marked as a wrapper.
    #[test]
    fn legacy_preset_without_wrapper_runs_stock_image() {
        let mut legacy = preset();
        legacy.wrapper_image = None;
        legacy.image_digest = None;
        let mut services = Vec::new();
        append_preset_resolution(
            "project",
            "preset-postgres-18",
            Ok(Some(legacy)),
            &mut services,
            &mut Vec::new(),
            &mut Vec::new(),
        );
        assert_eq!(services[0].image, "postgres:18-alpine");
        assert!(!services[0].is_wrapper);
        assert!(services[0].wrapper_env.is_empty());
    }

    /// AC2: strict resolution rejects a preset that has no catalog wrapper
    /// image, a mutable (digest-suffixed) wrapper reference, and two presets
    /// whose ids sanitize to a colliding control socket identity.
    #[test]
    fn strict_catalog_rejects_wrapper_and_socket_faults() {
        let mut no_wrapper = preset();
        no_wrapper.wrapper_image = None;
        assert!(matches!(
            strict_catalog_from_presets(vec![("postgres-a".into(), no_wrapper)]),
            Err(CatalogServiceResolutionError::StockImageWithoutWrapper { .. })
        ));

        let mut mutable_wrapper = preset();
        mutable_wrapper.wrapper_image =
            Some("ghcr.io/djinnos/djinn-postgres-wrapper@sha256:dead".into());
        assert!(matches!(
            strict_catalog_from_presets(vec![("postgres-a".into(), mutable_wrapper)]),
            Err(CatalogServiceResolutionError::MutableWrapperReference { .. })
        ));

        // "svc/a" and "svc:a" both sanitize to "svc_a.sock".
        let mut left = preset();
        left.service_type = "redis".into();
        left.port = 6390;
        left.conn_env_var = "LEFT_URL".into();
        let mut right = preset();
        right.service_type = "rabbitmq".into();
        right.port = 6391;
        right.conn_env_var = "RIGHT_URL".into();
        assert!(matches!(
            strict_catalog_from_presets(vec![("svc/a".into(), left), ("svc:a".into(), right)]),
            Err(CatalogServiceResolutionError::Duplicate {
                kind: "control socket identity",
                ..
            })
        ));
    }

    /// AC2: a resolved wrapper service names the exact socket the worker adapter
    /// connects to, and the pinned wrapper reference the sidecar runs.
    #[test]
    fn strict_resolution_exposes_wrapper_reference_and_socket() {
        let resolved =
            strict_catalog_from_presets(vec![("preset-postgres-18".into(), preset())]).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(
            resolved[0].wrapper_reference,
            "ghcr.io/djinnos/djinn-postgres-wrapper@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
        assert_eq!(resolved[0].control_socket_name, "preset-postgres-18.sock");
        assert_eq!(
            control_socket_path("preset-postgres-18"),
            std::path::Path::new("/var/run/djinn/service-control/preset-postgres-18.sock")
        );
    }

    #[test]
    fn strict_catalog_rejects_duplicate_and_malformed_material() {
        let valid = preset();
        assert!(matches!(
            strict_catalog_from_presets(vec![
                ("postgres-a".into(), valid.clone()),
                ("postgres-a".into(), valid.clone())
            ]),
            Err(CatalogServiceResolutionError::Duplicate {
                kind: "preset ID",
                ..
            })
        ));

        let mut duplicate_export = valid.clone();
        duplicate_export.conn_env_var = "DATABASE_URL,DATABASE_URL".into();
        assert!(matches!(
            strict_catalog_from_presets(vec![("postgres-a".into(), duplicate_export)]),
            Err(CatalogServiceResolutionError::Duplicate {
                kind: "exported environment",
                ..
            })
        ));

        let mut malformed_digest = valid.clone();
        malformed_digest.image_digest = Some("postgres:latest".into());
        assert!(matches!(
            strict_catalog_from_presets(vec![("postgres-a".into(), malformed_digest)]),
            Err(CatalogServiceResolutionError::MalformedServiceDigest { .. })
        ));

        let mut unsupported_protocol = valid.clone();
        unsupported_protocol.verification_protocol_revision = Some(2);
        assert!(matches!(
            strict_catalog_from_presets(vec![("postgres-a".into(), unsupported_protocol)]),
            Err(CatalogServiceResolutionError::UnsupportedProtocol { revision: 2, .. })
        ));

        let mut malformed_configuration = valid;
        malformed_configuration.resources = "[]".into();
        assert!(matches!(
            strict_catalog_from_presets(vec![("postgres-a".into(), malformed_configuration)]),
            Err(CatalogServiceResolutionError::MalformedConfiguration { .. })
        ));
    }

    #[test]
    fn strict_catalog_orders_reversed_rows_by_preset_id() {
        let mut redis = preset();
        redis.id = "preset-redis-7".into();
        redis.service_type = "redis".into();
        redis.port = 6379;
        redis.conn_env_var = "REDIS_URL".into();

        let postgres_row = ("preset-postgres-18".into(), preset());
        let redis_row = ("preset-redis-7".into(), redis);
        let ordered = strict_catalog_from_presets(vec![postgres_row.clone(), redis_row.clone()])
            .expect("valid catalog rows");
        let reversed = strict_catalog_from_presets(vec![redis_row, postgres_row])
            .expect("reversed valid catalog rows");

        assert_eq!(reversed, ordered);
        assert_eq!(
            reversed
                .iter()
                .map(|service| service.preset_id.as_str())
                .collect::<Vec<_>>(),
            ["preset-postgres-18", "preset-redis-7"]
        );
    }

    #[test]
    fn resolver_metadata_records_requested_and_injected_preset() {
        let requested = vec!["preset-postgres-18".to_string()];
        let mut services = Vec::new();
        let mut injected = Vec::new();
        let mut skipped = Vec::new();
        append_preset_resolution(
            "project-valid-service",
            &requested[0],
            Ok(Some(preset())),
            &mut services,
            &mut injected,
            &mut skipped,
        );
        let resolution = ImageServiceResolution {
            image: Some(ResolvedImageMetadata {
                id: "image-valid-service".into(),
                name: "image-valid-service".into(),
                tag: Some("registry.local/image:tag".into()),
            }),
            requested_preset_ids: requested,
            injected,
            skipped,
            lookup_error: None,
            services,
        };

        assert_eq!(resolution.requested_preset_ids, vec!["preset-postgres-18"]);
        assert_eq!(resolution.services.len(), 1);
        assert_eq!(resolution.injected.len(), 1);
        assert_eq!(resolution.injected[0].preset_id, "preset-postgres-18");
        assert_eq!(resolution.injected[0].service_type, "postgres");
        assert_eq!(resolution.injected[0].port, 5432);
        assert!(resolution.skipped.is_empty());
        assert!(resolution.lookup_error.is_none());
    }

    #[test]
    fn resolver_metadata_records_unknown_preset_as_skipped() {
        let requested = vec!["preset-does-not-exist".to_string()];
        let mut services = Vec::new();
        let mut injected = Vec::new();
        let mut skipped = Vec::new();
        append_preset_resolution(
            "project-missing-service",
            &requested[0],
            Ok(None),
            &mut services,
            &mut injected,
            &mut skipped,
        );
        let resolution = ImageServiceResolution {
            image: Some(ResolvedImageMetadata {
                id: "image-missing-service".into(),
                name: "image-missing-service".into(),
                tag: None,
            }),
            requested_preset_ids: requested,
            injected,
            skipped,
            lookup_error: None,
            services,
        };

        assert_eq!(
            resolution.requested_preset_ids,
            vec!["preset-does-not-exist"]
        );
        assert!(resolution.services.is_empty());
        assert!(resolution.injected.is_empty());
        assert_eq!(resolution.skipped.len(), 1);
        assert_eq!(resolution.skipped[0].preset_id, "preset-does-not-exist");
        assert!(resolution.skipped[0].reason.contains("unknown"));
    }

    // ---- hgd0 Wave 1 transport regression tests ----------------------------

    /// AC4: `sidecar_conn_env` for a preset with `conn_env_var` containing
    /// `TEST_POSTGRES_URL` emits that env var with the rendered loopback
    /// connection string.  These are static connection env vars — the preset
    /// mechanism does not attach or execute any pre-task commands.
    #[test]
    fn conn_env_test_postgres_url_from_multi_name_preset() {
        let s = spec(); // conn_env_var: "DATABASE_URL,TEST_POSTGRES_URL"
        let envs = sidecar_conn_env(&s);
        assert_eq!(envs.len(), 2);

        let expected = "postgres://postgres:postgres@127.0.0.1:5432/app_test";
        assert_eq!(envs[0].name, "DATABASE_URL");
        assert_eq!(envs[0].value.as_deref(), Some(expected));
        assert_eq!(envs[1].name, "TEST_POSTGRES_URL");
        assert_eq!(envs[1].value.as_deref(), Some(expected));

        // Both values are connection URLs, not commands.
        for env in &envs {
            let value = env.value.as_deref().unwrap_or("");
            assert!(
                value.starts_with("postgres://"),
                "conn env must be a URL, not a command: {}={}",
                env.name,
                value
            );
        }
    }

    /// AC4: `BackingServiceSpec` is a connection-injection primitive — it has
    /// no fields for pre-task commands, lifecycle hooks, or any command
    /// execution metadata.  This type-level regression guard ensures that
    /// service presets remain purely about connection string injection and
    /// don't accidentally acquire lifecycle coupling.
    #[test]
    fn backing_service_spec_has_no_pretask_command_fields() {
        let s = spec();
        // The only env vars produced by sidecar_conn_env are from
        // conn_env_var + conn_template — no lifecycle commands.
        let envs = sidecar_conn_env(&s);
        for env in &envs {
            // Connection env vars are rendered URL strings.
            let value = env.value.as_deref().unwrap_or("");
            assert!(
                value.starts_with("postgres://"),
                "service preset env must be a connection URL: {}={}",
                env.name,
                value
            );
        }

        // The sidecar container uses the image's default entrypoint,
        // not an injected command.
        let cfg = crate::config::KubernetesConfig::for_testing();
        let container = sidecar_container(&cfg, &s);
        assert!(
            container.command.is_none(),
            "sidecar container must not override entrypoint with commands"
        );
        // The sidecar's env is the service's own env (e.g. POSTGRES_PASSWORD),
        // not lifecycle pre-task commands.
        let sidecar_env = container.env.as_ref().unwrap();
        for env in sidecar_env {
            assert_ne!(
                env.name, "DJINN_PRE_TASK",
                "sidecar env must not contain pre-task command variables"
            );
        }
    }

    /// AC4: When a service preset resolves to an injected sidecar, the
    /// `InjectedServiceMetadata` carries exactly `preset_id`, `service_type`,
    /// `port`, and `conn_env_var` — no lifecycle/pre-task metadata leaks
    /// into the metadata the runtime logs as `task_run_services_resolved`.
    #[test]
    fn injected_service_metadata_has_no_pretask_fields() {
        let requested = ["preset-postgres-18".to_string()];
        let mut services = Vec::new();
        let mut injected = Vec::new();
        let mut skipped = Vec::new();
        append_preset_resolution(
            "project-metadata-test",
            &requested[0],
            Ok(Some(preset())),
            &mut services,
            &mut injected,
            &mut skipped,
        );

        assert_eq!(injected.len(), 1);
        let meta = &injected[0];
        // The metadata carries exactly these four fields.
        assert_eq!(meta.preset_id, "preset-postgres-18");
        assert_eq!(meta.service_type, "postgres");
        assert_eq!(meta.port, 5432);
        assert_eq!(meta.conn_env_var, "DATABASE_URL,TEST_POSTGRES_URL");

        // No pre-task command field exists on InjectedServiceMetadata.
        // (This is a compile-time guarantee from the struct definition,
        // but we also verify the serialized JSON has no unexpected keys.)
        let json = serde_json::to_value(meta).unwrap();
        let obj = json.as_object().expect("serialized as object");
        assert_eq!(
            obj.len(),
            4,
            "exactly 4 fields: preset_id, service_type, port, conn_env_var"
        );
        assert!(obj.contains_key("preset_id"));
        assert!(obj.contains_key("service_type"));
        assert!(obj.contains_key("port"));
        assert!(obj.contains_key("conn_env_var"));
    }
}
