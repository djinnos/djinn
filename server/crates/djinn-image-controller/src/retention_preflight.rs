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

use crate::retention::{self, CATALOG_REPO_PREFIX, RetentionPlan, SelectedImage, ZotRepository};

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
}

/// Preflight configuration for retention gating.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetentionPreflightConfig {
    /// Whether destructive deletion is enabled in the Helm/operator config.
    /// When `false`, preflight is advisory (dry-run report only, never blocks).
    /// When `true`, preflight runs the full fail-closed safety check.
    pub destructive_enabled: bool,
    /// Number of newest tags to retain per catalog repo.
    pub newest_tags: usize,
}

impl Default for RetentionPreflightConfig {
    fn default() -> Self {
        Self {
            destructive_enabled: false,
            newest_tags: 5,
        }
    }
}

/// Outcome of a retention preflight check.
#[derive(Debug)]
pub struct PreflightOutcome {
    /// The computed retention plan (always available, even in dry-run).
    pub plan: RetentionPlan,
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
    },
}

type PreflightResult = std::result::Result<PreflightOutcome, PreflightError>;

/// Run the retention preflight.
///
/// This is the main entry point. It fetches Zot state, enumerates selected
/// catalog images from the DB, runs the planner, and enforces fail-closed
/// behavior.
///
/// - When `cfg.destructive_enabled` is `false`, the outcome is advisory: a
///   dry-run report is produced but nothing blocks.
/// - When `cfg.destructive_enabled` is `true` and any selected image is unsafe,
///   returns [`PreflightError::UnsafeSelectedImages`].
pub async fn run_preflight(
    zot: &dyn ZotStateSource,
    selected: &[SelectedImage],
    cfg: &RetentionPreflightConfig,
) -> PreflightResult {
    // Fetch Zot state — errors propagate (fail-closed on fetch failure).
    let repos = zot.fetch_repositories().await?;

    let policy = retention::RetentionPolicy {
        newest_tags: cfg.newest_tags,
    };
    let plan = retention::plan_retention(&repos, selected, &policy);
    let report = retention::render_report(&plan);

    if cfg.destructive_enabled && !plan.is_safe {
        return Err(PreflightError::UnsafeSelectedImages {
            count: plan.unsafe_images.len(),
            plan,
        });
    }

    let blocks_rollout = cfg.destructive_enabled && !plan.is_safe;

    Ok(PreflightOutcome {
        plan,
        blocks_rollout,
        report,
    })
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
    destructive_enabled: bool,
    newest_tags: usize,
) -> RetentionPreflightConfig {
    RetentionPreflightConfig {
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
            destructive_enabled: true,
            newest_tags: 1,
        };
        let outcome = run_preflight(&zot, &selected, &cfg).await.unwrap();
        assert!(outcome.plan.is_safe, "alias via shared digest must be safe");
        assert!(!outcome.blocks_rollout);
    }
}
