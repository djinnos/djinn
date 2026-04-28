use crate::server::chat::prompt::layout::{
    PromptSegmentStability, compose_system_prompt, compose_system_prompt_segments,
    partition_system_prompt_segments,
};
use crate::server::chat::prompt::system_message::{
    ANTHROPIC_CACHE_BREAKPOINT_KEY, ANTHROPIC_STABLE_PREFIX_KIND, build_system_message,
    system_message_metadata,
};
use serde_json::json;

use super::super::DJINN_CHAT_SYSTEM_PROMPT;

#[test]
fn system_prompt_contains_base_prompt_first_and_project_block_before_client_system() {
    let project_context = "## Current Project\n**Name**: Demo  **Path**: /tmp/demo\n**Open epics**: 1  **Open tasks**: 2\n**Brief**: hello";
    let client_system = "client system message";
    let prompt =
        compose_system_prompt(DJINN_CHAT_SYSTEM_PROMPT, Some(project_context), Some(client_system));

    let base = DJINN_CHAT_SYSTEM_PROMPT.trim();
    assert!(prompt.starts_with(base));
    let base_pos = prompt.find(base).unwrap();
    let project_pos = prompt.find("## Current Project").unwrap();
    let client_pos = prompt.find(client_system).unwrap();
    assert!(base_pos <= project_pos);
    assert!(project_pos < client_pos);
}

#[test]
fn system_prompt_segments_mark_stable_project_context_for_caching() {
    let project_context = "## Current Project\nproject";
    let client_system = "be concise";

    let segments = compose_system_prompt_segments(
        DJINN_CHAT_SYSTEM_PROMPT,
        Some(project_context),
        Some(client_system),
    );

    assert_eq!(segments.len(), 3);
    assert_eq!(segments[0].stability, PromptSegmentStability::Stable);
    assert_eq!(segments[1].text, project_context);
    assert_eq!(segments[1].stability, PromptSegmentStability::Stable);
    assert_eq!(segments[2].text, client_system);
    assert_eq!(segments[2].stability, PromptSegmentStability::Dynamic);
}

#[test]
fn system_message_metadata_uses_explicit_anthropic_breakpoint_contract() {
    let metadata = system_message_metadata("anthropic/claude-3-5-sonnet", true)
        .expect("anthropic stable prefix should emit metadata");
    let provider_data = metadata.provider_data.expect("provider data");

    assert_eq!(
        provider_data,
        json!({
            ANTHROPIC_CACHE_BREAKPOINT_KEY: {
                "kind": ANTHROPIC_STABLE_PREFIX_KIND,
            }
        })
    );
    assert!(system_message_metadata("openai/gpt-4o", true).is_none());
    assert!(system_message_metadata("anthropic/claude-3-5-sonnet", false).is_none());
}

#[test]
fn build_system_message_preserves_segment_ordering() {
    let project_context = "## Current Project\nproject";
    let message = build_system_message(
        DJINN_CHAT_SYSTEM_PROMPT,
        Some(project_context),
        Some("volatile client system"),
        "anthropic/claude-3-5-sonnet",
    );

    assert_eq!(message.content.len(), 3);
    assert_eq!(
        message.content[0].as_text(),
        Some(DJINN_CHAT_SYSTEM_PROMPT.trim())
    );
    assert_eq!(message.content[1].as_text(), Some(project_context));
    assert_eq!(message.content[2].as_text(), Some("volatile client system"));
}

#[test]
fn build_system_message_skips_cache_breakpoint_for_non_anthropic() {
    let openai_message = build_system_message(
        DJINN_CHAT_SYSTEM_PROMPT,
        Some("## Current Project\nproject"),
        None,
        "openai/gpt-4o",
    );
    assert!(openai_message.metadata.is_none());

    let anthropic_base_only = build_system_message(
        DJINN_CHAT_SYSTEM_PROMPT,
        None,
        Some("volatile client system"),
        "anthropic/claude-3-5-sonnet",
    );
    assert!(anthropic_base_only.metadata.is_some());
}

#[test]
fn compose_segments_skips_empty_optional_segments() {
    let segments = compose_system_prompt_segments(DJINN_CHAT_SYSTEM_PROMPT, Some(""), Some("  \n "));
    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].text, DJINN_CHAT_SYSTEM_PROMPT.trim());
}

#[test]
fn partition_system_prompt_segments_extracts_explicit_dynamic_tail_boundary() {
    let segments = compose_system_prompt_segments(
        DJINN_CHAT_SYSTEM_PROMPT,
        Some("project ctx"),
        Some("client system\n\ntask context"),
    );

    let layout = partition_system_prompt_segments(&segments);

    assert_eq!(layout.stable_prefix.len(), 2);
    assert_eq!(
        layout.stable_prefix[0].text,
        DJINN_CHAT_SYSTEM_PROMPT.trim()
    );
    assert_eq!(layout.stable_prefix[1].text, "project ctx");
    assert_eq!(
        layout.dynamic_tail.as_deref(),
        Some("client system\n\ntask context")
    );
}

#[test]
fn build_system_message_threads_codebase_header_through_project_context_slot() {
    // PR E1 — the auto-injected `📦 CURRENT CODEBASE` block is wired
    // into the existing `project_context` slot so the resulting message
    // keeps the (base, project_context, client_system) ordering plus
    // the Anthropic cache breakpoint at the end of the stable prefix.
    let header = "## 📦 CURRENT CODEBASE\n\n**Status**: graph warmed, commit `abc12345`\n\n**Top hotspots** (by PageRank):\n- `auth::verify_token` (0.87)";
    let message = build_system_message(
        DJINN_CHAT_SYSTEM_PROMPT,
        Some(header),
        Some("be brief"),
        "anthropic/claude-3-5-sonnet",
    );

    assert_eq!(message.content.len(), 3);
    assert_eq!(
        message.content[0].as_text(),
        Some(DJINN_CHAT_SYSTEM_PROMPT.trim())
    );
    let project_block = message.content[1].as_text().expect("project block present");
    assert!(project_block.starts_with("## 📦 CURRENT CODEBASE"));
    assert!(project_block.contains("auth::verify_token"));
    assert_eq!(message.content[2].as_text(), Some("be brief"));
    // Stable-prefix cache breakpoint stays anchored to the last cached
    // block so the codebase header lives behind it.
    assert!(message.metadata.is_some());
}

#[tokio::test]
async fn codebase_header_builder_renders_status_hotspots_and_tree() {
    use crate::server::chat::prompt::codebase_header::{
        build_codebase_header, clear_cache_for_tests,
    };
    use async_trait::async_trait;
    use djinn_control_plane::bridge::{
        GraphStatus, ProjectCtx, RankedNode, RepoGraphOps,
    };
    use djinn_control_plane::test_support::StubRepoGraph;
    use std::sync::Arc;

    /// Stub stand-in: the chat system prompt assembly site receives a
    /// header text it can embed verbatim. We assert the contract — the
    /// builder produces a markdown block carrying status, hotspots, and
    /// a folder tree from the project root — using a fake ops layer
    /// that delegates everything but `status`/`ranked` to
    /// `StubRepoGraph`.
    struct CapturedOps;

    #[async_trait]
    impl RepoGraphOps for CapturedOps {
        async fn status(&self, _ctx: &ProjectCtx) -> Result<GraphStatus, String> {
            Ok(GraphStatus {
                project_id: "test".into(),
                warmed: true,
                last_warm_at: Some("2026-04-28T00:00:00Z".into()),
                pinned_commit: Some("deadbeef00".into()),
                commits_since_pin: Some(0),
            })
        }
        async fn ranked(
            &self,
            _ctx: &ProjectCtx,
            _kind: Option<&str>,
            _sort_by: Option<&str>,
            _limit: usize,
        ) -> Result<Vec<RankedNode>, String> {
            Ok(vec![RankedNode {
                key: "symbol:auth::verify_token".into(),
                kind: "function".into(),
                display_name: "auth::verify_token".into(),
                score: 0.87,
                page_rank: 0.87,
                structural_weight: 0.0,
                inbound_edge_weight: 0.0,
                outbound_edge_weight: 0.0,
            }])
        }
        // Delegate the long tail.
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
        async fn implementations(&self, ctx: &ProjectCtx, s: &str) -> Result<Vec<String>, String> {
            StubRepoGraph.implementations(ctx, s).await
        }
        async fn impact(
            &self,
            ctx: &ProjectCtx,
            k: &str,
            d: usize,
            g: Option<&str>,
            mc: Option<f64>,
        ) -> Result<djinn_control_plane::bridge::ImpactResult, String> {
            StubRepoGraph.impact(ctx, k, d, g, mc).await
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
            r: &[djinn_control_plane::bridge::ChangedRange],
        ) -> Result<djinn_control_plane::bridge::DiffTouchesResult, String> {
            StubRepoGraph.diff_touches(ctx, r).await
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

    clear_cache_for_tests();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("src/api")).unwrap();
    std::fs::create_dir_all(tmp.path().join("tests")).unwrap();

    let ops: Arc<dyn RepoGraphOps> = Arc::new(CapturedOps);
    let header = build_codebase_header(ops, "p-system-prompt", tmp.path())
        .await
        .expect("header should render");

    // Use it the way the handler does — feed into build_system_message
    // via the `project_context` slot — and verify the resulting message
    // carries the new section verbatim.
    let message = build_system_message(
        DJINN_CHAT_SYSTEM_PROMPT,
        Some(&header),
        Some("be brief"),
        "anthropic/claude-3-5-sonnet",
    );
    let combined: String = message
        .content
        .iter()
        .filter_map(|b| b.as_text())
        .collect::<Vec<_>>()
        .join("\n\n");
    assert!(combined.contains("## 📦 CURRENT CODEBASE"));
    assert!(combined.contains("graph warmed"));
    assert!(combined.contains("auth::verify_token"));
    assert!(combined.contains("- src/"));
    assert!(combined.contains("be brief"));
    // Budget — header itself stays under 2KB even with all three signals.
    assert!(header.len() <= 2_000);
}

#[test]
fn build_system_message_only_dynamic_tail_never_creates_cacheable_trailing_block() {
    let message = build_system_message(
        DJINN_CHAT_SYSTEM_PROMPT,
        None,
        Some("client system\n\ntask context"),
        "anthropic/claude-3-5-sonnet",
    );

    assert_eq!(message.content.len(), 2);
    assert_eq!(
        message.content[0].as_text(),
        Some(DJINN_CHAT_SYSTEM_PROMPT.trim())
    );
    assert_eq!(
        message.content[1].as_text(),
        Some("client system\n\ntask context")
    );
    assert!(message.metadata.is_some());
}
