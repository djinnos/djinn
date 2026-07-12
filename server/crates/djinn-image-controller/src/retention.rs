//! Pure Zot catalog retention planner.
//!
//! Computes the dry-run/preflight retention plan from supplied Zot repository
//! state and selected catalog-image records — no live registry or Kubernetes
//! access required. The preflight layer ([`crate::retention_preflight`]) wires
//! this to DB state and a mockable Zot-state trait; this module owns only the
//! pure arithmetic.
//!
//! ## Retention model
//!
//! * Only `djinn-image-*` catalog repositories are eligible for retention.
//!   BuildKit cache repos (`djinn-buildkitd-*`) and any other repo are never
//!   touched.
//! * Within each eligible repo, the **newest-N** tags by push timestamp are
//!   retained; the rest are deletion candidates.
//! * Every selected catalog image must remain pullable after retention. A
//!   selected image is safe if:
//!   - its tag is in the retained set, **or**
//!   - it has a digest pin (`repo@sha256:…`) whose manifest still exists in the
//!     retained set, **or**
//!   - it has an alias (a retained tag pointing to the same digest).
//! * If any selected image is unsafe, the plan is marked `unsafe` and the
//!   offending images are listed — the preflight must **fail closed** and not
//!   proceed with destructive retention.

use std::collections::BTreeMap;

/// Prefix for Djinn-managed catalog image repositories.
pub const CATALOG_REPO_PREFIX: &str = "djinn-image-";

/// A tag in a Zot repository, as the state client would report it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZotTag {
    /// Tag name (e.g. `5f8b93e437b6`).
    pub tag: String,
    /// Manifest digest this tag points to (`sha256:…`).
    pub digest: String,
    /// Size of the manifest + all layers it references, in bytes.
    pub size_bytes: u64,
    /// Push timestamp (ISO 8601 or any lexicographically sortable format).
    /// Used for newest-N ordering; higher = newer.
    pub pushed_at: String,
}

/// A Zot repository as the state client would report it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZotRepository {
    /// Repository name (e.g. `djinn-image-abc123`).
    pub name: String,
    /// All tags in the repository.
    pub tags: Vec<ZotTag>,
}

impl ZotRepository {
    /// True if this is a Djinn-managed catalog image repository
    /// (`djinn-image-*`).
    pub fn is_catalog_repo(&self) -> bool {
        self.name.starts_with(CATALOG_REPO_PREFIX)
    }

    /// Total bytes of all tags (sum of tag sizes — shared blobs may make this
    /// an over-estimate, but it's the best we can do without a dedup-aware
    /// size model).
    pub fn total_size_bytes(&self) -> u64 {
        self.tags.iter().map(|t| t.size_bytes).sum()
    }
}

/// A selected catalog image record, adapted from
/// [`djinn_db::SelectedCatalogImage`] into the pure shape the planner needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectedImage {
    /// Catalog image id (the suffix after `djinn-image-`).
    pub image_id: String,
    /// Repository name: `djinn-image-<image_id>`.
    pub repo: String,
    /// Tag the image dispatches on (`None` if not yet built).
    pub tag: Option<String>,
    /// Immutable manifest digest (`sha256:…`), `None` if not captured.
    pub digest: Option<String>,
    /// Build status string (`ready`, `building`, `none`, `failed`).
    pub status: String,
}

/// A tag that survives retention.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedTag {
    pub repo: String,
    pub tag: String,
    pub digest: String,
    pub size_bytes: u64,
}

/// A tag slated for deletion by the retention policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeletedTag {
    pub repo: String,
    pub tag: String,
    pub digest: String,
    pub size_bytes: u64,
}

/// A selected image that has been proven safe (pullable post-retention).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SafePin {
    pub image_id: String,
    pub repo: String,
    /// How the image is proven pullable.
    pub reason: SafeReason,
}

/// How a selected image remains pullable after retention.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SafeReason {
    /// The selected tag itself is in the retained set.
    TagRetained { tag: String },
    /// The image's digest is still reachable via a retained tag.
    DigestRetained { tag: String, digest: String },
    /// The image's digest is pinned via `repo@sha256:…` and a retained tag
    /// shares that digest (alias).
    AliasRetained { alias_tag: String, digest: String },
}

/// A selected image that cannot be proven pullable after retention — blocks
/// destructive rollout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsafeImage {
    pub image_id: String,
    pub repo: String,
    pub tag: Option<String>,
    pub digest: Option<String>,
    /// Why the image is unsafe (human-readable).
    pub reason: String,
}

/// A complete retention plan for one Zot repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoRetentionPlan {
    pub repo: String,
    pub retained: Vec<RetainedTag>,
    pub deleted: Vec<DeletedTag>,
}

/// The full retention plan across all catalog repositories, with selected-image
/// safety analysis and byte accounting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetentionPlan {
    /// Per-repo retention decisions, ordered by repo name.
    pub repos: Vec<RepoRetentionPlan>,
    /// Selected images proven safe, ordered by image_id.
    pub safe_pins: Vec<SafePin>,
    /// Selected images that are unsafe (block destructive retention), ordered by image_id.
    pub unsafe_images: Vec<UnsafeImage>,
    /// Bytes that would be reclaimed if all deleted tags were GC'd.
    pub projected_reclaimed_bytes: u64,
    /// Bytes that remain after GC (sum of retained tag sizes).
    pub projected_retained_bytes: u64,
    /// True if no selected image is unsafe.
    pub is_safe: bool,
}

/// Retention policy configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Number of newest tags to retain per catalog repo.
    pub newest_tags: usize,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self { newest_tags: 5 }
    }
}

/// Compute the retention plan from Zot repository state, selected images, and
/// a retention policy.
///
/// Pure: no I/O, no panics, deterministic output ordering.
pub fn plan_retention(
    repos: &[ZotRepository],
    selected: &[SelectedImage],
    policy: &RetentionPolicy,
) -> RetentionPlan {
    // Only catalog repos are eligible.
    let mut catalog_repos: Vec<&ZotRepository> =
        repos.iter().filter(|r| r.is_catalog_repo()).collect();
    catalog_repos.sort_by(|a, b| a.name.cmp(&b.name));

    let mut repo_plans = Vec::new();
    let mut total_reclaimed: u64 = 0;
    let mut total_retained: u64 = 0;

    for repo in &catalog_repos {
        let (plan, reclaimed, retained) = plan_repo(repo, policy);
        total_reclaimed += reclaimed;
        total_retained += retained;
        repo_plans.push(plan);
    }

    // Build a lookup: repo name → set of retained digests and tags.
    let mut retained_by_repo: BTreeMap<&str, (Vec<&str>, Vec<&str>)> = BTreeMap::new();
    for rp in &repo_plans {
        let digests: Vec<&str> = rp.retained.iter().map(|t| t.digest.as_str()).collect();
        let tags: Vec<&str> = rp.retained.iter().map(|t| t.tag.as_str()).collect();
        retained_by_repo.insert(&rp.repo, (digests, tags));
    }

    // Analyze selected images.
    let mut safe_pins = Vec::new();
    let mut unsafe_images = Vec::new();

    for sel in selected {
        let safe = check_selected_image(sel, retained_by_repo.get(sel.repo.as_str()));
        match safe {
            Some(reason) => safe_pins.push(SafePin {
                image_id: sel.image_id.clone(),
                repo: sel.repo.clone(),
                reason,
            }),
            None => {
                unsafe_images.push(UnsafeImage {
                    image_id: sel.image_id.clone(),
                    repo: sel.repo.clone(),
                    tag: sel.tag.clone(),
                    digest: sel.digest.clone(),
                    reason: unsafe_reason(sel),
                });
            }
        }
    }

    safe_pins.sort_by(|a, b| a.image_id.cmp(&b.image_id));
    unsafe_images.sort_by(|a, b| a.image_id.cmp(&b.image_id));

    let is_safe = unsafe_images.is_empty();

    RetentionPlan {
        repos: repo_plans,
        safe_pins,
        unsafe_images,
        projected_reclaimed_bytes: total_reclaimed,
        projected_retained_bytes: total_retained,
        is_safe,
    }
}

/// Compute the retention plan for a single catalog repository.
fn plan_repo(repo: &ZotRepository, policy: &RetentionPolicy) -> (RepoRetentionPlan, u64, u64) {
    // Sort tags newest-first by pushed_at (lexicographic descending).
    let mut tags: Vec<&ZotTag> = repo.tags.iter().collect();
    tags.sort_by(|a, b| b.pushed_at.cmp(&a.pushed_at));

    let n = policy.newest_tags.min(tags.len());
    let (retained_slice, deleted_slice) = tags.split_at(n);

    let mut retained = Vec::new();
    let mut deleted = Vec::new();
    let mut retained_bytes: u64 = 0;
    let mut reclaimed_bytes: u64 = 0;

    for t in retained_slice {
        retained.push(RetainedTag {
            repo: repo.name.clone(),
            tag: t.tag.clone(),
            digest: t.digest.clone(),
            size_bytes: t.size_bytes,
        });
        retained_bytes += t.size_bytes;
    }
    for t in deleted_slice {
        deleted.push(DeletedTag {
            repo: repo.name.clone(),
            tag: t.tag.clone(),
            digest: t.digest.clone(),
            size_bytes: t.size_bytes,
        });
        reclaimed_bytes += t.size_bytes;
    }

    // Deterministic ordering within each list: by tag name.
    retained.sort_by(|a, b| a.tag.cmp(&b.tag));
    deleted.sort_by(|a, b| a.tag.cmp(&b.tag));

    (
        RepoRetentionPlan {
            repo: repo.name.clone(),
            retained,
            deleted,
        },
        reclaimed_bytes,
        retained_bytes,
    )
}

/// Check if a selected image remains pullable after retention.
/// Returns `Some(reason)` if safe, `None` if unsafe.
fn check_selected_image(
    sel: &SelectedImage,
    retained: Option<&(Vec<&str>, Vec<&str>)>,
) -> Option<SafeReason> {
    // Not-ready images: if they have no tag/digest at all, they're not pullable
    // NOW, so retention can't make them *less* pullable. However, fail-closed
    // means we must still flag them. An image that is `none`/`building`/`failed`
    // with no tag is not pullable — it's unsafe for destructive retention
    // because deleting tags could prevent a future build's digest from matching.
    // But we only block if the image *has* a tag or digest that could be deleted.

    let Some((retained_digests, retained_tags)) = retained else {
        // No retained tags for this repo at all.
        return None;
    };

    // Case 1: the image's own tag is retained.
    if let Some(tag) = &sel.tag
        && retained_tags.contains(&tag.as_str())
    {
        return Some(SafeReason::TagRetained { tag: tag.clone() });
    }

    // Case 2: the image's digest is reachable via any retained tag.
    if let Some(digest) = &sel.digest
        && retained_digests.contains(&digest.as_str())
    {
        // Find the retained tag(s) with this digest for the reason.
        let alias_tag = retained_tags
            .iter()
            .copied()
            .find(|t| *t == sel.tag.as_deref().unwrap_or(""))
            .unwrap_or(retained_tags.first().copied().unwrap_or(""))
            .to_string();
        if sel.tag.as_deref() == Some(&alias_tag) {
            return Some(SafeReason::DigestRetained {
                tag: alias_tag,
                digest: digest.clone(),
            });
        }
        // An alias: a *different* retained tag shares this digest.
        return Some(SafeReason::AliasRetained {
            alias_tag,
            digest: digest.clone(),
        });
    }

    // Not provably pullable.
    None
}

/// Human-readable reason for why a selected image is unsafe.
fn unsafe_reason(sel: &SelectedImage) -> String {
    match (&sel.tag, &sel.digest) {
        (Some(tag), Some(digest)) => {
            format!("tag '{tag}' and digest '{digest}' are both slated for deletion")
        }
        (Some(tag), None) => {
            format!("tag '{tag}' is slated for deletion and no digest pin exists")
        }
        (None, Some(digest)) => {
            format!("digest '{digest}' is not reachable via any retained tag")
        }
        (None, None) => {
            "image is not built yet (no tag or digest) — cannot prove pullability".to_string()
        }
    }
}

/// Render a retention plan as a human-readable dry-run report.
///
/// Output is deterministic: repos sorted by name, tags sorted alphabetically,
/// selected images sorted by image_id.
pub fn render_report(plan: &RetentionPlan) -> String {
    let mut out = String::new();
    out.push_str("=== Zot Catalog Retention Dry-Run Report ===\n\n");

    // Summary.
    out.push_str(&format!(
        "Safe: {}\n",
        if plan.is_safe { "yes" } else { "NO" }
    ));
    out.push_str(&format!(
        "Projected reclaimed bytes: {}\n",
        plan.projected_reclaimed_bytes
    ));
    out.push_str(&format!(
        "Projected retained bytes: {}\n",
        plan.projected_retained_bytes
    ));
    out.push('\n');

    // Per-repo details.
    for rp in &plan.repos {
        out.push_str(&format!("Repository: {}\n", rp.repo));
        out.push_str(&format!("  Retained tags ({}):\n", rp.retained.len()));
        for t in &rp.retained {
            out.push_str(&format!(
                "    {}  digest={}  size={}B\n",
                t.tag, t.digest, t.size_bytes
            ));
        }
        out.push_str(&format!("  Deleted tags ({}):\n", rp.deleted.len()));
        for t in &rp.deleted {
            out.push_str(&format!(
                "    {}  digest={}  size={}B\n",
                t.tag, t.digest, t.size_bytes
            ));
        }
        out.push('\n');
    }

    // Selected-image pins/aliases.
    out.push_str(&format!(
        "Selected-image pins ({}):\n",
        plan.safe_pins.len()
    ));
    for pin in &plan.safe_pins {
        let reason_str = match &pin.reason {
            SafeReason::TagRetained { tag } => format!("tag '{tag}' retained"),
            SafeReason::DigestRetained { tag, digest } => {
                format!("digest '{digest}' reachable via retained tag '{tag}'")
            }
            SafeReason::AliasRetained { alias_tag, digest } => {
                format!("digest '{digest}' aliased by retained tag '{alias_tag}'")
            }
        };
        out.push_str(&format!(
            "  image {}  repo {}  SAFE: {}\n",
            pin.image_id, pin.repo, reason_str
        ));
    }
    out.push('\n');

    // Unsafe images (if any).
    if !plan.unsafe_images.is_empty() {
        out.push_str(&format!(
            "UNSAFE selected images ({}):\n",
            plan.unsafe_images.len()
        ));
        for img in &plan.unsafe_images {
            out.push_str(&format!(
                "  image {}  repo {}  UNSAFE: {}\n",
                img.image_id, img.repo, img.reason
            ));
        }
    }

    out.push_str("\n--- Post-enable Zot GC observation guidance ---\n");
    out.push_str(
        "After enabling destructive retention, confirm reclamation by observing \
         Zot GC: verify djinn-image-* repo tag counts drop to the retained set, \
         confirm projected_reclaimed_bytes matches actual storage reduction, \
         and check Zot logs for garbage-collection completion. Production GC \
         execution is operator-owned; see the runbook for the full checklist.\n",
    );

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(name: &str, digest: &str, size: u64, pushed: &str) -> ZotTag {
        ZotTag {
            tag: name.into(),
            digest: digest.into(),
            size_bytes: size,
            pushed_at: pushed.into(),
        }
    }

    fn repo(name: &str, tags: Vec<ZotTag>) -> ZotRepository {
        ZotRepository {
            name: name.into(),
            tags,
        }
    }

    fn selected(
        image_id: &str,
        tag: Option<&str>,
        digest: Option<&str>,
        status: &str,
    ) -> SelectedImage {
        SelectedImage {
            image_id: image_id.into(),
            repo: format!("{CATALOG_REPO_PREFIX}{image_id}"),
            tag: tag.map(String::from),
            digest: digest.map(String::from),
            status: status.into(),
        }
    }

    // ── catalog vs cache filtering ────────────────────────────────────────

    #[test]
    fn non_catalog_repos_are_ignored() {
        let repos = vec![
            repo(
                "djinn-buildkitd-foo",
                vec![tag("a", "sha256:1", 100, "2024-01-01")],
            ),
            repo("random-repo", vec![tag("b", "sha256:2", 200, "2024-01-01")]),
        ];
        let plan = plan_retention(&repos, &[], &RetentionPolicy { newest_tags: 1 });
        assert!(plan.repos.is_empty());
        assert_eq!(plan.projected_reclaimed_bytes, 0);
    }

    #[test]
    fn catalog_repos_are_processed() {
        let repos = vec![repo(
            "djinn-image-abc",
            vec![
                tag("t1", "sha256:1", 100, "2024-01-01"),
                tag("t2", "sha256:2", 200, "2024-01-02"),
            ],
        )];
        let plan = plan_retention(&repos, &[], &RetentionPolicy { newest_tags: 1 });
        assert_eq!(plan.repos.len(), 1);
        // Newest 1 tag retained (t2), older deleted (t1).
        assert_eq!(plan.repos[0].retained.len(), 1);
        assert_eq!(plan.repos[0].retained[0].tag, "t2");
        assert_eq!(plan.repos[0].deleted.len(), 1);
        assert_eq!(plan.repos[0].deleted[0].tag, "t1");
    }

    // ── newest-N selection ────────────────────────────────────────────────

    #[test]
    fn newest_n_retention_keeps_most_recent() {
        let repos = vec![repo(
            "djinn-image-x",
            vec![
                tag("old", "sha256:1", 100, "2024-01-01"),
                tag("mid", "sha256:2", 200, "2024-01-02"),
                tag("new", "sha256:3", 300, "2024-01-03"),
            ],
        )];
        let plan = plan_retention(&repos, &[], &RetentionPolicy { newest_tags: 2 });
        // Retain the 2 newest: mid + new.
        let retained_tags: Vec<&str> = plan.repos[0]
            .retained
            .iter()
            .map(|t| t.tag.as_str())
            .collect();
        assert!(retained_tags.contains(&"mid"));
        assert!(retained_tags.contains(&"new"));
        assert_eq!(plan.repos[0].deleted.len(), 1);
        assert_eq!(plan.repos[0].deleted[0].tag, "old");
    }

    #[test]
    fn newest_n_greater_than_tag_count_retains_all() {
        let repos = vec![repo(
            "djinn-image-x",
            vec![tag("only", "sha256:1", 100, "2024-01-01")],
        )];
        let plan = plan_retention(&repos, &[], &RetentionPolicy { newest_tags: 5 });
        assert_eq!(plan.repos[0].retained.len(), 1);
        assert!(plan.repos[0].deleted.is_empty());
    }

    // ── byte accounting ───────────────────────────────────────────────────

    #[test]
    fn byte_accounting_sums_correctly() {
        let repos = vec![repo(
            "djinn-image-x",
            vec![
                tag("a", "sha256:1", 100, "2024-01-01"),
                tag("b", "sha256:2", 200, "2024-01-02"),
                tag("c", "sha256:3", 300, "2024-01-03"),
            ],
        )];
        let plan = plan_retention(&repos, &[], &RetentionPolicy { newest_tags: 1 });
        // Retain newest (c=300), reclaim a+b = 100+200 = 300.
        assert_eq!(plan.projected_retained_bytes, 300);
        assert_eq!(plan.projected_reclaimed_bytes, 300);
    }

    // ── selected-image safety ─────────────────────────────────────────────

    #[test]
    fn selected_tag_retained_is_safe() {
        let repos = vec![repo(
            "djinn-image-i1",
            vec![
                tag("hash1", "sha256:aaa", 100, "2024-01-01"),
                tag("hash2", "sha256:bbb", 200, "2024-01-02"),
            ],
        )];
        let sel = vec![selected("i1", Some("hash2"), Some("sha256:bbb"), "ready")];
        let plan = plan_retention(&repos, &sel, &RetentionPolicy { newest_tags: 1 });
        assert!(plan.is_safe);
        assert_eq!(plan.safe_pins.len(), 1);
        assert!(plan.unsafe_images.is_empty());
        assert!(matches!(
            &plan.safe_pins[0].reason,
            SafeReason::TagRetained { tag } if tag == "hash2"
        ));
    }

    #[test]
    fn selected_tag_deleted_but_digest_retained_is_safe() {
        // Image selected on tag "hash1" but that tag will be deleted.
        // However, the same digest is also under retained tag "hash2" (alias).
        let repos = vec![repo(
            "djinn-image-i1",
            vec![
                tag("hash1", "sha256:same", 100, "2024-01-01"),
                tag("hash2", "sha256:same", 200, "2024-01-02"),
            ],
        )];
        let sel = vec![selected("i1", Some("hash1"), Some("sha256:same"), "ready")];
        let plan = plan_retention(&repos, &sel, &RetentionPolicy { newest_tags: 1 });
        // Newest 1 = hash2. hash1 is deleted, but its digest survives via hash2.
        assert!(plan.is_safe);
        assert_eq!(plan.safe_pins.len(), 1);
    }

    #[test]
    fn selected_image_blocks_when_tag_and_digest_lost() {
        // Selected image's tag AND digest will both be deleted — no alias.
        let repos = vec![repo(
            "djinn-image-i1",
            vec![
                tag("old-hash", "sha256:unique-old", 100, "2024-01-01"),
                tag("new-hash", "sha256:unique-new", 200, "2024-01-02"),
                tag("newer-hash", "sha256:unique-newer", 300, "2024-01-03"),
            ],
        )];
        let sel = vec![selected(
            "i1",
            Some("old-hash"),
            Some("sha256:unique-old"),
            "ready",
        )];
        // newest_tags=2 retains new-hash + newer-hash; old-hash is deleted.
        let plan = plan_retention(&repos, &sel, &RetentionPolicy { newest_tags: 2 });
        assert!(!plan.is_safe);
        assert_eq!(plan.unsafe_images.len(), 1);
        assert_eq!(plan.unsafe_images[0].image_id, "i1");
    }

    #[test]
    fn selected_image_no_repo_in_zot_is_unsafe() {
        // Selected image exists in DB but its repo is missing from Zot state.
        let repos: Vec<ZotRepository> = vec![];
        let sel = vec![selected("i1", Some("hash"), Some("sha256:abc"), "ready")];
        let plan = plan_retention(&repos, &sel, &RetentionPolicy { newest_tags: 5 });
        assert!(!plan.is_safe);
        assert_eq!(plan.unsafe_images.len(), 1);
    }

    #[test]
    fn not_ready_selected_image_with_tag_is_checked() {
        // A building image with a tag: if that tag gets deleted, it's unsafe.
        let repos = vec![repo(
            "djinn-image-i1",
            vec![
                tag("building-tag", "sha256:build", 100, "2024-01-01"),
                tag("new-tag", "sha256:new", 200, "2024-01-02"),
            ],
        )];
        let sel = vec![selected(
            "i1",
            Some("building-tag"),
            Some("sha256:build"),
            "building",
        )];
        let plan = plan_retention(&repos, &sel, &RetentionPolicy { newest_tags: 1 });
        assert!(
            !plan.is_safe,
            "building image whose tag gets deleted must block"
        );
    }

    // ── deterministic ordering ────────────────────────────────────────────

    #[test]
    fn plan_deterministic_output_ordering() {
        let repos = vec![
            repo(
                "djinn-image-zzz",
                vec![tag("t1", "sha256:1", 100, "2024-01-01")],
            ),
            repo(
                "djinn-image-aaa",
                vec![tag("t2", "sha256:2", 200, "2024-01-01")],
            ),
        ];
        let plan = plan_retention(&repos, &[], &RetentionPolicy { newest_tags: 5 });
        assert_eq!(plan.repos[0].repo, "djinn-image-aaa");
        assert_eq!(plan.repos[1].repo, "djinn-image-zzz");
    }

    #[test]
    fn retained_tags_sorted_alphabetically() {
        let repos = vec![repo(
            "djinn-image-x",
            vec![
                tag("zebra", "sha256:1", 100, "2024-01-03"),
                tag("alpha", "sha256:2", 200, "2024-01-02"),
                tag("mango", "sha256:3", 300, "2024-01-01"),
            ],
        )];
        let plan = plan_retention(&repos, &[], &RetentionPolicy { newest_tags: 2 });
        let tags: Vec<&str> = plan.repos[0]
            .retained
            .iter()
            .map(|t| t.tag.as_str())
            .collect();
        assert_eq!(tags, vec!["alpha", "zebra"]);
    }

    // ── report rendering ──────────────────────────────────────────────────

    #[test]
    fn render_report_contains_all_sections() {
        let repos = vec![repo(
            "djinn-image-i1",
            vec![
                tag("keep", "sha256:keep", 500, "2024-01-02"),
                tag("drop", "sha256:drop", 300, "2024-01-01"),
            ],
        )];
        let sel = vec![selected("i1", Some("keep"), Some("sha256:keep"), "ready")];
        let plan = plan_retention(&repos, &sel, &RetentionPolicy { newest_tags: 1 });
        let report = render_report(&plan);

        assert!(report.contains("Zot Catalog Retention Dry-Run Report"));
        assert!(report.contains("Safe: yes"));
        assert!(report.contains("Projected reclaimed bytes: 300"));
        assert!(report.contains("Projected retained bytes: 500"));
        assert!(report.contains("Repository: djinn-image-i1"));
        assert!(report.contains("Retained tags (1)"));
        assert!(report.contains("Deleted tags (1)"));
        assert!(report.contains("Selected-image pins (1)"));
        assert!(report.contains("image i1"));
    }

    #[test]
    fn render_report_shows_unsafe_images() {
        let repos = vec![repo(
            "djinn-image-i1",
            vec![tag("new", "sha256:new", 200, "2024-01-02")],
        )];
        let sel = vec![selected("i1", Some("old"), Some("sha256:old"), "ready")];
        let plan = plan_retention(&repos, &sel, &RetentionPolicy { newest_tags: 1 });
        let report = render_report(&plan);

        assert!(report.contains("Safe: NO"));
        assert!(report.contains("UNSAFE selected images (1)"));
        assert!(report.contains("image i1"));
    }

    #[test]
    fn empty_state_produces_safe_empty_plan() {
        let plan = plan_retention(&[], &[], &RetentionPolicy::default());
        assert!(plan.is_safe);
        assert!(plan.repos.is_empty());
        assert_eq!(plan.projected_reclaimed_bytes, 0);
        assert_eq!(plan.projected_retained_bytes, 0);
    }
}
