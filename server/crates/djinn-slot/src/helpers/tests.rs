use super::*;
use async_trait::async_trait;
use djinn_control_plane::bridge::{
    ApiSurfaceEntry, BoundaryRule, BoundaryViolation, ChangeKind, ChangedRange, CycleGroup,
    DeadSymbolEntry, DeprecatedHit, DetectedChangesResult, DetectedTouchedSymbol,
    DiffTouchesResult, EdgeCategory, EdgeEntry, GraphStatus, HotPathHit, HotspotEntry, ImpactEntry,
    ImpactResult, MetricsAtResult, NeighborsResult, OrphanEntry, PagerankTier, PathResult,
    ProjectCtx, RankedNode, RelatedSymbol, RepoGraphOps, SearchHit, SymbolAtHit, SymbolContext,
    SymbolDescription, SymbolNode,
};
use djinn_core::events::EventBus;
use djinn_core::models::Project;
use djinn_db::{Database, ProjectRepository};
use djinn_memory::Note;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
struct FakeRepoGraphOps {
    ranked: Vec<RankedNode>,
    contexts: HashMap<String, SymbolContext>,
    /// PR E3: canned `detect_changes` response. Any non-empty slice
    /// is returned verbatim with the from/to shas echoed back.
    detect_changes_touched: Vec<DetectedTouchedSymbol>,
    /// PR E3: canned `impact` responses keyed by symbol uid. Each
    /// entry yields an `ImpactResult::Detailed`.
    impacts: HashMap<String, Vec<ImpactEntry>>,
}

#[async_trait]
impl RepoGraphOps for FakeRepoGraphOps {
    async fn neighbors(
        &self,
        _: &ProjectCtx,
        _: &str,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<NeighborsResult, String> {
        Err("unused in test".into())
    }
    async fn ranked(
        &self,
        _: &ProjectCtx,
        _workspace: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<RankedNode>, String> {
        Ok(self.ranked.clone())
    }
    async fn implementations(&self, _: &ProjectCtx, _: &str) -> Result<Vec<String>, String> {
        Err("unused in test".into())
    }
    async fn impact(
        &self,
        _: &ProjectCtx,
        _workspace: Option<&str>,
        key: &str,
        _: usize,
        _: Option<&str>,
        _: Option<f64>,
    ) -> Result<ImpactResult, String> {
        // PR E3: return the canned impact entries for the queried
        // key (or an empty detailed list when nothing is canned).
        // Tests that exercise other surfaces leave `impacts` empty,
        // so the previous `unused in test` error is preserved iff
        // they explicitly key off an empty result.
        let entries = self.impacts.get(key).cloned().unwrap_or_default();
        Ok(ImpactResult::Detailed(entries))
    }
    async fn search(
        &self,
        _: &ProjectCtx,
        _: &str,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<SearchHit>, String> {
        Err("unused in test".into())
    }
    async fn cycles(
        &self,
        _: &ProjectCtx,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<CycleGroup>, String> {
        Err("unused in test".into())
    }
    async fn orphans(
        &self,
        _: &ProjectCtx,
        _workspace: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<OrphanEntry>, String> {
        Err("unused in test".into())
    }
    async fn path(
        &self,
        _: &ProjectCtx,
        _workspace: Option<&str>,
        _: &str,
        _: &str,
        _: Option<usize>,
    ) -> Result<Option<PathResult>, String> {
        Err("unused in test".into())
    }
    async fn edges(
        &self,
        _: &ProjectCtx,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<EdgeEntry>, String> {
        Err("unused in test".into())
    }
    async fn describe(&self, _: &ProjectCtx, _: &str) -> Result<Option<SymbolDescription>, String> {
        Err("unused in test".into())
    }
    async fn context(
        &self,
        _: &ProjectCtx,
        key: &str,
        _: bool,
    ) -> Result<Option<djinn_control_plane::bridge::SymbolContext>, String> {
        Ok(self.contexts.get(key).cloned())
    }
    async fn status(&self, _: &ProjectCtx) -> Result<GraphStatus, String> {
        Err("unused in test".into())
    }
    async fn symbols_at(
        &self,
        _: &ProjectCtx,
        _: &str,
        _: u32,
        _: Option<u32>,
    ) -> Result<Vec<SymbolAtHit>, String> {
        Err("unused in test".into())
    }
    async fn diff_touches(
        &self,
        _: &ProjectCtx,
        _: &[ChangedRange],
    ) -> Result<DiffTouchesResult, String> {
        Err("unused in test".into())
    }
    async fn detect_changes(
        &self,
        _: &ProjectCtx,
        from_sha: Option<&str>,
        to_sha: Option<&str>,
        _: &[String],
    ) -> Result<DetectedChangesResult, String> {
        // PR E3: replay the canned touched-symbols list. Tests that
        // exercise other surfaces leave the list empty, which is a
        // valid `detect_changes` shape (the helper treats empty as
        // "no diff signal" and returns None).
        let mut by_file: std::collections::BTreeMap<String, Vec<DetectedTouchedSymbol>> =
            std::collections::BTreeMap::new();
        for sym in &self.detect_changes_touched {
            by_file
                .entry(sym.file_path.clone())
                .or_default()
                .push(sym.clone());
        }
        Ok(DetectedChangesResult {
            from_sha: from_sha.unwrap_or("").to_string(),
            to_sha: to_sha.unwrap_or("").to_string(),
            touched_symbols: self.detect_changes_touched.clone(),
            by_file,
        })
    }
    async fn api_surface(
        &self,
        _: &ProjectCtx,
        _workspace: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<ApiSurfaceEntry>, String> {
        Err("unused in test".into())
    }
    async fn boundary_check(
        &self,
        _: &ProjectCtx,
        _: &[BoundaryRule],
        _: &str,
    ) -> Result<Vec<BoundaryViolation>, String> {
        Err("unused in test".into())
    }
    async fn hotspots(
        &self,
        _: &ProjectCtx,
        _: u32,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<HotspotEntry>, String> {
        Err("unused in test".into())
    }
    async fn complexity(
        &self,
        _: &ProjectCtx,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: usize,
    ) -> Result<djinn_control_plane::bridge::ComplexityResult, String> {
        Err("unused in test".into())
    }
    async fn refactor_candidates(
        &self,
        _: &ProjectCtx,
        _: Option<u32>,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::RefactorCandidate>, String> {
        Err("unused in test".into())
    }
    async fn metrics_at(&self, _: &ProjectCtx) -> Result<MetricsAtResult, String> {
        Err("unused in test".into())
    }
    async fn dead_symbols(
        &self,
        _: &ProjectCtx,
        _: &str,
        _: usize,
    ) -> Result<Vec<DeadSymbolEntry>, String> {
        Err("unused in test".into())
    }
    async fn deprecated_callers(
        &self,
        _: &ProjectCtx,
        _: usize,
    ) -> Result<Vec<DeprecatedHit>, String> {
        Err("unused in test".into())
    }
    async fn touches_hot_path(
        &self,
        _: &ProjectCtx,
        _workspace: Option<&str>,
        _: &[String],
        _: &[String],
        _: &[String],
    ) -> Result<Vec<HotPathHit>, String> {
        Err("unused in test".into())
    }
    async fn coupling(
        &self,
        _: &ProjectCtx,
        _: &str,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::CouplingEntry>, String> {
        Err("unused in test".into())
    }
    async fn churn(
        &self,
        _: &ProjectCtx,
        _: usize,
        _: Option<u32>,
    ) -> Result<Vec<djinn_control_plane::bridge::ChurnEntry>, String> {
        Err("unused in test".into())
    }
    async fn coupling_hotspots(
        &self,
        _: &ProjectCtx,
        _: usize,
        _: Option<u32>,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::CoupledPairEntry>, String> {
        Err("unused in test".into())
    }
    async fn coupling_hubs(
        &self,
        _: &ProjectCtx,
        _: usize,
        _: Option<u32>,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::CouplingHubEntry>, String> {
        Err("unused in test".into())
    }
    async fn resolve(
        &self,
        _: &ProjectCtx,
        _: &str,
        _: Option<&str>,
    ) -> Result<djinn_control_plane::bridge::ResolveOutcome, String> {
        Err("unused in test".into())
    }
    async fn snapshot(
        &self,
        _: &ProjectCtx,
        _workspace: Option<&str>,
        _: djinn_control_plane::bridge::SnapshotLevel,
        _: usize,
        _: &djinn_control_plane::tools::graph_exclusions::GraphExclusions,
    ) -> Result<djinn_control_plane::bridge::SnapshotPayload, String> {
        Err("unused in test".into())
    }
}

async fn setup_project() -> (
    Database,
    crate::host::SlotContext,
    Project,
    tempfile::TempDir,
) {
    let db = Database::open_in_memory().expect("db");
    db.ensure_initialized().await.expect("init db");
    let tmp = crate::test_helpers::test_tempdir("slot-helpers-");
    let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());
    let project = project_repo
        .create("test-project", "test", "test-project")
        .await
        .expect("create project");
    let ctx = crate::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
    (db, ctx, project, tmp)
}

/// Mutex serializing tests that mutate `DJINN_AUTO_CODE_CONTEXT_ROLES`.
/// Tests run in parallel by default and the env var is process-global.
static AUTO_CODE_CONTEXT_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn worker_task(project_id: &str) -> Task {
    Task {
        id: uuid::Uuid::now_v7().to_string(),
        project_id: project_id.to_string(),
        short_id: "wtst".to_string(),
        epic_id: None,
        title: "Refactor server/src/new_area.rs".to_string(),
        description: "Touch server/src/new_area.rs to clean up the helpers in there.".to_string(),
        design: String::new(),
        issue_type: "task".to_string(),
        status: "open".to_string(),
        priority: 1,
        owner: "dev".to_string(),
        labels: "[]".to_string(),
        acceptance_criteria: "[]".to_string(),
        reopen_count: 0,
        continuation_count: 0,
        total_reopen_count: 0,
        intervention_count: 0,
        last_intervention_at: None,
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        closed_at: None,
        close_reason: None,
        merge_commit_sha: None,
        pr_url: None,
        merge_conflict_metadata: None,
        memory_refs: "[]".to_string(),
        agent_type: None,
        created_by_user_id: None,
        ci_status: "unknown".to_string(),
        ci_head_sha: None,
        ci_pr_number: None,
        ci_blocking_required_check_names: "[]".to_string(),
        ci_failure_fingerprint: None,
        ci_first_seen_at: None,
        ci_last_seen_at: None,
        ci_same_signature_count: 0,
        ci_last_remediation_base_sha: None,
        ci_mirror_head_sha: None,
        ci_github_head_sha: None,
        ci_heads_diverged: None,
        ci_head_observation_error: None,
        ci_mq_state: None,
        ci_mq_run_id: None,
        ci_mq_head_sha: None,
        ci_mq_failed_check_names: None,
        ci_mq_failure_fingerprint: None,
        ci_mq_same_signature_count: None,
        ci_mq_first_seen_at: None,
        ci_mq_last_seen_at: None,
        unresolved_blocker_count: 0,
    }
}

fn make_symbol_context(name: &str, file_path: &str) -> SymbolContext {
    let mut outgoing: BTreeMap<EdgeCategory, Vec<RelatedSymbol>> = BTreeMap::new();
    outgoing.insert(
        EdgeCategory::Calls,
        vec![
            RelatedSymbol {
                uid: "symbol:foo".to_string(),
                name: "foo".to_string(),
                kind: "function".to_string(),
                file_path: Some("server/src/new_area.rs".to_string()),
                confidence: 0.9,
                confidence_tier: "extracted".to_string(),
                confidence_reason: None,
                excluded_reason: None,
                route_language_chain: None,
            },
            RelatedSymbol {
                uid: "symbol:bar".to_string(),
                name: "bar".to_string(),
                kind: "function".to_string(),
                file_path: Some("server/src/new_area.rs".to_string()),
                confidence: 0.9,
                confidence_tier: "extracted".to_string(),
                confidence_reason: None,
                excluded_reason: None,
                route_language_chain: None,
            },
            RelatedSymbol {
                uid: "symbol:baz".to_string(),
                name: "baz".to_string(),
                kind: "function".to_string(),
                file_path: Some("server/src/new_area.rs".to_string()),
                confidence: 0.9,
                confidence_tier: "extracted".to_string(),
                confidence_reason: None,
                excluded_reason: None,
                route_language_chain: None,
            },
        ],
    );
    outgoing.insert(
        EdgeCategory::Reads,
        vec![
            RelatedSymbol {
                uid: "symbol:my_field".to_string(),
                name: "my_field".to_string(),
                kind: "field".to_string(),
                file_path: Some("server/src/new_area.rs".to_string()),
                confidence: 0.9,
                confidence_tier: "extracted".to_string(),
                confidence_reason: None,
                excluded_reason: None,
                route_language_chain: None,
            },
            RelatedSymbol {
                uid: "symbol:other_field".to_string(),
                name: "other_field".to_string(),
                kind: "field".to_string(),
                file_path: Some("server/src/new_area.rs".to_string()),
                confidence: 0.9,
                confidence_tier: "extracted".to_string(),
                confidence_reason: None,
                excluded_reason: None,
                route_language_chain: None,
            },
        ],
    );
    // 5 callers across two categories — 5 total.
    let mut incoming: BTreeMap<EdgeCategory, Vec<RelatedSymbol>> = BTreeMap::new();
    incoming.insert(
        EdgeCategory::Calls,
        (0..5)
            .map(|i| RelatedSymbol {
                uid: format!("symbol:caller_{i}"),
                name: format!("caller_{i}"),
                kind: "function".to_string(),
                file_path: None,
                confidence: 0.9,
                confidence_tier: "extracted".to_string(),
                confidence_reason: None,
                excluded_reason: None,
                route_language_chain: None,
            })
            .collect(),
    );
    SymbolContext {
        symbol: SymbolNode {
            uid: format!("symbol:{name}"),
            name: name.to_string(),
            kind: "function".to_string(),
            file_path: file_path.to_string(),
            start_line: 1,
            end_line: 10,
            content: None,
            method_metadata: None,
            complexity: None,
        },
        incoming,
        outgoing,
        processes: vec![],
    }
}

#[test]
fn auto_code_context_role_flag_parses_csv() {
    let _guard = AUTO_CODE_CONTEXT_ENV_LOCK.lock().unwrap();
    // SAFETY: single-threaded section guarded by env-lock mutex.
    unsafe {
        std::env::remove_var(AUTO_CODE_CONTEXT_ROLES_ENV);
    }
    assert!(!is_role_auto_code_context_enabled("worker"));
    assert!(!is_role_auto_code_context_enabled(""));
    unsafe {
        std::env::set_var(AUTO_CODE_CONTEXT_ROLES_ENV, "");
    }
    assert!(!is_role_auto_code_context_enabled("worker"));
    unsafe {
        std::env::set_var(AUTO_CODE_CONTEXT_ROLES_ENV, "worker, REVIEWER");
    }
    assert!(is_role_auto_code_context_enabled("worker"));
    assert!(is_role_auto_code_context_enabled("reviewer"));
    assert!(!is_role_auto_code_context_enabled("planner"));
    unsafe {
        std::env::remove_var(AUTO_CODE_CONTEXT_ROLES_ENV);
    }
}

#[tokio::test]
async fn build_role_code_graph_context_returns_none_when_role_not_enabled() {
    let _guard = AUTO_CODE_CONTEXT_ENV_LOCK.lock().unwrap();
    unsafe {
        std::env::remove_var(AUTO_CODE_CONTEXT_ROLES_ENV);
    }
    let (_db, mut ctx, project, _tmp) = setup_project().await;
    ctx.repo_graph_ops = Some(Arc::new(FakeRepoGraphOps::default()));
    let project_path = "/tmp/proj".to_string();
    let task = worker_task(&project.id);
    let scope_paths = vec!["server/src/new_area.rs".to_string()];
    let result =
        build_role_code_graph_context("worker", &task, &ctx, &project_path, &scope_paths).await;
    assert!(result.is_none());
}

#[tokio::test]
async fn build_role_code_graph_context_emits_bullets_for_enabled_role() {
    let _guard = AUTO_CODE_CONTEXT_ENV_LOCK.lock().unwrap();
    // SAFETY: env mutation guarded by AUTO_CODE_CONTEXT_ENV_LOCK.
    unsafe {
        std::env::set_var(AUTO_CODE_CONTEXT_ROLES_ENV, "worker,reviewer");
    }
    let (_db, mut ctx, project, _tmp) = setup_project().await;
    let bar_key = "symbol:rust pkg server/src/new_area.rs `Bar`#";
    let qux_key = "symbol:rust pkg server/src/new_area.rs `Qux`#";
    let unrelated_key = "symbol:rust pkg other/path.rs `Other`#";
    let mut contexts = HashMap::new();
    contexts.insert(
        bar_key.to_string(),
        make_symbol_context("Bar", "server/src/new_area.rs"),
    );
    contexts.insert(
        qux_key.to_string(),
        make_symbol_context("Qux", "server/src/new_area.rs"),
    );
    contexts.insert(
        unrelated_key.to_string(),
        make_symbol_context("Other", "other/path.rs"),
    );
    ctx.repo_graph_ops = Some(Arc::new(FakeRepoGraphOps {
        ranked: vec![
            RankedNode {
                key: bar_key.to_string(),
                kind: "symbol".to_string(),
                display_name: "Bar".to_string(),
                score: 0.9,
                page_rank: 0.91,
                structural_weight: 1.0,
                inbound_edge_weight: 1.0,
                outbound_edge_weight: 1.0,
                ..Default::default()
            },
            RankedNode {
                key: unrelated_key.to_string(),
                kind: "symbol".to_string(),
                display_name: "Other".to_string(),
                score: 0.85,
                page_rank: 0.4,
                structural_weight: 1.0,
                inbound_edge_weight: 1.0,
                outbound_edge_weight: 1.0,
                ..Default::default()
            },
            RankedNode {
                key: qux_key.to_string(),
                kind: "symbol".to_string(),
                display_name: "Qux".to_string(),
                score: 0.8,
                page_rank: 0.3,
                structural_weight: 1.0,
                inbound_edge_weight: 1.0,
                outbound_edge_weight: 1.0,
                ..Default::default()
            },
        ],
        contexts,
        ..FakeRepoGraphOps::default()
    }));
    let project_path = "/tmp/proj".to_string();
    let task = worker_task(&project.id);
    let scope_paths = vec!["server/src/new_area.rs".to_string()];
    let body = build_role_code_graph_context("worker", &task, &ctx, &project_path, &scope_paths)
        .await
        .expect("worker code-graph context should be present");
    // Bar bullet — top symbol in scope file.
    assert!(
        body.contains(
            "- `server/src/new_area.rs::Bar` (callers: 5, callees: 5, pagerank-tier: high)"
        ),
        "expected Bar bullet, got: {body}"
    );
    // Qux bullet — also in scope file.
    assert!(
        body.contains("`server/src/new_area.rs::Qux`"),
        "expected Qux bullet, got: {body}"
    );
    // Other (out of scope) must be excluded.
    assert!(
        !body.contains("Other"),
        "out-of-scope symbol leaked into bullets: {body}"
    );
    // Sub-bullets render the call / read targets.
    assert!(body.contains("calls: foo, bar, baz"));
    assert!(body.contains("reads: my_field, other_field"));
    // Same role enabled for reviewer too.
    let body_reviewer =
        build_role_code_graph_context("reviewer", &task, &ctx, &project_path, &scope_paths).await;
    assert!(body_reviewer.is_some());
    // Lead role is not in the allowlist → no auto-injection.
    let body_lead =
        build_role_code_graph_context("lead", &task, &ctx, &project_path, &scope_paths).await;
    assert!(body_lead.is_none());
    unsafe {
        std::env::remove_var(AUTO_CODE_CONTEXT_ROLES_ENV);
    }
}

#[tokio::test]
async fn build_role_code_graph_context_skips_when_no_scope_paths() {
    let _guard = AUTO_CODE_CONTEXT_ENV_LOCK.lock().unwrap();
    unsafe {
        std::env::set_var(AUTO_CODE_CONTEXT_ROLES_ENV, "worker");
    }
    let (_db, mut ctx, project, _tmp) = setup_project().await;
    ctx.repo_graph_ops = Some(Arc::new(FakeRepoGraphOps::default()));
    let task = worker_task(&project.id);
    let result = build_role_code_graph_context("worker", &task, &ctx, "/tmp/proj", &[]).await;
    assert!(result.is_none());
    unsafe {
        std::env::remove_var(AUTO_CODE_CONTEXT_ROLES_ENV);
    }
}

fn touched_symbol(
    uid: &str,
    name: &str,
    file_path: &str,
    tier: PagerankTier,
) -> DetectedTouchedSymbol {
    DetectedTouchedSymbol {
        uid: uid.to_string(),
        name: name.to_string(),
        kind: "function".to_string(),
        file_path: file_path.to_string(),
        start_line: 1,
        end_line: 10,
        pagerank_tier: tier,
        change_kind: ChangeKind::Modified,
    }
}

/// Synthesize an `impact` result yielding the requested `(direct,
/// extra_total, modules)` triple. `direct` entries land at depth 1
/// in distinct dummy files spread across `modules` two-segment
/// buckets; `extra_total` entries land at depth 2 in the same
/// buckets to inflate the total impacted set without affecting
/// `direct`.
fn synth_impact(direct: usize, extra_total: usize, modules: usize) -> Vec<ImpactEntry> {
    let mut entries = Vec::with_capacity(direct + extra_total);
    let modules = modules.max(1);
    for i in 0..direct {
        let bucket = i % modules;
        entries.push(ImpactEntry {
            // uid: test-only synthetic symbol key mirrors key.
            uid: format!("symbol:caller_{i}"),
            key: format!("symbol:caller_{i}"),
            depth: 1,
            file_path: Some(format!("crate{bucket}/src/file_{i}.rs")),
            confidence_tier: None,
            exclusion_reason: None,
        });
    }
    for i in 0..extra_total {
        let bucket = i % modules;
        entries.push(ImpactEntry {
            // uid: test-only synthetic symbol key mirrors key.
            uid: format!("symbol:transitive_{i}"),
            key: format!("symbol:transitive_{i}"),
            depth: 2,
            file_path: Some(format!("crate{bucket}/src/transitive_{i}.rs")),
            confidence_tier: None,
            exclusion_reason: None,
        });
    }
    entries
}

#[tokio::test]
async fn build_reviewer_diff_context_returns_none_when_role_not_enabled() {
    let _guard = AUTO_CODE_CONTEXT_ENV_LOCK.lock().unwrap();
    unsafe {
        std::env::remove_var(AUTO_CODE_CONTEXT_ROLES_ENV);
    }
    let (_db, mut ctx, project, _tmp) = setup_project().await;
    ctx.repo_graph_ops = Some(Arc::new(FakeRepoGraphOps {
        detect_changes_touched: vec![touched_symbol(
            "symbol:foo::bar",
            "foo::bar",
            "src/foo.rs",
            PagerankTier::High,
        )],
        ..FakeRepoGraphOps::default()
    }));
    let task = worker_task(&project.id);
    let result = build_reviewer_diff_context(
        "reviewer",
        &task,
        &ctx,
        "/tmp/proj",
        Some("base-sha"),
        Some("head-sha"),
    )
    .await;
    assert!(result.is_none(), "no allowlist entry should skip");
}

#[tokio::test]
async fn build_reviewer_diff_context_skips_without_shas() {
    let _guard = AUTO_CODE_CONTEXT_ENV_LOCK.lock().unwrap();
    unsafe {
        std::env::set_var(AUTO_CODE_CONTEXT_ROLES_ENV, "reviewer");
    }
    let (_db, mut ctx, project, _tmp) = setup_project().await;
    ctx.repo_graph_ops = Some(Arc::new(FakeRepoGraphOps::default()));
    let task = worker_task(&project.id);
    let result =
        build_reviewer_diff_context("reviewer", &task, &ctx, "/tmp/proj", None, None).await;
    assert!(result.is_none(), "no shas → no injection");
    unsafe {
        std::env::remove_var(AUTO_CODE_CONTEXT_ROLES_ENV);
    }
}

#[tokio::test]
async fn build_reviewer_diff_context_emits_sorted_bullets() {
    let _guard = AUTO_CODE_CONTEXT_ENV_LOCK.lock().unwrap();
    unsafe {
        std::env::set_var(AUTO_CODE_CONTEXT_ROLES_ENV, "reviewer");
    }
    let (_db, mut ctx, project, _tmp) = setup_project().await;
    // High-risk: 12 direct callers across 3 modules → HIGH bucket.
    let high_uid = "symbol:auth::middleware::verify_token";
    // Low-risk: 1 direct caller → LOW bucket.
    let low_uid = "symbol:utils::tiny_helper";
    // Critical: 25 direct callers → CRITICAL bucket.
    let critical_uid = "symbol:db::User::from_session";
    let mut impacts: HashMap<String, Vec<ImpactEntry>> = HashMap::new();
    impacts.insert(high_uid.to_string(), synth_impact(12, 0, 3));
    impacts.insert(low_uid.to_string(), synth_impact(1, 0, 1));
    impacts.insert(critical_uid.to_string(), synth_impact(25, 0, 4));
    ctx.repo_graph_ops = Some(Arc::new(FakeRepoGraphOps {
        detect_changes_touched: vec![
            touched_symbol(
                low_uid,
                "utils::tiny_helper",
                "src/utils/mod.rs",
                PagerankTier::Low,
            ),
            touched_symbol(
                high_uid,
                "auth::middleware::verify_token",
                "src/auth/middleware.rs",
                PagerankTier::High,
            ),
            touched_symbol(
                critical_uid,
                "db::User::from_session",
                "src/db/user.rs",
                PagerankTier::High,
            ),
        ],
        impacts,
        ..FakeRepoGraphOps::default()
    }));
    let task = worker_task(&project.id);
    let body = build_reviewer_diff_context(
        "reviewer",
        &task,
        &ctx,
        "/tmp/proj",
        Some("base-sha"),
        Some("head-sha"),
    )
    .await
    .expect("reviewer diff context should be present");
    // Header is rendered.
    assert!(
        body.contains("## Changed symbols (HIGH risk first)"),
        "expected header, got: {body}"
    );
    // Each touched symbol's bullet is rendered with its risk + counts.
    assert!(
        body.contains("`db::User::from_session` (CRITICAL risk, 25 direct callers, 4 modules)"),
        "expected critical bullet, got: {body}"
    );
    assert!(
        body.contains("`auth::middleware::verify_token` (HIGH risk, 12 direct callers, 3 modules)"),
        "expected high bullet, got: {body}"
    );
    assert!(
        body.contains("`utils::tiny_helper` (LOW risk, 1 direct callers, 1 modules)"),
        "expected low bullet, got: {body}"
    );
    // File paths are surfaced on the sub-bullet.
    assert!(
        body.contains("file: src/auth/middleware.rs"),
        "expected file sub-bullet, got: {body}"
    );
    // CRITICAL must come before HIGH must come before LOW.
    let crit_idx = body
        .find("`db::User::from_session`")
        .expect("critical bullet present");
    let high_idx = body
        .find("`auth::middleware::verify_token`")
        .expect("high bullet present");
    let low_idx = body
        .find("`utils::tiny_helper`")
        .expect("low bullet present");
    assert!(crit_idx < high_idx, "CRITICAL should sort before HIGH");
    assert!(high_idx < low_idx, "HIGH should sort before LOW");
    unsafe {
        std::env::remove_var(AUTO_CODE_CONTEXT_ROLES_ENV);
    }
}

#[tokio::test]
async fn build_reviewer_diff_context_returns_none_when_no_touched_symbols() {
    let _guard = AUTO_CODE_CONTEXT_ENV_LOCK.lock().unwrap();
    unsafe {
        std::env::set_var(AUTO_CODE_CONTEXT_ROLES_ENV, "reviewer");
    }
    let (_db, mut ctx, project, _tmp) = setup_project().await;
    ctx.repo_graph_ops = Some(Arc::new(FakeRepoGraphOps::default()));
    let task = worker_task(&project.id);
    let result = build_reviewer_diff_context(
        "reviewer",
        &task,
        &ctx,
        "/tmp/proj",
        Some("base-sha"),
        Some("head-sha"),
    )
    .await;
    assert!(result.is_none(), "empty detect_changes → no injection");
    unsafe {
        std::env::remove_var(AUTO_CODE_CONTEXT_ROLES_ENV);
    }
}
// New tests appended below the existing test suite in helpers/tests.rs.

fn fixture_note(
    note_type: &str,
    title: &str,
    permalink: &str,
    abstract_text: Option<&str>,
    overview_text: Option<&str>,
    content: &str,
    confidence: f64,
) -> Note {
    Note {
        id: format!("note:{title}"),
        project_id: "project_test".to_string(),
        permalink: permalink.to_string(),
        title: title.to_string(),
        file_path: String::new(),
        storage: "db".to_string(),
        note_type: note_type.to_string(),
        folder: permalink.split('/').next().unwrap_or("").to_string(),
        status: "active".to_string(),
        tags: "[]".to_string(),
        content: content.to_string(),
        retrieval_anchor: None,
        created_at: "2026-01-01T00:00:00.000Z".to_string(),
        updated_at: "2026-01-01T00:00:00.000Z".to_string(),
        last_accessed: "2026-01-01T00:00:00.000Z".to_string(),
        access_count: 0,
        confidence,
        abstract_: abstract_text.map(|s| s.to_string()),
        overview: overview_text.map(|s| s.to_string()),
        scope_paths: "[]".to_string(),
    }
}
#[test]
fn format_knowledge_notes_appends_permalink_on_each_line() {
    // Two notes - different types, distinct permalinks - must each surface
    // their permalink on the rendered line and still preserve the existing
    // type / title / summary shape so the prompt's meaning is unchanged.
    let notes = vec![
        fixture_note(
            "pitfall",
            "Refinement target-less",
            "pitfalls/refinement-target-less",
            Some("Refinements on proposals without a target project die as opaque agent_failure."),
            None,
            "Long body content that should NOT appear because abstract wins.",
            0.5,
        ),
        fixture_note(
            "pattern",
            "Anchor Note",
            "patterns/anchor",
            Some("Use anchors for retrieval."),
            None,
            "Body remains separate from the retrieval anchor.",
            0.9,
        ),
    ];

    let rendered = format_knowledge_notes(&notes, 2000);

    assert!(
        rendered.contains(
            "**[Pitfall] Refinement target-less**: Refinements on proposals without a target project die as opaque agent_failure. (permalink: pitfalls/refinement-target-less)",
        ),
        "expected pitfall line with permalink, got: {rendered}"
    );
    assert!(
        rendered.contains(
            "**[Pattern] Anchor Note**: Use anchors for retrieval. (permalink: patterns/anchor)",
        ),
        "expected pattern line with permalink, got: {rendered}"
    );
    assert!(
        !rendered.contains("Long body content that should NOT appear"),
        "body content leaked past abstract selection: {rendered}"
    );
}

#[test]
fn format_knowledge_notes_permalink_visible_when_line_fits_within_budget() {
    let notes = vec![fixture_note(
        "case",
        "Sample Case",
        "cases/sample-case",
        Some("Short case abstract."),
        None,
        "Body text.",
        0.6,
    )];

    let rendered = format_knowledge_notes(&notes, 2000);
    assert_eq!(
        rendered,
        "- **[Case] Sample Case**: Short case abstract. (permalink: cases/sample-case)"
    );
}

#[test]
fn format_knowledge_notes_empty_input_returns_empty_string() {
    let rendered = format_knowledge_notes(&[], 2000);
    assert!(
        rendered.is_empty(),
        "expected empty output, got: {rendered:?}"
    );
}

#[test]
fn format_knowledge_notes_budget_counts_permalink_in_truncation() {
    let notes = vec![
        fixture_note("note", "short", "a/short", Some("a"), None, "", 0.5),
        fixture_note(
            "note",
            "medium-summary-text",
            "b/medium-summary",
            Some("b"),
            None,
            "",
            0.5,
        ),
    ];
    let first_line = "- **[Note] short**: a (permalink: a/short)";
    let second_line = "- **[Note] medium-summary-text**: b (permalink: b/medium-summary)";
    let used_after_first = first_line.len() + 1;
    let budget = used_after_first + second_line.len() - 1;

    let rendered = format_knowledge_notes(&notes, budget);
    assert_eq!(
        rendered, first_line,
        "expected only the first line (with permalink) within budget, got: {rendered:?}"
    );
    assert!(
        !rendered.contains("(permalink: b/medium-summary)"),
        "second note's permalink leaked past budget: {rendered}"
    );
}

#[test]
fn format_knowledge_notes_budget_rejects_line_whose_permalink_itself_overflows() {
    let notes = vec![fixture_note(
        "pattern",
        "Long",
        "patterns/this-permalink-slug-is-intentionally-very-long-on-purpose",
        Some("summary"),
        None,
        "",
        0.5,
    )];

    let rendered = format_knowledge_notes(&notes, 100);
    assert!(
        rendered.is_empty(),
        "single-line overflow must drop the note rather than partial-emit, got: {rendered:?}"
    );
    assert!(
        !rendered.contains("patterns/this-permalink-slug"),
        "overflow line must not leak, got: {rendered}"
    );
}

// ---------------------------------------------------------------------------
// pack_knowledge_notes tests
// ---------------------------------------------------------------------------

#[test]
fn pack_knowledge_notes_rendered_matches_format_knowledge_notes() {
    // The rendered output of pack_knowledge_notes must be byte-identical to
    // format_knowledge_notes for the same inputs, at every budget size.
    let notes = vec![
        fixture_note(
            "pitfall",
            "Refinement target-less",
            "pitfalls/refinement-target-less",
            Some("Refinements on proposals without a target project die as opaque agent_failure."),
            None,
            "Long body content that should NOT appear because abstract wins.",
            0.5,
        ),
        fixture_note(
            "pattern",
            "Anchor Note",
            "patterns/anchor",
            Some("Use anchors for retrieval."),
            None,
            "Body remains separate from the retrieval anchor.",
            0.9,
        ),
    ];

    // Generous budget: both fit.
    assert_eq!(
        pack_knowledge_notes(&notes, 2000).rendered,
        format_knowledge_notes(&notes, 2000),
    );

    // Tight budget: only first fits.
    let first_line = "- **[Pitfall] Refinement target-less**: Refinements on proposals without a target project die as opaque agent_failure. (permalink: pitfalls/refinement-target-less)";
    let budget = first_line.len();
    assert_eq!(
        pack_knowledge_notes(&notes, budget).rendered,
        format_knowledge_notes(&notes, budget),
    );

    // Zero budget: nothing fits.
    assert_eq!(
        pack_knowledge_notes(&notes, 0).rendered,
        format_knowledge_notes(&notes, 0),
    );
}

#[test]
fn pack_knowledge_notes_empty_input_returns_empty() {
    let packed = pack_knowledge_notes(&[], 2000);
    assert!(packed.rendered.is_empty(), "expected empty rendered text");
    assert!(packed.outcomes.is_empty(), "expected empty outcomes");
    assert_eq!(packed.total_injected_chars, 0);
    assert_eq!(packed.total_injected_tokens, 0);
}

#[test]
fn pack_knowledge_notes_all_injected_when_budget_generous() {
    let notes = vec![
        fixture_note(
            "pitfall",
            "Pit One",
            "pitfalls/one",
            Some("Abstract one."),
            None,
            "",
            0.5,
        ),
        fixture_note(
            "pattern",
            "Pat Two",
            "patterns/two",
            Some("Abstract two."),
            None,
            "",
            0.5,
        ),
        fixture_note(
            "case",
            "Case Three",
            "cases/three",
            Some("Abstract three."),
            None,
            "",
            0.5,
        ),
    ];

    let packed = pack_knowledge_notes(&notes, 5000);
    assert_eq!(packed.outcomes.len(), 3);
    for outcome in &packed.outcomes {
        assert_eq!(outcome.disposition, NotePackDisposition::Injected);
        assert!(
            outcome.estimated_rendered_chars.is_some(),
            "injected note must have char estimate"
        );
        assert!(
            outcome.estimated_rendered_tokens.is_some(),
            "injected note must have token estimate"
        );
    }
    assert!(packed.total_injected_chars > 0);
    assert!(packed.total_injected_tokens > 0);
}

#[test]
fn pack_knowledge_notes_budget_prunes_first_overflow_and_all_subsequent() {
    let notes = vec![
        fixture_note("note", "short", "a/short", Some("a"), None, "", 0.5),
        fixture_note(
            "note",
            "medium-summary-text",
            "b/medium-summary",
            Some("b"),
            None,
            "",
            0.5,
        ),
        fixture_note("note", "third-note", "c/third", Some("c"), None, "", 0.5),
    ];

    // Budget only fits the first line.
    let first_line = "- **[Note] short**: a (permalink: a/short)";
    let budget = first_line.len();

    let packed = pack_knowledge_notes(&notes, budget);
    assert_eq!(packed.outcomes.len(), 3);

    // First note injected.
    assert_eq!(
        packed.outcomes[0].disposition,
        NotePackDisposition::Injected
    );
    assert_eq!(packed.outcomes[0].permalink, "a/short");
    assert_eq!(packed.outcomes[0].title, "short");

    // Second note budget-pruned (first overflow).
    assert_eq!(
        packed.outcomes[1].disposition,
        NotePackDisposition::BudgetPruned
    );
    assert_eq!(packed.outcomes[1].permalink, "b/medium-summary");
    assert_eq!(packed.outcomes[1].title, "medium-summary-text");
    assert!(packed.outcomes[1].estimated_rendered_chars.is_none());
    assert!(packed.outcomes[1].estimated_rendered_tokens.is_none());

    // Third note also budget-pruned (cascade after first overflow).
    assert_eq!(
        packed.outcomes[2].disposition,
        NotePackDisposition::BudgetPruned
    );
    assert_eq!(packed.outcomes[2].permalink, "c/third");

    // Rendered text only has the first note.
    assert_eq!(packed.rendered, first_line);
}

#[test]
fn pack_knowledge_notes_zero_budget_prunes_all() {
    let notes = vec![
        fixture_note("note", "A", "a/a", Some("a"), None, "", 0.5),
        fixture_note("note", "B", "b/b", Some("b"), None, "", 0.5),
    ];

    let packed = pack_knowledge_notes(&notes, 0);
    assert_eq!(packed.outcomes.len(), 2);
    for outcome in &packed.outcomes {
        assert_eq!(outcome.disposition, NotePackDisposition::BudgetPruned);
        assert!(outcome.estimated_rendered_chars.is_none());
        assert!(outcome.estimated_rendered_tokens.is_none());
    }
    assert!(packed.rendered.is_empty());
    assert_eq!(packed.total_injected_chars, 0);
    assert_eq!(packed.total_injected_tokens, 0);
}

#[test]
fn pack_knowledge_notes_outcome_metadata_matches_permalink_and_title() {
    let notes = vec![
        fixture_note(
            "pitfall",
            "Refinement target-less",
            "pitfalls/refinement-target-less",
            Some("Refinements on proposals without a target project die as opaque agent_failure."),
            None,
            "",
            0.5,
        ),
        fixture_note(
            "pattern",
            "Anchor Note",
            "patterns/anchor",
            Some("Use anchors for retrieval."),
            None,
            "",
            0.9,
        ),
    ];

    let packed = pack_knowledge_notes(&notes, 2000);
    assert_eq!(
        packed.outcomes[0].permalink,
        "pitfalls/refinement-target-less"
    );
    assert_eq!(packed.outcomes[0].title, "Refinement target-less");
    assert_eq!(packed.outcomes[1].permalink, "patterns/anchor");
    assert_eq!(packed.outcomes[1].title, "Anchor Note");
}

#[test]
fn pack_knowledge_notes_injected_char_estimate_matches_rendered_line_length() {
    let notes = vec![fixture_note(
        "case",
        "Sample Case",
        "cases/sample-case",
        Some("Short case abstract."),
        None,
        "Body text.",
        0.6,
    )];

    let packed = pack_knowledge_notes(&notes, 2000);
    let expected_line =
        "- **[Case] Sample Case**: Short case abstract. (permalink: cases/sample-case)";
    assert_eq!(packed.rendered, expected_line);
    assert_eq!(
        packed.outcomes[0].estimated_rendered_chars,
        Some(expected_line.len()),
        "char estimate must match the actual rendered line length"
    );
}

#[test]
fn pack_knowledge_notes_token_estimate_is_ceil_of_chars_divided_by_four() {
    let notes = vec![fixture_note(
        "note",
        "Tok",
        "t/tok",
        Some("x"),
        None,
        "",
        0.5,
    )];

    let packed = pack_knowledge_notes(&notes, 2000);
    let chars = packed.outcomes[0].estimated_rendered_chars.unwrap();
    let expected_tokens = ((chars as f64) / 4.0).ceil() as usize;
    assert_eq!(
        packed.outcomes[0].estimated_rendered_tokens,
        Some(expected_tokens),
        "token estimate must be ceil(chars / 4.0)"
    );
    // Verify aggregate totals are consistent.
    assert_eq!(packed.total_injected_chars, chars + 1); // +1 for newline
    let expected_total_tokens = ((packed.total_injected_chars as f64) / 4.0).ceil() as usize;
    assert_eq!(packed.total_injected_tokens, expected_total_tokens);
}

#[test]
fn pack_knowledge_notes_budget_permalink_overflow_prunes() {
    // Mirrors the existing format_knowledge_notes_budget_rejects_line_whose_permalink_itself_overflows
    // test, ensuring pack_knowledge_notes behaves identically.
    let notes = vec![fixture_note(
        "pattern",
        "Long",
        "patterns/this-permalink-slug-is-intentionally-very-long-on-purpose",
        Some("summary"),
        None,
        "",
        0.5,
    )];

    let packed = pack_knowledge_notes(&notes, 100);
    assert!(
        packed.rendered.is_empty(),
        "single-line overflow must drop the note, got: {:?}",
        packed.rendered
    );
    assert_eq!(packed.outcomes.len(), 1);
    assert_eq!(
        packed.outcomes[0].disposition,
        NotePackDisposition::BudgetPruned
    );
    assert!(packed.outcomes[0].estimated_rendered_chars.is_none());
}

/// Regression: once the budget is exhausted, subsequent notes must be
/// classified as budget-pruned **without** computing their label, summary,
/// or rendered line content.  The old buggy version would continue
/// evaluating the fallback summary for later notes, panicking on notes
/// whose `content[..min(100)]` lands on a non-UTF-8 byte boundary.
#[test]
fn pack_knowledge_notes_budget_exhausted_skips_content_for_later_notes() {
    // Note 1: overflows budget → triggers budget_exhausted.
    let notes = vec![
        fixture_note(
            "note",
            "overflow",
            "a/overflow",
            Some("This abstract is intentionally long enough to overflow the tiny budget."),
            None,
            "",
            0.5,
        ),
        // Note 2: no abstract/overview, content whose byte 100 is a
        // non-UTF-8 boundary.  The fallback summary `content[..min(100)]`
        // would panic if reached.
        fixture_note(
            "note",
            "utf8-trap",
            "b/trap",
            None,
            None,
            &("a".repeat(99) + "é"), // byte index 100 = inside 'é' (2 bytes)
            0.3,
        ),
    ];

    let budget = 50; // tiny budget; nothing fits
    let packed = pack_knowledge_notes(&notes, budget);

    assert_eq!(packed.outcomes.len(), 2);
    // Both notes must be budget-pruned.
    assert_eq!(
        packed.outcomes[0].disposition,
        NotePackDisposition::BudgetPruned
    );
    assert_eq!(
        packed.outcomes[1].disposition,
        NotePackDisposition::BudgetPruned
    );
    // Rendered output is empty.
    assert!(packed.rendered.is_empty());
    // Crucially: the function must not panic on note 2's non-UTF-8 boundary.
}
