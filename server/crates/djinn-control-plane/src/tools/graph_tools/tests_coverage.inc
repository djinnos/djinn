// glqk: index-coverage contract integration tests.
//
// A two-workspace fixture project with one deliberately broken workspace
// indexer (`server` = Rust, timed out) and one healthy workspace (`ui` =
// TypeScript, indexed). Asserts the gap shows up in:
//   * the `coverage` op (the UI-facing payload),
//   * the per-response coverage advisory (here: on a `dead_symbols` response),
//   * the `impact_check` verdict (escalated to `needs_spike`, naming `server`).

use djinn_db::{
    COVERAGE_STATUS_INDEXED, COVERAGE_STATUS_TIMED_OUT, ProjectWorkspaceCoverageRepository,
    ProjectWorkspaceCoverageUpsert,
};

const COVERAGE_PROJECT: &str = "proj-coverage";

/// Seed the two-workspace coverage fixture into `db` (project row + coverage
/// rows). `server` (rust) timed out; `ui` (typescript) indexed.
async fn seed_coverage_fixture(db: &Database) {
    sqlx::query("INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1,$2,$3,$4)")
        .bind(COVERAGE_PROJECT)
        .bind(COVERAGE_PROJECT)
        .bind("test")
        .bind("coverage-repo")
        .execute(db.pool())
        .await
        .expect("seed project");

    ProjectWorkspaceCoverageRepository::new(db.clone())
        .replace_for_project(
            COVERAGE_PROJECT,
            &[
                ProjectWorkspaceCoverageUpsert {
                    project_id: COVERAGE_PROJECT,
                    workspace_slug: "ui",
                    language: "typescript",
                    status: COVERAGE_STATUS_INDEXED,
                    detail: None,
                    workspace_root: "ui",
                    marker_evidence: Some("tsconfig.json, package.json"),
                    discovered_files: Some(40),
                    indexed_files: Some(40),
                    commit_sha: "c1",
                },
                ProjectWorkspaceCoverageUpsert {
                    project_id: COVERAGE_PROJECT,
                    workspace_slug: "server",
                    language: "rust",
                    status: COVERAGE_STATUS_TIMED_OUT,
                    detail: Some("rust-analyzer scip timed out at 1200s".into()),
                    workspace_root: "server",
                    marker_evidence: Some("Cargo.toml"),
                    discovered_files: Some(300),
                    indexed_files: Some(0),
                    commit_sha: "c1",
                },
            ],
        )
        .await
        .expect("seed coverage rows");
}

fn coverage_ctx() -> ProjectCtx {
    ProjectCtx {
        id: COVERAGE_PROJECT.to_string(),
        clone_path: "/workspace/repo".to_string(),
        workspace: None,
        sub_path: None,
    }
}

fn coverage_server(graph: WorkspaceFixtureOps, db: Database) -> DjinnMcpServer {
    let state = McpState::new(
        db,
        EventBus::noop(),
        CatalogService::new(),
        HealthTracker::new(),
        None,
        None,
        None,
        None,
        Arc::new(crate::state::stubs::StubLspOps),
        Arc::new(crate::state::stubs::StubRuntimeOps),
        Arc::new(crate::state::stubs::StubGitOps),
        Arc::new(graph),
    );
    DjinnMcpServer::new(state)
}

#[tokio::test]
async fn coverage_op_surfaces_the_gap() {
    let db = Database::open_in_memory().expect("db");
    db.ensure_initialized().await.expect("init");
    seed_coverage_fixture(&db).await;
    let server = coverage_server(WorkspaceFixtureOps::new(None), db);

    let mut params = test_params("coverage");
    params.project_id = COVERAGE_PROJECT.to_string();

    let response = server
        .dispatch_code_graph(&coverage_ctx(), &mut params)
        .await
        .expect("coverage dispatch");

    let CodeGraphResponse::Coverage(cov) = response else {
        panic!("expected Coverage response");
    };
    assert!(cov.has_gaps, "the timed-out server workspace is a gap");
    assert_eq!(cov.workspaces.len(), 2);

    let server_row = cov
        .workspaces
        .iter()
        .find(|w| w.workspace_slug == "server")
        .expect("server row");
    assert_eq!(server_row.status, "timed_out");
    assert!(server_row.is_gap);
    assert_eq!(server_row.indexed_files, Some(0));
    assert_eq!(server_row.discovered_files, Some(300));

    // Per-language rollup present for both languages.
    assert_eq!(cov.language_rollup.len(), 2);
    let rust = cov
        .language_rollup
        .iter()
        .find(|r| r.language == "rust")
        .expect("rust rollup");
    assert_eq!(rust.workspaces_gap, 1);
    assert_eq!(rust.workspaces_indexed, 0);

    // Discovered-but-unindexed source roots name the server workspace.
    assert_eq!(cov.unindexed_source_roots.len(), 1);
    assert_eq!(cov.unindexed_source_roots[0].workspace_slug, "server");
}

#[tokio::test]
async fn coverage_advisory_attaches_to_dead_symbols() {
    let db = Database::open_in_memory().expect("db");
    db.ensure_initialized().await.expect("init");
    seed_coverage_fixture(&db).await;

    // Build a bare dead_symbols response and run the advisory attach directly
    // (the same helper the dispatch layer calls on every response).
    let mut response = CodeGraphResponse::DeadSymbols(DeadSymbolsResponse {
        symbols: Vec::new(),
        next_step: None,
        graph_staleness: None,
        coverage: None,
    });
    attach_coverage_advisory(&db, COVERAGE_PROJECT, &mut response).await;

    let CodeGraphResponse::DeadSymbols(dead) = response else {
        panic!("expected DeadSymbols");
    };
    let advisory = dead.coverage.expect("coverage advisory attached");
    assert_eq!(advisory.unindexed_workspaces.len(), 1);
    assert_eq!(advisory.unindexed_workspaces[0].workspace_slug, "server");
    assert!(advisory.message.contains("server"));
}

#[tokio::test]
async fn coverage_advisory_absent_when_clean() {
    let db = Database::open_in_memory().expect("db");
    db.ensure_initialized().await.expect("init");
    // Seed only an indexed workspace — no gap.
    sqlx::query("INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1,$2,$3,$4)")
        .bind("proj-clean")
        .bind("proj-clean")
        .bind("test")
        .bind("clean-repo")
        .execute(db.pool())
        .await
        .expect("seed project");
    ProjectWorkspaceCoverageRepository::new(db.clone())
        .replace_for_project(
            "proj-clean",
            &[ProjectWorkspaceCoverageUpsert {
                project_id: "proj-clean",
                workspace_slug: "server",
                language: "rust",
                status: COVERAGE_STATUS_INDEXED,
                detail: None,
                workspace_root: "server",
                marker_evidence: Some("Cargo.toml"),
                discovered_files: Some(10),
                indexed_files: Some(10),
                commit_sha: "c1",
            }],
        )
        .await
        .expect("seed");

    let mut response = CodeGraphResponse::DeadSymbols(DeadSymbolsResponse {
        symbols: Vec::new(),
        next_step: None,
        graph_staleness: None,
        coverage: None,
    });
    attach_coverage_advisory(&db, "proj-clean", &mut response).await;
    let CodeGraphResponse::DeadSymbols(dead) = response else {
        panic!("expected DeadSymbols");
    };
    assert!(
        dead.coverage.is_none(),
        "no advisory when coverage is clean"
    );
}

#[tokio::test]
async fn impact_check_escalates_to_needs_spike_on_uncovered_scope() {
    let db = Database::open_in_memory().expect("db");
    db.ensure_initialized().await.expect("init");
    seed_coverage_fixture(&db).await;

    // A crate that lives inside the (timed-out) `server` workspace.
    let crate_graph = CrateGraphResponse {
        crates: vec![CrateNodeEntry {
            name: "djinn-graph".to_string(),
            manifest_path: "server/crates/djinn-graph/Cargo.toml".to_string(),
            loc: 100,
            node_count: 10,
            fan_in: 0.0,
            fan_out: 0.0,
            inbound_weight: 0.0,
            outbound_weight: 0.0,
        }],
        edges: Vec::new(),
        message: None,
    };
    let graph = WorkspaceFixtureOps::new(None)
        .with_pinned_commit("c1")
        .with_crate_graph(crate_graph);
    let server = coverage_server(graph, db);

    let mut params = test_params("impact_check");
    params.project_id = COVERAGE_PROJECT.to_string();
    params.impact_targets = Some(vec!["djinn-graph".to_string()]);
    params.scope_crates = Some(vec!["djinn-graph".to_string()]);
    // Match the pinned commit so the staleness gate does NOT fire — we want to
    // exercise the coverage gate, not the stale short-circuit.
    params.current_head = Some("c1".to_string());

    let response = server
        .dispatch_code_graph(&coverage_ctx(), &mut params)
        .await
        .expect("impact_check dispatch");

    let CodeGraphResponse::ImpactCheck(ic) = response else {
        panic!("expected ImpactCheck");
    };
    assert_eq!(
        ic.recommendation, "needs_spike",
        "an unindexed workspace intersecting the scope must escalate"
    );
    assert!(ic.low_confidence);
    let advisory = ic.coverage.expect("coverage advisory on impact_check");
    assert!(
        advisory
            .unindexed_workspaces
            .iter()
            .any(|w| w.workspace_slug == "server"),
        "the escalation must name the uncovered workspace"
    );
    assert!(
        ic.next_step.as_deref().unwrap_or_default().contains("server"),
        "next_step names the uncovered workspace"
    );
}

/// A gap in an UNRELATED workspace (typescript `ui`) must NOT escalate a
/// Rust-scoped impact_check — the design ties escalation to intersection.
#[tokio::test]
async fn impact_check_ignores_unrelated_workspace_gap() {
    let db = Database::open_in_memory().expect("db");
    db.ensure_initialized().await.expect("init");
    // `server` indexed cleanly; only `ui` (typescript) is a gap.
    sqlx::query("INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1,$2,$3,$4)")
        .bind("proj-unrelated")
        .bind("proj-unrelated")
        .bind("test")
        .bind("unrelated-repo")
        .execute(db.pool())
        .await
        .expect("seed project");
    ProjectWorkspaceCoverageRepository::new(db.clone())
        .replace_for_project(
            "proj-unrelated",
            &[
                ProjectWorkspaceCoverageUpsert {
                    project_id: "proj-unrelated",
                    workspace_slug: "server",
                    language: "rust",
                    status: COVERAGE_STATUS_INDEXED,
                    detail: None,
                    workspace_root: "server",
                    marker_evidence: Some("Cargo.toml"),
                    discovered_files: Some(10),
                    indexed_files: Some(10),
                    commit_sha: "c1",
                },
                ProjectWorkspaceCoverageUpsert {
                    project_id: "proj-unrelated",
                    workspace_slug: "ui",
                    language: "typescript",
                    status: COVERAGE_STATUS_TIMED_OUT,
                    detail: Some("scip-typescript timed out".into()),
                    workspace_root: "ui",
                    marker_evidence: Some("tsconfig.json"),
                    discovered_files: Some(20),
                    indexed_files: Some(0),
                    commit_sha: "c1",
                },
            ],
        )
        .await
        .expect("seed");

    let crate_graph = CrateGraphResponse {
        crates: vec![CrateNodeEntry {
            name: "djinn-graph".to_string(),
            manifest_path: "server/crates/djinn-graph/Cargo.toml".to_string(),
            loc: 100,
            node_count: 10,
            fan_in: 0.0,
            fan_out: 0.0,
            inbound_weight: 0.0,
            outbound_weight: 0.0,
        }],
        edges: Vec::new(),
        message: None,
    };
    let graph = WorkspaceFixtureOps::new(None)
        .with_pinned_commit("c1")
        .with_crate_graph(crate_graph);
    let server = coverage_server(graph, db);

    let ctx = ProjectCtx {
        id: "proj-unrelated".to_string(),
        clone_path: "/workspace/repo".to_string(),
        workspace: None,
        sub_path: None,
    };
    let mut params = test_params("impact_check");
    params.project_id = "proj-unrelated".to_string();
    params.impact_targets = Some(vec!["djinn-graph".to_string()]);
    params.scope_crates = Some(vec!["djinn-graph".to_string()]);
    params.current_head = Some("c1".to_string());

    let response = server
        .dispatch_code_graph(&ctx, &mut params)
        .await
        .expect("impact_check dispatch");
    let CodeGraphResponse::ImpactCheck(ic) = response else {
        panic!("expected ImpactCheck");
    };
    assert_ne!(
        ic.recommendation, "needs_spike",
        "a gap in an unrelated (ui) workspace must not escalate a rust-scoped check"
    );
}
