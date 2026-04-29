//! Auto-injected `📦 CURRENT CODEBASE` block for the chat system prompt.
//!
//! When [`is_enabled`] returns `true` and the chat request carries a
//! `project` ref, [`build_codebase_header`] produces a compact markdown
//! block that gives the model structural awareness of the active
//! project without forcing the user to call `code_graph` first.
//!
//! The block is built from three parallel queries against the canonical
//! graph (`status` → node/edge counts + last-warm timestamp, `ranked
//! sort_by=pagerank limit=8` → top hotspots) plus a depth-2 ASCII folder
//! tree walked from the project's clone path. Each query has its own
//! short budget; on individual failures we log + skip the failed signal
//! and emit whatever is available.
//!
//! Results are memoized in an in-process `HashMap<(project_id, head),
//! CachedHeader>` for 60s to avoid hammering the graph cache on every
//! chat turn. The cache key combines the project id with the warmed
//! `pinned_commit` (when present) so a re-warm naturally invalidates
//! the cached header.
//!
//! Behind the `DJINN_CHAT_AUTO_CODEBASE_HEADER` env var, default
//! `false` until the soak completes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

use djinn_control_plane::bridge::{ProjectCtx, RankedNode, RepoGraphOps};
use tokio::time::timeout;

/// Maximum total length of the rendered header (markdown). Keeps the
/// cache breakpoint stable and bounds the cost of feeding this into
/// every chat turn.
const HEADER_BUDGET_CHARS: usize = 2_000;

/// Per-query budget for `status` and `ranked`. Each has 1s on the wall
/// clock; on timeout we drop the signal and emit what we have.
const PER_QUERY_BUDGET: Duration = Duration::from_millis(1_500);

/// Memoization TTL — the header is cheap to render but the underlying
/// queries hit the canonical graph + filesystem. 60s matches the
/// hot-path knob in similar caches and is short enough that the user
/// rarely sees stale numbers after a re-warm.
const CACHE_TTL: Duration = Duration::from_secs(60);

/// Folder-tree depth. Depth-2 captures the typical top-level layout
/// (`src/`, `tests/`, etc.) plus their immediate children without
/// drowning the prompt.
const FOLDER_TREE_DEPTH: usize = 2;

/// Top-N hotspots emitted from the `ranked` signal. The plan calls for
/// 8; we cap here so the budget doesn't blow up on noisy graphs.
const HOTSPOTS_LIMIT: usize = 8;

/// Directory names skipped by the folder-tree walker. Mirrors the
/// usual VCS / build-artifact ignore set.
const FOLDER_TREE_SKIP: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "dist",
    "build",
    ".djinn",
    ".idea",
    ".vscode",
    "__pycache__",
    ".venv",
    "venv",
];

/// Read the `DJINN_CHAT_AUTO_CODEBASE_HEADER` env var. Default
/// `false` — the header only renders when an operator has opted in.
pub(in crate::server::chat) fn is_enabled() -> bool {
    match std::env::var("DJINN_CHAT_AUTO_CODEBASE_HEADER") {
        Ok(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "true" | "1" | "on" | "yes"
        ),
        Err(_) => false,
    }
}

#[derive(Clone)]
struct CachedHeader {
    text: String,
    inserted_at: Instant,
}

type HeaderCache = RwLock<HashMap<(String, String), CachedHeader>>;

fn header_cache() -> &'static HeaderCache {
    static CACHE: OnceLock<HeaderCache> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Build the `📦 CURRENT CODEBASE` block for `(project_id, clone_path)`,
/// or `None` when nothing useful is available (e.g. graph not warmed
/// and folder walk failed).
///
/// Caches successful renders for [`CACHE_TTL`]. Cache key is
/// `(project_id, pinned_commit_or_warm_marker)` so a re-warm or the
/// graph going from cold→warm invalidates the cached entry.
pub(in crate::server::chat) async fn build_codebase_header(
    ops: Arc<dyn RepoGraphOps>,
    project_id: &str,
    clone_path: &Path,
) -> Option<String> {
    let ctx = ProjectCtx {
        id: project_id.to_owned(),
        clone_path: clone_path.to_string_lossy().into_owned(),
    };

    // Run status, ranked, and folder-tree in parallel. status_result
    // doubles as the cache key source via `pinned_commit`; we always
    // wait for it.
    let status_fut = timeout(PER_QUERY_BUDGET, ops.status(&ctx));
    let ranked_fut = timeout(
        PER_QUERY_BUDGET,
        ops.ranked(&ctx, None, Some("pagerank"), HOTSPOTS_LIMIT),
    );
    let clone_path_owned = clone_path.to_path_buf();
    let tree_fut = tokio::task::spawn_blocking(move || folder_tree(&clone_path_owned, FOLDER_TREE_DEPTH));

    let (status_outcome, ranked_outcome, tree_outcome) = tokio::join!(status_fut, ranked_fut, tree_fut);

    let status = match status_outcome {
        Ok(Ok(s)) => Some(s),
        Ok(Err(err)) => {
            tracing::warn!(project_id, %err, "codebase_header: graph status failed");
            None
        }
        Err(_) => {
            tracing::warn!(project_id, "codebase_header: graph status timed out");
            None
        }
    };

    // Build a cache key. When the graph is warmed we use the pinned
    // commit; otherwise a short marker so cold-graph headers still
    // memoize across rapid chat turns.
    let cache_key_tail = status
        .as_ref()
        .and_then(|s| s.pinned_commit.clone())
        .unwrap_or_else(|| {
            if status.as_ref().map(|s| s.warmed).unwrap_or(false) {
                "warmed-no-commit".to_string()
            } else {
                "cold".to_string()
            }
        });
    let cache_key = (project_id.to_string(), cache_key_tail);

    if let Some(cached) = lookup_cache(&cache_key) {
        return Some(cached);
    }

    let ranked = match ranked_outcome {
        Ok(Ok(r)) => r,
        Ok(Err(err)) => {
            tracing::warn!(project_id, %err, "codebase_header: ranked failed");
            Vec::new()
        }
        Err(_) => {
            tracing::warn!(project_id, "codebase_header: ranked timed out");
            Vec::new()
        }
    };

    let tree = match tree_outcome {
        Ok(t) => t,
        Err(err) => {
            tracing::warn!(project_id, %err, "codebase_header: folder-tree task panicked");
            None
        }
    };

    let header = render_header(status.as_ref(), &ranked, tree.as_deref());
    if header.is_empty() {
        return None;
    }
    let truncated = truncate_to_budget(&header, HEADER_BUDGET_CHARS);
    insert_cache(cache_key, truncated.clone());
    Some(truncated)
}

fn lookup_cache(key: &(String, String)) -> Option<String> {
    let guard = header_cache().read().ok()?;
    let entry = guard.get(key)?;
    if entry.inserted_at.elapsed() < CACHE_TTL {
        Some(entry.text.clone())
    } else {
        None
    }
}

fn insert_cache(key: (String, String), text: String) {
    if let Ok(mut guard) = header_cache().write() {
        // Opportunistic eviction: drop expired entries while we hold
        // the writer. Bounded by the number of distinct projects in a
        // chat-active deployment so this stays cheap.
        guard.retain(|_, v| v.inserted_at.elapsed() < CACHE_TTL);
        guard.insert(
            key,
            CachedHeader {
                text,
                inserted_at: Instant::now(),
            },
        );
    }
}

#[cfg(test)]
pub(in crate::server::chat) fn clear_cache_for_tests() {
    if let Ok(mut guard) = header_cache().write() {
        guard.clear();
    }
}

/// Render the markdown block from the three signals. Any signal can
/// be missing — the resulting block lists what's available and skips
/// what isn't. Returns an empty string when *nothing* is available.
fn render_header(
    status: Option<&djinn_control_plane::bridge::GraphStatus>,
    ranked: &[RankedNode],
    folder_tree: Option<&str>,
) -> String {
    let mut sections: Vec<String> = Vec::new();

    if let Some(status_line) = render_status_line(status) {
        sections.push(format!("**Status**: {status_line}"));
    }

    if !ranked.is_empty() {
        let mut lines = vec!["**Top hotspots** (by PageRank):".to_string()];
        for node in ranked.iter().take(HOTSPOTS_LIMIT) {
            lines.push(format!(
                "- `{}` ({:.2})",
                node.display_name, node.page_rank
            ));
        }
        sections.push(lines.join("\n"));
    }

    if let Some(tree) = folder_tree.filter(|t| !t.trim().is_empty()) {
        sections.push(format!("**Folder tree (depth {FOLDER_TREE_DEPTH})**:\n{tree}"));
    }

    if sections.is_empty() {
        return String::new();
    }

    let mut header = String::from("## 📦 CURRENT CODEBASE\n\n");
    header.push_str(&sections.join("\n\n"));
    header
}

fn render_status_line(status: Option<&djinn_control_plane::bridge::GraphStatus>) -> Option<String> {
    let status = status?;
    let mut parts = Vec::new();
    if status.warmed {
        parts.push("graph warmed".to_string());
        if let Some(commit) = &status.pinned_commit {
            let short = if commit.len() > 8 { &commit[..8] } else { commit.as_str() };
            parts.push(format!("commit `{short}`"));
        }
        if let Some(last_warm) = &status.last_warm_at {
            parts.push(format!("warmed at {last_warm}"));
        }
        if let Some(commits_since) = status.commits_since_pin {
            if commits_since > 0 {
                parts.push(format!("{commits_since} commit(s) ahead"));
            }
        }
    } else {
        parts.push("graph not yet warmed".to_string());
    }
    Some(parts.join(", "))
}

/// Truncate `s` to at most `budget` bytes on a UTF-8 boundary, appending
/// a single `…` marker when truncation occurs. The marker counts toward
/// the budget so the returned string is always `<= budget` bytes.
fn truncate_to_budget(s: &str, budget: usize) -> String {
    if s.len() <= budget {
        return s.to_string();
    }
    const MARKER: char = '…';
    let marker_len = MARKER.len_utf8();
    if budget <= marker_len {
        return MARKER.to_string();
    }
    let mut end = budget - marker_len;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push(MARKER);
    out
}

/// Walk `root` to `depth` levels and emit a sorted bullet list of
/// directories. Files are not listed — only the directory structure,
/// which is what the model needs for layout awareness.
///
/// Skips entries in [`FOLDER_TREE_SKIP`] and any path whose final
/// component starts with `.` (hidden files / VCS metadata).
pub(in crate::server::chat) fn folder_tree(root: &Path, depth: usize) -> Option<String> {
    if !root.is_dir() {
        return None;
    }
    let mut lines: Vec<String> = Vec::new();
    walk_dir(root, 0, depth, &mut lines).ok()?;
    if lines.is_empty() {
        return None;
    }
    Some(lines.join("\n"))
}

fn walk_dir(
    dir: &Path,
    current_depth: usize,
    max_depth: usize,
    out: &mut Vec<String>,
) -> std::io::Result<()> {
    if current_depth >= max_depth {
        return Ok(());
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') && !FOLDER_TREE_SKIP.contains(&name) {
                // Skip generic hidden dirs (.cache, .pytest_cache, …)
                return false;
            }
            !FOLDER_TREE_SKIP.contains(&name)
        })
        .collect();
    entries.sort();
    for entry in entries {
        let name = match entry.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_owned(),
            None => continue,
        };
        let indent = "  ".repeat(current_depth);
        out.push(format!("{indent}- {name}/"));
        walk_dir(&entry, current_depth + 1, max_depth, out)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use djinn_control_plane::bridge::{GraphStatus, RankedNode};
    use djinn_control_plane::test_support::StubRepoGraph;
    use std::sync::Arc;

    /// Test fake that delegates everything to [`StubRepoGraph`] except
    /// `status` and `ranked`, which the auto-codebase header reads.
    #[derive(Default)]
    struct FakeOps {
        status: Option<GraphStatus>,
        ranked: Vec<RankedNode>,
        status_err: Option<String>,
        ranked_err: Option<String>,
    }

    #[async_trait]
    impl RepoGraphOps for FakeOps {
        async fn status(&self, ctx: &ProjectCtx) -> Result<GraphStatus, String> {
            if let Some(err) = &self.status_err {
                return Err(err.clone());
            }
            if let Some(status) = &self.status {
                return Ok(status.clone());
            }
            StubRepoGraph.status(ctx).await
        }
        async fn ranked(
            &self,
            _ctx: &ProjectCtx,
            _kind: Option<&str>,
            _sort_by: Option<&str>,
            _limit: usize,
        ) -> Result<Vec<RankedNode>, String> {
            if let Some(err) = &self.ranked_err {
                return Err(err.clone());
            }
            Ok(self.ranked.clone())
        }
        // Delegate every other method to StubRepoGraph.
        async fn neighbors(
            &self,
            ctx: &ProjectCtx,
            key: &str,
            d: Option<&str>,
            g: Option<&str>,
            kf: Option<&str>,
        ) -> Result<djinn_control_plane::bridge::NeighborsResult, String> {
            StubRepoGraph.neighbors(ctx, key, d, g, kf).await
        }
        async fn implementations(
            &self,
            ctx: &ProjectCtx,
            sym: &str,
        ) -> Result<Vec<String>, String> {
            StubRepoGraph.implementations(ctx, sym).await
        }
        async fn impact(
            &self,
            ctx: &ProjectCtx,
            key: &str,
            depth: usize,
            g: Option<&str>,
            mc: Option<f64>,
        ) -> Result<djinn_control_plane::bridge::ImpactResult, String> {
            StubRepoGraph.impact(ctx, key, depth, g, mc).await
        }
        async fn search(
            &self,
            ctx: &ProjectCtx,
            q: &str,
            kf: Option<&str>,
            l: usize,
        ) -> Result<Vec<djinn_control_plane::bridge::SearchHit>, String> {
            StubRepoGraph.search(ctx, q, kf, l).await
        }
        async fn cycles(
            &self,
            ctx: &ProjectCtx,
            kf: Option<&str>,
            ms: usize,
        ) -> Result<Vec<djinn_control_plane::bridge::CycleGroup>, String> {
            StubRepoGraph.cycles(ctx, kf, ms).await
        }
        async fn orphans(
            &self,
            ctx: &ProjectCtx,
            kf: Option<&str>,
            v: Option<&str>,
            l: usize,
        ) -> Result<Vec<djinn_control_plane::bridge::OrphanEntry>, String> {
            StubRepoGraph.orphans(ctx, kf, v, l).await
        }
        async fn path(
            &self,
            ctx: &ProjectCtx,
            f: &str,
            t: &str,
            md: Option<usize>,
        ) -> Result<Option<djinn_control_plane::bridge::PathResult>, String> {
            StubRepoGraph.path(ctx, f, t, md).await
        }
        async fn edges(
            &self,
            ctx: &ProjectCtx,
            fg: &str,
            tg: &str,
            ek: Option<&str>,
            l: usize,
        ) -> Result<Vec<djinn_control_plane::bridge::EdgeEntry>, String> {
            StubRepoGraph.edges(ctx, fg, tg, ek, l).await
        }
        async fn describe(
            &self,
            ctx: &ProjectCtx,
            k: &str,
        ) -> Result<Option<djinn_control_plane::bridge::SymbolDescription>, String> {
            StubRepoGraph.describe(ctx, k).await
        }
        async fn context(
            &self,
            ctx: &ProjectCtx,
            k: &str,
            ic: bool,
        ) -> Result<Option<djinn_control_plane::bridge::SymbolContext>, String> {
            StubRepoGraph.context(ctx, k, ic).await
        }
        async fn symbols_at(
            &self,
            ctx: &ProjectCtx,
            f: &str,
            sl: u32,
            el: Option<u32>,
        ) -> Result<Vec<djinn_control_plane::bridge::SymbolAtHit>, String> {
            StubRepoGraph.symbols_at(ctx, f, sl, el).await
        }
        async fn diff_touches(
            &self,
            ctx: &ProjectCtx,
            ranges: &[djinn_control_plane::bridge::ChangedRange],
        ) -> Result<djinn_control_plane::bridge::DiffTouchesResult, String> {
            StubRepoGraph.diff_touches(ctx, ranges).await
        }
        async fn detect_changes(
            &self,
            ctx: &ProjectCtx,
            f: Option<&str>,
            t: Option<&str>,
            cf: &[String],
        ) -> Result<djinn_control_plane::bridge::DetectedChangesResult, String> {
            StubRepoGraph.detect_changes(ctx, f, t, cf).await
        }
        async fn api_surface(
            &self,
            ctx: &ProjectCtx,
            mg: Option<&str>,
            v: Option<&str>,
            l: usize,
        ) -> Result<Vec<djinn_control_plane::bridge::ApiSurfaceEntry>, String> {
            StubRepoGraph.api_surface(ctx, mg, v, l).await
        }
        async fn boundary_check(
            &self,
            ctx: &ProjectCtx,
            r: &[djinn_control_plane::bridge::BoundaryRule],
        ) -> Result<Vec<djinn_control_plane::bridge::BoundaryViolation>, String> {
            StubRepoGraph.boundary_check(ctx, r).await
        }
        async fn hotspots(
            &self,
            ctx: &ProjectCtx,
            wd: u32,
            fg: Option<&str>,
            l: usize,
        ) -> Result<Vec<djinn_control_plane::bridge::HotspotEntry>, String> {
            StubRepoGraph.hotspots(ctx, wd, fg, l).await
        }
        async fn complexity(
            &self,
            ctx: &ProjectCtx,
            t: &str,
            sb: &str,
            fg: Option<&str>,
            l: usize,
        ) -> Result<djinn_control_plane::bridge::ComplexityResult, String> {
            StubRepoGraph.complexity(ctx, t, sb, fg, l).await
        }
        async fn refactor_candidates(
            &self,
            ctx: &ProjectCtx,
            sd: Option<u32>,
            fg: Option<&str>,
            l: usize,
        ) -> Result<Vec<djinn_control_plane::bridge::RefactorCandidate>, String> {
            StubRepoGraph.refactor_candidates(ctx, sd, fg, l).await
        }
        async fn metrics_at(
            &self,
            ctx: &ProjectCtx,
        ) -> Result<djinn_control_plane::bridge::MetricsAtResult, String> {
            StubRepoGraph.metrics_at(ctx).await
        }
        async fn dead_symbols(
            &self,
            ctx: &ProjectCtx,
            c: &str,
            l: usize,
        ) -> Result<Vec<djinn_control_plane::bridge::DeadSymbolEntry>, String> {
            StubRepoGraph.dead_symbols(ctx, c, l).await
        }
        async fn deprecated_callers(
            &self,
            ctx: &ProjectCtx,
            l: usize,
        ) -> Result<Vec<djinn_control_plane::bridge::DeprecatedHit>, String> {
            StubRepoGraph.deprecated_callers(ctx, l).await
        }
        async fn touches_hot_path(
            &self,
            ctx: &ProjectCtx,
            se: &[String],
            ss: &[String],
            s: &[String],
        ) -> Result<Vec<djinn_control_plane::bridge::HotPathHit>, String> {
            StubRepoGraph.touches_hot_path(ctx, se, ss, s).await
        }
        async fn coupling(
            &self,
            ctx: &ProjectCtx,
            fp: &str,
            l: usize,
        ) -> Result<Vec<djinn_control_plane::bridge::CouplingEntry>, String> {
            StubRepoGraph.coupling(ctx, fp, l).await
        }
        async fn churn(
            &self,
            ctx: &ProjectCtx,
            l: usize,
            sd: Option<u32>,
        ) -> Result<Vec<djinn_control_plane::bridge::ChurnEntry>, String> {
            StubRepoGraph.churn(ctx, l, sd).await
        }
        async fn coupling_hotspots(
            &self,
            ctx: &ProjectCtx,
            l: usize,
            sd: Option<u32>,
            mfpc: usize,
        ) -> Result<Vec<djinn_control_plane::bridge::CoupledPairEntry>, String> {
            StubRepoGraph.coupling_hotspots(ctx, l, sd, mfpc).await
        }
        async fn coupling_hubs(
            &self,
            ctx: &ProjectCtx,
            l: usize,
            sd: Option<u32>,
            mfpc: usize,
        ) -> Result<Vec<djinn_control_plane::bridge::CouplingHubEntry>, String> {
            StubRepoGraph.coupling_hubs(ctx, l, sd, mfpc).await
        }
        async fn resolve(
            &self,
            ctx: &ProjectCtx,
            k: &str,
            kh: Option<&str>,
        ) -> Result<djinn_control_plane::bridge::ResolveOutcome, String> {
            StubRepoGraph.resolve(ctx, k, kh).await
        }
        async fn snapshot(
            &self,
            ctx: &ProjectCtx,
            cap: usize,
            ex: &djinn_control_plane::tools::graph_exclusions::GraphExclusions,
        ) -> Result<djinn_control_plane::bridge::SnapshotPayload, String> {
            StubRepoGraph.snapshot(ctx, cap, ex).await
        }
    }

    fn warmed_status() -> GraphStatus {
        GraphStatus {
            project_id: "p1".into(),
            warmed: true,
            last_warm_at: Some("2026-04-28T00:00:00Z".into()),
            pinned_commit: Some("abc1234567890".into()),
            commits_since_pin: Some(0),
        }
    }

    fn ranked_node(name: &str, score: f64) -> RankedNode {
        RankedNode {
            key: format!("symbol:{name}"),
            kind: "function".into(),
            display_name: name.into(),
            score,
            page_rank: score,
            structural_weight: 0.0,
            inbound_edge_weight: 0.0,
            outbound_edge_weight: 0.0,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn full_header_includes_status_hotspots_and_tree() {
        clear_cache_for_tests();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src/api")).unwrap();
        std::fs::create_dir_all(tmp.path().join("src/models")).unwrap();
        std::fs::create_dir_all(tmp.path().join("tests")).unwrap();
        // skipped dirs
        std::fs::create_dir_all(tmp.path().join("target/debug")).unwrap();
        std::fs::create_dir_all(tmp.path().join(".git/objects")).unwrap();

        let ops: Arc<dyn RepoGraphOps> = Arc::new(FakeOps {
            status: Some(warmed_status()),
            ranked: vec![
                ranked_node("verify_token", 0.87),
                ranked_node("from_session", 0.81),
            ],
            ..Default::default()
        });

        let header = build_codebase_header(ops, "p1-uniq-full", tmp.path())
            .await
            .expect("header should be produced");

        assert!(header.starts_with("## 📦 CURRENT CODEBASE"));
        assert!(header.contains("graph warmed"));
        assert!(header.contains("commit `abc12345`"));
        assert!(header.contains("Top hotspots"));
        assert!(header.contains("verify_token"));
        assert!(header.contains("0.87"));
        assert!(header.contains("Folder tree"));
        assert!(header.contains("- src/"));
        assert!(header.contains("- tests/"));
        // Skipped paths must not leak into the tree.
        assert!(!header.contains("target/"));
        assert!(!header.contains(".git"));
        assert!(header.len() <= HEADER_BUDGET_CHARS);
    }

    #[tokio::test]
    async fn header_renders_with_only_status_when_others_fail() {
        clear_cache_for_tests();
        let tmp = tempfile::tempdir().unwrap();
        // No subdirs → folder tree is None.

        let ops: Arc<dyn RepoGraphOps> = Arc::new(FakeOps {
            status: Some(warmed_status()),
            ranked_err: Some("graph cold".into()),
            ..Default::default()
        });

        let header = build_codebase_header(ops, "p1-uniq-only-status", tmp.path())
            .await
            .expect("status alone is enough to render");
        assert!(header.contains("graph warmed"));
        assert!(!header.contains("Top hotspots"));
        assert!(!header.contains("Folder tree"));
    }

    #[tokio::test]
    async fn cold_graph_with_tree_still_emits_header() {
        clear_cache_for_tests();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();

        let ops: Arc<dyn RepoGraphOps> = Arc::new(FakeOps {
            status: Some(GraphStatus {
                project_id: "p1".into(),
                warmed: false,
                last_warm_at: None,
                pinned_commit: None,
                commits_since_pin: None,
            }),
            ..Default::default()
        });

        let header = build_codebase_header(ops, "p1-uniq-cold", tmp.path())
            .await
            .expect("cold graph + tree should still render");
        assert!(header.contains("graph not yet warmed"));
        assert!(header.contains("Folder tree"));
    }

    #[tokio::test]
    async fn returns_none_when_everything_fails() {
        clear_cache_for_tests();
        let tmp = tempfile::tempdir().unwrap();
        // Empty dir → folder_tree is None too.

        let ops: Arc<dyn RepoGraphOps> = Arc::new(FakeOps {
            status_err: Some("no warm".into()),
            ranked_err: Some("no warm".into()),
            ..Default::default()
        });

        let header = build_codebase_header(ops, "p1-uniq-none", tmp.path()).await;
        assert!(header.is_none());
    }

    #[tokio::test]
    async fn cache_returns_same_header_within_ttl() {
        clear_cache_for_tests();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();

        let ops: Arc<dyn RepoGraphOps> = Arc::new(FakeOps {
            status: Some(warmed_status()),
            ranked: vec![ranked_node("first_call", 0.9)],
            ..Default::default()
        });
        let first = build_codebase_header(ops.clone(), "p1-uniq-cache", tmp.path())
            .await
            .unwrap();

        // Second call uses a different stub but should hit the cache.
        let ops2: Arc<dyn RepoGraphOps> = Arc::new(FakeOps {
            status: Some(warmed_status()),
            ranked: vec![ranked_node("second_call_should_not_appear", 0.99)],
            ..Default::default()
        });
        let second = build_codebase_header(ops2, "p1-uniq-cache", tmp.path())
            .await
            .unwrap();
        assert_eq!(first, second);
        assert!(second.contains("first_call"));
        assert!(!second.contains("second_call_should_not_appear"));
    }

    #[test]
    fn folder_tree_skips_hidden_and_artifact_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        for d in ["src", "tests", "target", ".git", ".cache", "node_modules"] {
            std::fs::create_dir_all(tmp.path().join(d)).unwrap();
        }
        let tree = folder_tree(tmp.path(), 1).unwrap();
        assert!(tree.contains("- src/"));
        assert!(tree.contains("- tests/"));
        assert!(!tree.contains("target"));
        assert!(!tree.contains(".git"));
        assert!(!tree.contains(".cache"));
        assert!(!tree.contains("node_modules"));
    }

    #[test]
    fn truncate_to_budget_handles_utf8_and_marker() {
        let big = "A".repeat(2_500);
        let cut = truncate_to_budget(&big, 100);
        assert!(cut.len() <= 100);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn is_enabled_defaults_false() {
        // The harness may have leaked an env var from a previous test,
        // so explicitly clear it for this assertion.
        // SAFETY: tests in this module are not parallel against the
        // env-var write path on the same key.
        unsafe {
            std::env::remove_var("DJINN_CHAT_AUTO_CODEBASE_HEADER");
        }
        assert!(!is_enabled());
    }
}
