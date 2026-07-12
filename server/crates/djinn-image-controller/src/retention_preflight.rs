//! Retention preflight — fail-closed gating before destructive Zot retention.
//!
//! Composes three inputs:
//! 1. **Zot registry state** — fetched via a mockable trait
//!    ([`ZotStateSource`]) so tests never need a live registry.
//! 2. **Selected catalog-image DB state** — from
//!    `ImageRepository::list_selected_catalog_images` (task `tqh7`).
//! 3. **The pure retention planner** ([`crate::retention::plan_retention`]).
//!
//! When destructive retention is requested (or would be enabled), preflight
//! runs the planner and checks whether every currently selected catalog image
//! remains pullable by retained tag, digest pin, or alias. If any image cannot
//! be proven pullable, preflight **fails closed** — returns an `Err` and does
//! NOT proceed.

use crate::retention::{
    self, CATALOG_REPO_PREFIX, RetentionPlan, SelectedImage, ZotRepository, ZotTag,
};
use djinn_provider::http_util::{HttpClient, HttpError, HttpRequestBuilder, HttpResponse};
use serde::Deserialize;
use std::time::Duration;

/// A mockable source of Zot repository/tag state.
///
/// In production this is backed by Zot's HTTP API (or a kubectl exec into the
/// Zot pod). In tests, implement this trait with canned data.
#[async_trait::async_trait]
pub trait ZotStateSource: Send + Sync {
    /// Fetch all repositories and their tags from Zot.
    async fn fetch_repositories(&self) -> Result<Vec<ZotRepository>, ZotStateError>;
}

/// Errors that can arise when fetching Zot state.
#[derive(Debug, thiserror::Error)]
pub enum ZotStateError {
    #[error("zot state fetch failed: {0}")]
    Fetch(String),
    #[error("zot state parse error: {0}")]
    Parse(String),
    #[error("zot transport failure during {operation}: {message}")]
    Transport { operation: String, message: String },
    #[error("zot returned HTTP {status} during {operation}")]
    Status { operation: String, status: u16 },
    #[error("zot returned incomplete state for {repo}:{tag}: missing {field}")]
    Incomplete {
        repo: String,
        tag: String,
        field: &'static str,
    },
}

/// Explicit authentication for Zot API requests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ZotHttpAuth {
    None,
    Basic { username: String, password: String },
    Bearer(String),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZotHttpConfig {
    pub endpoint: String,
    pub auth: ZotHttpAuth,
    pub request_timeout: Duration,
}
impl ZotHttpConfig {
    pub fn new(endpoint: impl Into<String>, auth: ZotHttpAuth) -> Self {
        Self {
            endpoint: endpoint.into().trim_end_matches('/').to_owned(),
            auth,
            request_timeout: Duration::from_secs(15),
        }
    }
}
/// Production Zot Distribution client; it maps only `djinn-image-*` repos.
#[derive(Clone)]
pub struct ZotHttpStateSource {
    config: ZotHttpConfig,
    client: HttpClient,
}
impl ZotHttpStateSource {
    pub fn new(config: ZotHttpConfig) -> Result<Self, ZotStateError> {
        let client = HttpClient::new(config.request_timeout)?;
        Ok(Self { config, client })
    }
    fn request(&self, url: &str) -> HttpRequestBuilder {
        let request = self
            .client
            .get(url)
            .header("Accept", "application/vnd.oci.image.manifest.v1+json");
        match &self.config.auth {
            ZotHttpAuth::None => request,
            ZotHttpAuth::Basic { username, password } => request.basic_auth(username, password),
            ZotHttpAuth::Bearer(token) => request.bearer_auth(token),
        }
    }
    async fn response(&self, url: &str, op: &'static str) -> Result<HttpResponse, ZotStateError> {
        self.request(url)
            .send(op)
            .await
            .map_err(ZotStateError::from)
    }
    async fn names(
        &self,
        mut next: Option<String>,
        op: &'static str,
        key: &'static str,
    ) -> Result<Vec<String>, ZotStateError> {
        let mut names = Vec::new();
        while let Some(url) = next.take() {
            let response = self.response(&url, op).await?;
            let link = response
                .header("Link")
                .as_deref()
                .and_then(next_link)
                .map(str::to_owned);
            let body: serde_json::Value = response.json().await?;
            let entries = body
                .get(key)
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| ZotStateError::Parse(format!("missing array field {key}")))?;
            for value in entries {
                names.push(
                    value
                        .as_str()
                        .ok_or_else(|| ZotStateError::Parse(format!("non-string {key}")))?
                        .to_owned(),
                );
            }
            next = link.map(|url| {
                if url.starts_with("http") {
                    url
                } else {
                    format!("{}{}", self.config.endpoint, url)
                }
            });
        }
        names.sort();
        names.dedup();
        Ok(names)
    }
    async fn tag(&self, repo: &str, tag: &str) -> Result<ZotTag, ZotStateError> {
        let response = self
            .response(
                &format!("{}/v2/{repo}/manifests/{tag}", self.config.endpoint),
                "manifest",
            )
            .await?;
        let digest = response
            .header("Docker-Content-Digest")
            .filter(|v| !v.is_empty())
            .ok_or_else(|| ZotStateError::Incomplete {
                repo: repo.into(),
                tag: tag.into(),
                field: "Docker-Content-Digest",
            })?;
        let bytes = response.bytes("manifest").await?;
        let manifest: Manifest =
            serde_json::from_slice(&bytes).map_err(|e| ZotStateError::Parse(e.to_string()))?;
        let config = manifest.config.ok_or_else(|| ZotStateError::Incomplete {
            repo: repo.into(),
            tag: tag.into(),
            field: "manifest config",
        })?;
        let config_size = config.size.ok_or_else(|| ZotStateError::Incomplete {
            repo: repo.into(),
            tag: tag.into(),
            field: "manifest config size",
        })?;
        let image_config: ImageConfig = self
            .response(
                &format!("{}/v2/{repo}/blobs/{}", self.config.endpoint, config.digest),
                "image config",
            )
            .await?
            .json()
            .await?;
        let pushed_at = image_config
            .created
            .filter(|v| !v.is_empty())
            .ok_or_else(|| ZotStateError::Incomplete {
                repo: repo.into(),
                tag: tag.into(),
                field: "config created timestamp",
            })?;
        let layers = manifest.layers.ok_or_else(|| ZotStateError::Incomplete {
            repo: repo.into(),
            tag: tag.into(),
            field: "manifest layers",
        })?;
        let layer_size = layers.into_iter().try_fold(0_u64, |total, layer| {
            let size = layer.size.ok_or_else(|| ZotStateError::Incomplete {
                repo: repo.into(),
                tag: tag.into(),
                field: "manifest layer size",
            })?;
            Ok::<_, ZotStateError>(total + size)
        })?;
        Ok(ZotTag {
            tag: tag.into(),
            digest,
            size_bytes: bytes.len() as u64 + config_size + layer_size,
            pushed_at,
        })
    }
}
#[async_trait::async_trait]
impl ZotStateSource for ZotHttpStateSource {
    async fn fetch_repositories(&self) -> Result<Vec<ZotRepository>, ZotStateError> {
        let repositories = self
            .names(
                Some(format!("{}/v2/_catalog?n=100", self.config.endpoint)),
                "catalog",
                "repositories",
            )
            .await?;
        let mut result = Vec::new();
        for name in repositories
            .into_iter()
            .filter(|name| name.starts_with(CATALOG_REPO_PREFIX))
        {
            let names = self
                .names(
                    Some(format!(
                        "{}/v2/{name}/tags/list?n=100",
                        self.config.endpoint
                    )),
                    "tag list",
                    "tags",
                )
                .await?;
            let mut tags = Vec::new();
            for tag in names {
                tags.push(self.tag(&name, &tag).await?);
            }
            result.push(ZotRepository { name, tags });
        }
        Ok(result)
    }
}
impl From<HttpError> for ZotStateError {
    fn from(e: HttpError) -> Self {
        match e {
            HttpError::Build { message } => ZotStateError::Transport {
                operation: "client construction".to_string(),
                message,
            },
            HttpError::Transport { operation, message } => {
                ZotStateError::Transport { operation, message }
            }
            HttpError::Status { operation, status } => ZotStateError::Status { operation, status },
            HttpError::Body { operation, message } => {
                ZotStateError::Transport { operation, message }
            }
            HttpError::Parse { message } => ZotStateError::Parse(message),
        }
    }
}

fn next_link(value: &str) -> Option<&str> {
    let part = value
        .split(',')
        .find(|part| part.contains("rel=\"next\""))?;
    let start = part.find('<')? + 1;
    let end = part[start..].find('>')? + start;
    Some(&part[start..end])
}
#[derive(Deserialize)]
struct Manifest {
    config: Option<Descriptor>,
    layers: Option<Vec<Descriptor>>,
}
#[derive(Deserialize)]
struct Descriptor {
    digest: String,
    size: Option<u64>,
}
#[derive(Deserialize)]
struct ImageConfig {
    created: Option<String>,
}

/// Preflight configuration for retention gating.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetentionPreflightConfig {
    pub enabled: bool,
    /// Whether destructive deletion is enabled in the Helm/operator config.
    /// Only meaningful when `enabled` is `true`. When `false`, preflight is
    /// advisory (dry-run report only, never blocks). When `true`, preflight
    /// runs the full fail-closed safety check.
    pub destructive_enabled: bool,
    /// Number of newest tags to retain per catalog repo.
    pub newest_tags: usize,
}

impl Default for RetentionPreflightConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            destructive_enabled: false,
            newest_tags: 5,
        }
    }
}

impl RetentionPreflightConfig {
    /// Derive the bounded preflight mode from this configuration.
    pub fn mode(&self) -> PreflightMode {
        if !self.enabled {
            PreflightMode::Disabled
        } else if self.destructive_enabled {
            PreflightMode::Destructive
        } else {
            PreflightMode::DryRun
        }
    }
}

/// Bounded retention preflight mode — one value per operational state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreflightMode {
    /// Retention is disabled — preflight is a no-op.
    Disabled,
    /// Advisory dry-run — report only, never blocks.
    DryRun,
    /// Destructive — fail-closed safety gate is active.
    Destructive,
}

impl PreflightMode {
    /// Stable lowercase label for logs/reports/metrics.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::DryRun => "dry_run",
            Self::Destructive => "destructive",
        }
    }
}

/// Bounded retention preflight outcome — one value per deterministic result
/// state. Covers all five states required by the cross-path observability
/// matrix: disabled, advisory dry-run, safe destructive, blocked destructive,
/// and fetch/state error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreflightOutcomeKind {
    /// Retention is disabled — no plan computed, no Zot contact.
    Disabled,
    /// Dry-run advisory report produced (plan may be safe or unsafe).
    Advisory,
    /// Destructive mode and all selected images are proven pullable.
    DestructiveSafe,
    /// Destructive mode but one or more selected images are unsafe — blocked.
    DestructiveBlocked,
    /// Zot state fetch or DB enumeration failed — fail-closed.
    FetchError,
}

impl PreflightOutcomeKind {
    /// Stable lowercase label for logs/reports/metrics.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Advisory => "advisory",
            Self::DestructiveSafe => "destructive_safe",
            Self::DestructiveBlocked => "destructive_blocked",
            Self::FetchError => "fetch_error",
        }
    }
}

/// Outcome of a retention preflight check.
#[derive(Debug)]
pub struct PreflightOutcome {
    /// The computed retention plan (always available, even in dry-run).
    pub plan: RetentionPlan,
    /// Bounded preflight mode (disabled/dry-run/destructive).
    pub mode: PreflightMode,
    /// Bounded deterministic outcome kind.
    pub outcome_kind: PreflightOutcomeKind,
    /// Whether this preflight would block rollout (true only when destructive
    /// AND unsafe images exist).
    pub blocks_rollout: bool,
    /// Human-readable dry-run report.
    pub report: String,
}

/// Preflight error — returned when the preflight itself fails (e.g. Zot fetch
/// error), or when fail-closed safety blocks destructive retention.
#[derive(Debug, thiserror::Error)]
pub enum PreflightError {
    /// Could not fetch Zot state.
    #[error("zot state error: {0}")]
    ZotState(#[from] ZotStateError),

    /// Destructive retention is enabled but selected images cannot be proven
    /// pullable. This is the fail-closed block.
    #[error(
        "destructive retention blocked: {count} selected image(s) cannot be proven pullable after retention"
    )]
    UnsafeSelectedImages {
        count: usize,
        /// The retention plan with the full analysis.
        plan: RetentionPlan,
        /// The deterministic dry-run report (so callers can surface it even
        /// when preflight returns `Err`).
        report: String,
    },
}

type PreflightResult = std::result::Result<PreflightOutcome, PreflightError>;

/// Run the retention preflight.
///
/// This is the main entry point. It fetches Zot state, enumerates selected
/// catalog images from the DB, runs the planner, and enforces fail-closed
/// behavior.
///
/// - When `cfg.enabled` is `false`, returns a [`PreflightMode::Disabled`] /
///   [`PreflightOutcomeKind::Disabled`] outcome with an empty plan and no Zot
///   contact.
/// - When `cfg.destructive_enabled` is `false` (dry-run), the outcome is
///   advisory: a dry-run report is produced but nothing blocks.
/// - When `cfg.destructive_enabled` is `true` and any selected image is unsafe,
///   returns [`PreflightError::UnsafeSelectedImages`] (fail-closed).
pub async fn run_preflight(
    zot: &dyn ZotStateSource,
    selected: &[SelectedImage],
    cfg: &RetentionPreflightConfig,
) -> PreflightResult {
    let mode = cfg.mode();

    // Disabled: no Zot contact, no DB enumeration. Produce a bounded disabled
    // outcome so the observability matrix has a deterministic representation.
    if mode == PreflightMode::Disabled {
        let plan = retention::plan_retention(&[], &[], &retention::RetentionPolicy::default());
        return Ok(PreflightOutcome {
            plan,
            mode,
            outcome_kind: PreflightOutcomeKind::Disabled,
            blocks_rollout: false,
            report: render_disabled_report(),
        });
    }

    // Fetch Zot state — errors propagate (fail-closed on fetch failure).
    let repos = zot.fetch_repositories().await?;

    let policy = retention::RetentionPolicy {
        newest_tags: cfg.newest_tags,
    };
    let plan = retention::plan_retention(&repos, selected, &policy);
    let report = render_mode_report(&plan, mode);

    if mode == PreflightMode::Destructive && !plan.is_safe {
        return Err(PreflightError::UnsafeSelectedImages {
            count: plan.unsafe_images.len(),
            plan,
            report,
        });
    }

    let blocks_rollout = mode == PreflightMode::Destructive && !plan.is_safe;

    let outcome_kind = if mode == PreflightMode::Destructive {
        PreflightOutcomeKind::DestructiveSafe
    } else {
        PreflightOutcomeKind::Advisory
    };

    Ok(PreflightOutcome {
        plan,
        mode,
        outcome_kind,
        blocks_rollout,
        report,
    })
}

/// Render the disabled-mode report — a bounded, deterministic representation
/// that does not require a live registry.
fn render_disabled_report() -> String {
    let mut out = String::new();
    out.push_str("=== Zot Catalog Retention Preflight Report ===\n\n");
    out.push_str("Mode: disabled\n");
    out.push_str("Outcome: disabled\n");
    out.push_str("Retention is not enabled; no Zot contact or plan computed.\n");
    out
}

/// Wrap the pure planner report with a bounded mode/outcome header so the
/// operator-facing surface always identifies the preflight mode and
/// deterministic result kind alongside the candidate/byte/safety detail.
fn render_mode_report(plan: &RetentionPlan, mode: PreflightMode) -> String {
    let outcome_kind = if mode == PreflightMode::Destructive {
        if plan.is_safe {
            PreflightOutcomeKind::DestructiveSafe
        } else {
            PreflightOutcomeKind::DestructiveBlocked
        }
    } else {
        PreflightOutcomeKind::Advisory
    };

    let mut out = String::new();
    out.push_str("=== Zot Catalog Retention Preflight Report ===\n\n");
    out.push_str(&format!("Mode: {}\n", mode.label()));
    out.push_str(&format!("Outcome: {}\n", outcome_kind.label()));

    // Aggregate candidate tag counts across all repos.
    let retained_count: usize = plan.repos.iter().map(|r| r.retained.len()).sum();
    let deleted_count: usize = plan.repos.iter().map(|r| r.deleted.len()).sum();
    let candidate_count = retained_count + deleted_count;
    out.push_str(&format!("Candidate tags: {candidate_count}\n"));
    out.push_str(&format!("Retained tags: {retained_count}\n"));
    out.push_str(&format!("Deleted tags: {deleted_count}\n"));
    out.push_str(&format!(
        "Projected reclaimed bytes: {}\n",
        plan.projected_reclaimed_bytes
    ));
    out.push_str(&format!(
        "Projected retained bytes: {}\n",
        plan.projected_retained_bytes
    ));
    out.push_str(&format!(
        "Safe: {}\n",
        if plan.is_safe { "yes" } else { "NO" }
    ));
    out.push_str(&format!("Selected images safe: {}\n", plan.safe_pins.len()));
    out.push_str(&format!(
        "Selected images unsafe: {}\n\n",
        plan.unsafe_images.len()
    ));

    // Append the detailed per-repo / pin / unsafe sections from the pure
    // planner report (without re-emitting its header/summary lines).
    out.push_str(&retention::render_report(plan));
    out
}

/// Convert DB-level [`djinn_db::SelectedCatalogImage`] records into the pure
/// [`SelectedImage`] shape the planner expects.
///
/// The DB `tag` column stores the full registry ref (e.g.
/// `reg/djinn-image-i1:hash`), but Zot tags are just the suffix (`hash`).
/// This function extracts the bare tag name so planner tag comparison works.
pub fn adapt_selected_images(db_rows: &[djinn_db::SelectedCatalogImage]) -> Vec<SelectedImage> {
    db_rows
        .iter()
        .map(|r| SelectedImage {
            image_id: r.image_id.clone(),
            repo: format!("{CATALOG_REPO_PREFIX}{}", r.image_id),
            tag: r.tag.as_deref().map(extract_tag_name),
            digest: r.registry_digest.clone(),
            status: r.status.clone(),
        })
        .collect()
}

/// Build a [`RetentionPreflightConfig`] from the Helm-rendered retention
/// settings. This is how the controller reads `mrgt`'s Helm values at runtime.
pub fn preflight_config_from_helm(
    enabled: bool,
    destructive_enabled: bool,
    newest_tags: usize,
) -> RetentionPreflightConfig {
    RetentionPreflightConfig {
        enabled,
        destructive_enabled,
        newest_tags,
    }
}

/// Derive the catalog repo name for an image id (matching
/// `format_catalog_image_tag` / `BuildSubject::tag_repo_segment`).
pub fn catalog_repo_name(image_id: &str) -> String {
    format!("{CATALOG_REPO_PREFIX}{image_id}")
}

/// Extract the bare tag name from a full image ref like
/// `reg/djinn-image-i1:hash` → `hash`. Only a `:` after the last `/` is a
/// tag separator (registry `host:port` prefixes must not be mistaken).
fn extract_tag_name(full_ref: &str) -> String {
    let last_slash = full_ref.rfind('/').map(|i| i + 1).unwrap_or(0);
    match full_ref[last_slash..].find(':') {
        Some(rel) => full_ref[last_slash + rel + 1..].to_string(),
        None => full_ref[last_slash..].to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retention::ZotTag;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    struct HttpResponse {
        path: &'static str,
        status: u16,
        headers: Vec<(&'static str, &'static str)>,
        body: &'static str,
        authorization: Option<&'static str>,
    }

    async fn mock_http(responses: Vec<HttpResponse>) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for response in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut buffer = [0; 1024];
                    let count = stream.read(&mut buffer).await.unwrap();
                    assert!(count > 0, "client closed before sending HTTP headers");
                    request.extend_from_slice(&buffer[..count]);
                    if request.windows(4).any(|part| part == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8(request).unwrap();
                assert!(request.starts_with(&format!("GET {} HTTP/1.1", response.path)));
                if let Some(auth) = response.authorization {
                    assert!(
                        request
                            .to_ascii_lowercase()
                            .contains(&format!("authorization: {}\r\n", auth.to_ascii_lowercase()))
                    );
                }
                let reason = if response.status == 200 {
                    "OK"
                } else {
                    "Error"
                };
                let mut wire = format!(
                    "HTTP/1.1 {} {reason}\r\nConnection: close\r\nContent-Length: {}\r\n",
                    response.status,
                    response.body.len()
                );
                for (name, value) in response.headers {
                    wire.push_str(&format!("{name}: {value}\r\n"));
                }
                wire.push_str("\r\n");
                wire.push_str(response.body);
                stream.write_all(wire.as_bytes()).await.unwrap();
            }
        });
        (endpoint, server)
    }

    fn ok(path: &'static str, body: &'static str) -> HttpResponse {
        HttpResponse {
            path,
            status: 200,
            headers: vec![],
            body,
            authorization: None,
        }
    }

    #[tokio::test]
    async fn http_source_paginates_filters_and_maps_catalog_state() {
        let new_manifest = r#"{"config":{"digest":"sha256:config-new","size":11},"layers":[{"digest":"sha256:layer-new","size":21}]}"#;
        let old_manifest = r#"{"config":{"digest":"sha256:config-old","size":10},"layers":[{"digest":"sha256:layer-old","size":20}]}"#;
        let (endpoint, server) = mock_http(vec![
            HttpResponse {
                path: "/v2/_catalog?n=100",
                status: 200,
                headers: vec![(
                    "Link",
                    "</v2/_catalog?n=100&last=djinn-image-alpha>; rel=\"next\"",
                )],
                body: r#"{"repositories":["buildkit-cache","djinn-image-alpha"]}"#,
                authorization: None,
            },
            ok(
                "/v2/_catalog?n=100&last=djinn-image-alpha",
                r#"{"repositories":["unrelated","djinn-image-zed"]}"#,
            ),
            HttpResponse {
                path: "/v2/djinn-image-alpha/tags/list?n=100",
                status: 200,
                headers: vec![(
                    "Link",
                    "</v2/djinn-image-alpha/tags/list?n=100&last=old>; rel=\"next\"",
                )],
                body: r#"{"tags":["old"]}"#,
                authorization: None,
            },
            ok(
                "/v2/djinn-image-alpha/tags/list?n=100&last=old",
                r#"{"tags":["new"]}"#,
            ),
            HttpResponse {
                path: "/v2/djinn-image-alpha/manifests/new",
                status: 200,
                headers: vec![("Docker-Content-Digest", "sha256:new")],
                body: new_manifest,
                authorization: None,
            },
            ok(
                "/v2/djinn-image-alpha/blobs/sha256:config-new",
                r#"{"created":"2024-01-02T00:00:00Z"}"#,
            ),
            HttpResponse {
                path: "/v2/djinn-image-alpha/manifests/old",
                status: 200,
                headers: vec![("Docker-Content-Digest", "sha256:old")],
                body: old_manifest,
                authorization: None,
            },
            ok(
                "/v2/djinn-image-alpha/blobs/sha256:config-old",
                r#"{"created":"2024-01-01T00:00:00Z"}"#,
            ),
            ok("/v2/djinn-image-zed/tags/list?n=100", r#"{"tags":[]}"#),
        ])
        .await;
        let source =
            ZotHttpStateSource::new(ZotHttpConfig::new(endpoint, ZotHttpAuth::None)).unwrap();
        let repos = source.fetch_repositories().await.unwrap();
        server.await.unwrap();
        assert_eq!(repos.len(), 2);
        assert_eq!(repos[0].name, "djinn-image-alpha");
        assert_eq!(
            repos[0]
                .tags
                .iter()
                .map(|tag| tag.tag.as_str())
                .collect::<Vec<_>>(),
            ["new", "old"]
        );
        assert_eq!(repos[0].tags[0].digest, "sha256:new");
        assert_eq!(repos[0].tags[0].pushed_at, "2024-01-02T00:00:00Z");
        assert_eq!(repos[0].tags[0].size_bytes, new_manifest.len() as u64 + 32);
        assert_eq!(repos[1].name, "djinn-image-zed");
    }

    #[tokio::test]
    async fn http_source_sends_configured_authentication() {
        let (endpoint, server) = mock_http(vec![HttpResponse {
            authorization: Some("Basic YWxpY2U6c2VjcmV0"),
            ..ok("/v2/_catalog?n=100", r#"{"repositories":[]}"#)
        }])
        .await;
        let source = ZotHttpStateSource::new(ZotHttpConfig::new(
            endpoint,
            ZotHttpAuth::Basic {
                username: "alice".into(),
                password: "secret".into(),
            },
        ))
        .unwrap();
        assert!(source.fetch_repositories().await.unwrap().is_empty());
        server.await.unwrap();

        let (endpoint, server) = mock_http(vec![HttpResponse {
            authorization: Some("Bearer token-123"),
            ..ok("/v2/_catalog?n=100", r#"{"repositories":[]}"#)
        }])
        .await;
        let source = ZotHttpStateSource::new(ZotHttpConfig::new(
            endpoint,
            ZotHttpAuth::Bearer("token-123".into()),
        ))
        .unwrap();
        assert!(source.fetch_repositories().await.unwrap().is_empty());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn http_source_returns_typed_errors_for_bad_http_responses() {
        let (endpoint, server) = mock_http(vec![ok("/v2/_catalog?n=100", "not-json")]).await;
        let source =
            ZotHttpStateSource::new(ZotHttpConfig::new(endpoint, ZotHttpAuth::None)).unwrap();
        assert!(matches!(
            source.fetch_repositories().await,
            Err(ZotStateError::Parse(_))
        ));
        server.await.unwrap();

        let (endpoint, server) = mock_http(vec![HttpResponse {
            status: 503,
            ..ok("/v2/_catalog?n=100", "unavailable")
        }])
        .await;
        let source =
            ZotHttpStateSource::new(ZotHttpConfig::new(endpoint, ZotHttpAuth::None)).unwrap();
        assert!(matches!(
            source.fetch_repositories().await,
            Err(ZotStateError::Status {
                operation,
                status: 503
            }) if operation == "catalog"
        ));
        server.await.unwrap();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let source =
            ZotHttpStateSource::new(ZotHttpConfig::new(endpoint, ZotHttpAuth::None)).unwrap();
        assert!(matches!(
            source.fetch_repositories().await,
            Err(ZotStateError::Transport {
                operation,
                ..
            }) if operation == "catalog"
        ));
    }

    #[tokio::test]
    async fn http_source_rejects_missing_descriptor_size() {
        let (endpoint, server) = mock_http(vec![
            ok("/v2/_catalog?n=100", r#"{"repositories":["djinn-image-alpha"]}"#),
            ok("/v2/djinn-image-alpha/tags/list?n=100", r#"{"tags":["v1"]}"#),
            HttpResponse { path: "/v2/djinn-image-alpha/manifests/v1", status: 200, headers: vec![("Docker-Content-Digest", "sha256:v1")], body: r#"{"config":{"digest":"sha256:config"},"layers":[{"digest":"sha256:layer","size":20}]}"#, authorization: None },
        ]).await;
        let source =
            ZotHttpStateSource::new(ZotHttpConfig::new(endpoint, ZotHttpAuth::None)).unwrap();
        assert!(matches!(
            source.fetch_repositories().await,
            Err(ZotStateError::Incomplete {
                field: "manifest config size",
                ..
            })
        ));
        server.await.unwrap();
    }

    // ── Mock Zot state source ─────────────────────────────────────────────

    struct MockZot {
        repos: Vec<ZotRepository>,
    }

    #[async_trait::async_trait]
    impl ZotStateSource for MockZot {
        async fn fetch_repositories(&self) -> Result<Vec<ZotRepository>, ZotStateError> {
            Ok(self.repos.clone())
        }
    }

    struct FailingZot;

    #[async_trait::async_trait]
    impl ZotStateSource for FailingZot {
        async fn fetch_repositories(&self) -> Result<Vec<ZotRepository>, ZotStateError> {
            Err(ZotStateError::Fetch("connection refused".into()))
        }
    }

    fn ztag(name: &str, digest: &str, size: u64, pushed: &str) -> ZotTag {
        ZotTag {
            tag: name.into(),
            digest: digest.into(),
            size_bytes: size,
            pushed_at: pushed.into(),
        }
    }

    fn zrepo(name: &str, tags: Vec<ZotTag>) -> ZotRepository {
        ZotRepository {
            name: name.into(),
            tags,
        }
    }

    fn sel(image_id: &str, tag: Option<&str>, digest: Option<&str>, status: &str) -> SelectedImage {
        SelectedImage {
            image_id: image_id.into(),
            repo: catalog_repo_name(image_id),
            tag: tag.map(String::from),
            digest: digest.map(String::from),
            status: status.into(),
        }
    }

    // ── Safe rollout ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn safe_rollout_when_destructive_and_all_images_safe() {
        let zot = MockZot {
            repos: vec![zrepo(
                "djinn-image-i1",
                vec![
                    ztag("keep", "sha256:keep", 500, "2024-01-02"),
                    ztag("drop", "sha256:drop", 300, "2024-01-01"),
                ],
            )],
        };
        let selected = vec![sel("i1", Some("keep"), Some("sha256:keep"), "ready")];
        let cfg = RetentionPreflightConfig {
            enabled: true,
            destructive_enabled: true,
            newest_tags: 1,
        };
        let outcome = run_preflight(&zot, &selected, &cfg).await.unwrap();
        assert!(!outcome.blocks_rollout);
        assert!(outcome.plan.is_safe);
    }

    // ── Unsafe selected-image blocking ────────────────────────────────────

    #[tokio::test]
    async fn blocks_when_destructive_and_image_unsafe() {
        let zot = MockZot {
            repos: vec![zrepo(
                "djinn-image-i1",
                vec![
                    ztag("new", "sha256:new", 200, "2024-01-02"),
                    ztag("old", "sha256:old", 100, "2024-01-01"),
                ],
            )],
        };
        // Selected image's tag "old" will be deleted and its digest is unique.
        let selected = vec![sel("i1", Some("old"), Some("sha256:old"), "ready")];
        let cfg = RetentionPreflightConfig {
            enabled: true,
            destructive_enabled: true,
            newest_tags: 1,
        };
        let err = run_preflight(&zot, &selected, &cfg).await.unwrap_err();
        assert!(
            matches!(err, PreflightError::UnsafeSelectedImages { count, .. } if count == 1),
            "destructive retention must fail closed when an image is unsafe"
        );
    }

    #[tokio::test]
    async fn does_not_block_when_destructive_disabled_even_if_unsafe() {
        // Dry-run mode: report the plan but don't block.
        let zot = MockZot {
            repos: vec![zrepo(
                "djinn-image-i1",
                vec![
                    ztag("new", "sha256:new", 200, "2024-01-02"),
                    ztag("old", "sha256:old", 100, "2024-01-01"),
                ],
            )],
        };
        let selected = vec![sel("i1", Some("old"), Some("sha256:old"), "ready")];
        let cfg = RetentionPreflightConfig {
            enabled: true,
            destructive_enabled: false, // dry-run
            newest_tags: 1,
        };
        let outcome = run_preflight(&zot, &selected, &cfg).await.unwrap();
        assert!(!outcome.blocks_rollout, "dry-run must never block");
        assert!(
            !outcome.plan.is_safe,
            "plan should still flag the unsafe image in the report"
        );
        assert!(outcome.report.contains("UNSAFE"));
    }

    // ── Destructive-disabled / dry-run behavior ───────────────────────────

    #[tokio::test]
    async fn dry_run_report_has_all_sections() {
        let zot = MockZot {
            repos: vec![zrepo(
                "djinn-image-i1",
                vec![
                    ztag("keep", "sha256:keep", 500, "2024-01-02"),
                    ztag("drop", "sha256:drop", 300, "2024-01-01"),
                ],
            )],
        };
        let selected = vec![sel("i1", Some("keep"), Some("sha256:keep"), "ready")];
        let cfg = RetentionPreflightConfig {
            enabled: true,
            destructive_enabled: false,
            newest_tags: 1,
        };
        let outcome = run_preflight(&zot, &selected, &cfg).await.unwrap();
        assert!(outcome.report.contains("Retained tags"));
        assert!(outcome.report.contains("Deleted tags"));
        assert!(outcome.report.contains("Selected-image pins"));
        assert!(outcome.report.contains("Projected reclaimed bytes: 300"));
        assert!(outcome.report.contains("Projected retained bytes: 500"));
    }

    // ── Mocked Zot-state fetch errors ─────────────────────────────────────

    #[tokio::test]
    async fn zot_fetch_error_fails_preflight() {
        let zot = FailingZot;
        let selected: Vec<SelectedImage> = vec![];
        let cfg = RetentionPreflightConfig {
            enabled: true,
            destructive_enabled: true,
            newest_tags: 1,
        };
        let err = run_preflight(&zot, &selected, &cfg).await.unwrap_err();
        assert!(
            matches!(err, PreflightError::ZotState(ZotStateError::Fetch(_))),
            "Zot fetch failure must propagate as an error (fail-closed)"
        );
    }

    #[tokio::test]
    async fn zot_fetch_error_fails_even_in_dry_run() {
        // Even in dry-run, a Zot fetch error should surface — the operator
        // needs to know the report is based on stale/missing data.
        let zot = FailingZot;
        let selected: Vec<SelectedImage> = vec![];
        let cfg = RetentionPreflightConfig {
            enabled: true,
            destructive_enabled: false,
            newest_tags: 1,
        };
        let result = run_preflight(&zot, &selected, &cfg).await;
        assert!(
            result.is_err(),
            "fetch error must propagate even in dry-run"
        );
    }

    // ── adapt_selected_images ─────────────────────────────────────────────

    #[test]
    fn adapt_selected_images_maps_db_rows() {
        let db_rows = vec![djinn_db::SelectedCatalogImage {
            image_id: "i1".into(),
            name: "Rust".into(),
            tag: Some("reg/djinn-image-i1:hash".into()),
            registry_digest: Some("sha256:abc".into()),
            status: "ready".into(),
            last_error: None,
            selected_project_ids: vec!["p1".into(), "p2".into()],
        }];
        let adapted = adapt_selected_images(&db_rows);
        assert_eq!(adapted.len(), 1);
        assert_eq!(adapted[0].image_id, "i1");
        assert_eq!(adapted[0].repo, "djinn-image-i1");
        assert_eq!(adapted[0].tag.as_deref(), Some("hash"));
        assert_eq!(adapted[0].digest.as_deref(), Some("sha256:abc"));
    }

    #[test]
    fn catalog_repo_name_matches_build_subject() {
        assert_eq!(catalog_repo_name("abc123"), "djinn-image-abc123");
    }

    // ── multiple repos and images ────────────────────────────────────────

    #[tokio::test]
    async fn multiple_repos_some_safe_some_unsafe_blocks() {
        let zot = MockZot {
            repos: vec![
                zrepo(
                    "djinn-image-safe",
                    vec![
                        ztag("keep", "sha256:s1", 100, "2024-01-02"),
                        ztag("drop", "sha256:s2", 50, "2024-01-01"),
                    ],
                ),
                zrepo(
                    "djinn-image-risky",
                    vec![
                        ztag("keep", "sha256:r1", 100, "2024-01-02"),
                        ztag("drop", "sha256:r2", 50, "2024-01-01"),
                    ],
                ),
            ],
        };
        let selected = vec![
            sel("safe", Some("keep"), Some("sha256:s1"), "ready"),
            sel("risky", Some("drop"), Some("sha256:r2"), "ready"),
        ];
        let cfg = RetentionPreflightConfig {
            enabled: true,
            destructive_enabled: true,
            newest_tags: 1,
        };
        let err = run_preflight(&zot, &selected, &cfg).await.unwrap_err();
        assert!(
            matches!(err, PreflightError::UnsafeSelectedImages { count, .. } if count == 1),
            "one unsafe image out of two must still block"
        );
    }

    #[tokio::test]
    async fn digest_alias_makes_deleted_tag_safe() {
        // Selected image uses tag "v1" which will be deleted, but the retained
        // tag "v2" points to the same digest (alias).
        let zot = MockZot {
            repos: vec![zrepo(
                "djinn-image-i1",
                vec![
                    ztag("v1", "sha256:shared", 100, "2024-01-01"),
                    ztag("v2", "sha256:shared", 200, "2024-01-02"),
                ],
            )],
        };
        let selected = vec![sel("i1", Some("v1"), Some("sha256:shared"), "ready")];
        let cfg = RetentionPreflightConfig {
            enabled: true,
            destructive_enabled: true,
            newest_tags: 1,
        };
        let outcome = run_preflight(&zot, &selected, &cfg).await.unwrap();
        assert!(outcome.plan.is_safe, "alias via shared digest must be safe");
        assert!(!outcome.blocks_rollout);
    }

    // ── Bounded mode/outcome contract ────────────────────────────────────

    #[tokio::test]
    async fn disabled_mode_produces_disabled_outcome_without_zot_contact() {
        let zot = FailingZot; // Should never be called.
        let cfg = RetentionPreflightConfig {
            enabled: false,
            destructive_enabled: false,
            newest_tags: 5,
        };
        let outcome = run_preflight(&zot, &[], &cfg).await.unwrap();
        assert_eq!(outcome.mode, PreflightMode::Disabled);
        assert_eq!(outcome.outcome_kind, PreflightOutcomeKind::Disabled);
        assert!(!outcome.blocks_rollout);
        assert!(outcome.report.contains("Mode: disabled"));
        assert!(outcome.report.contains("Outcome: disabled"));
    }

    #[tokio::test]
    async fn dry_run_mode_produces_advisory_outcome() {
        let zot = MockZot {
            repos: vec![two_tag_fixture()],
        };
        let selected = vec![sel("i1", Some("keep"), Some("sha256:keep"), "ready")];
        let cfg = RetentionPreflightConfig {
            enabled: true,
            destructive_enabled: false,
            newest_tags: 1,
        };
        let outcome = run_preflight(&zot, &selected, &cfg).await.unwrap();
        assert_eq!(outcome.mode, PreflightMode::DryRun);
        assert_eq!(outcome.outcome_kind, PreflightOutcomeKind::Advisory);
        assert!(!outcome.blocks_rollout);
        assert!(outcome.report.contains("Mode: dry_run"));
        assert!(outcome.report.contains("Outcome: advisory"));
    }

    #[tokio::test]
    async fn destructive_safe_produces_destructive_safe_outcome() {
        let zot = MockZot {
            repos: vec![two_tag_fixture()],
        };
        let selected = vec![sel("i1", Some("keep"), Some("sha256:keep"), "ready")];
        let cfg = RetentionPreflightConfig {
            enabled: true,
            destructive_enabled: true,
            newest_tags: 1,
        };
        let outcome = run_preflight(&zot, &selected, &cfg).await.unwrap();
        assert_eq!(outcome.mode, PreflightMode::Destructive);
        assert_eq!(outcome.outcome_kind, PreflightOutcomeKind::DestructiveSafe);
        assert!(!outcome.blocks_rollout);
        assert!(outcome.report.contains("Mode: destructive"));
        assert!(outcome.report.contains("Outcome: destructive_safe"));
    }

    #[tokio::test]
    async fn destructive_blocked_produces_error_with_report() {
        let zot = MockZot {
            repos: vec![two_tag_fixture()],
        };
        let selected = vec![sel("i1", Some("drop"), Some("sha256:drop"), "ready")];
        let cfg = RetentionPreflightConfig {
            enabled: true,
            destructive_enabled: true,
            newest_tags: 1,
        };
        let err = run_preflight(&zot, &selected, &cfg).await.unwrap_err();
        match err {
            PreflightError::UnsafeSelectedImages { count, report, .. } => {
                assert_eq!(count, 1);
                assert!(
                    report.contains("Mode: destructive"),
                    "blocked report must contain mode: {report}"
                );
                assert!(
                    report.contains("Outcome: destructive_blocked"),
                    "blocked report must contain outcome: {report}"
                );
                assert!(report.contains("UNSAFE"));
            }
            other => panic!("expected UnsafeSelectedImages, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_error_produces_fail_closed_in_both_modes() {
        let zot = FailingZot;
        // Destructive mode: must fail.
        let cfg_destructive = RetentionPreflightConfig {
            enabled: true,
            destructive_enabled: true,
            newest_tags: 1,
        };
        assert!(run_preflight(&zot, &[], &cfg_destructive).await.is_err());

        // Dry-run mode: must also fail (fail-closed on fetch error).
        let cfg_dry_run = RetentionPreflightConfig {
            enabled: true,
            destructive_enabled: false,
            newest_tags: 1,
        };
        assert!(run_preflight(&zot, &[], &cfg_dry_run).await.is_err());
    }

    #[tokio::test]
    async fn report_contains_candidate_tag_counts_and_bytes() {
        let zot = MockZot {
            repos: vec![two_tag_fixture()],
        };
        let selected = vec![sel("i1", Some("keep"), Some("sha256:keep"), "ready")];
        let cfg = RetentionPreflightConfig {
            enabled: true,
            destructive_enabled: false,
            newest_tags: 1,
        };
        let outcome = run_preflight(&zot, &selected, &cfg).await.unwrap();
        // Aggregate counts.
        assert!(outcome.report.contains("Candidate tags: 2"));
        assert!(outcome.report.contains("Retained tags: 1"));
        assert!(outcome.report.contains("Deleted tags: 1"));
        // Projected bytes.
        assert!(outcome.report.contains("Projected reclaimed bytes: 300"));
        assert!(outcome.report.contains("Projected retained bytes: 500"));
        // Selected-image safety.
        assert!(outcome.report.contains("Selected images safe: 1"));
        assert!(outcome.report.contains("Selected images unsafe: 0"));
    }

    #[tokio::test]
    async fn report_contains_gc_observation_guidance() {
        let zot = MockZot {
            repos: vec![two_tag_fixture()],
        };
        let selected = vec![sel("i1", Some("keep"), Some("sha256:keep"), "ready")];
        let cfg = RetentionPreflightConfig {
            enabled: true,
            destructive_enabled: false,
            newest_tags: 1,
        };
        let outcome = run_preflight(&zot, &selected, &cfg).await.unwrap();
        assert!(
            outcome
                .report
                .contains("Post-enable Zot GC observation guidance"),
            "report must contain GC observation guidance"
        );
        assert!(
            outcome.report.contains("operator-owned"),
            "report must state production execution is operator-owned"
        );
    }

    #[test]
    fn preflight_config_mode_derivation() {
        assert_eq!(
            RetentionPreflightConfig {
                enabled: false,
                destructive_enabled: false,
                newest_tags: 5
            }
            .mode(),
            PreflightMode::Disabled
        );
        assert_eq!(
            RetentionPreflightConfig {
                enabled: false,
                destructive_enabled: true,
                newest_tags: 5
            }
            .mode(),
            PreflightMode::Disabled,
            "disabled overrides destructive_enabled"
        );
        assert_eq!(
            RetentionPreflightConfig {
                enabled: true,
                destructive_enabled: false,
                newest_tags: 5
            }
            .mode(),
            PreflightMode::DryRun
        );
        assert_eq!(
            RetentionPreflightConfig {
                enabled: true,
                destructive_enabled: true,
                newest_tags: 5
            }
            .mode(),
            PreflightMode::Destructive
        );
    }

    fn two_tag_fixture() -> ZotRepository {
        zrepo(
            "djinn-image-i1",
            vec![
                ztag("keep", "sha256:keep", 500, "2024-01-02"),
                ztag("drop", "sha256:drop", 300, "2024-01-01"),
            ],
        )
    }
}
