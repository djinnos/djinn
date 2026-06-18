//! In-crate test helpers — mirrors the sliver of
//! `djinn-server::test_helpers` the canonical-graph / repo-map tests need
//! without dragging the server crate in.
//!
//! Exposed as a top-level module under `#[cfg(test)]` so all siblings can
//! consume `crate::test_helpers::*`.
//!
//! Every item in here is also consumed by the in-crate `#[cfg(test)] mod
//! tests` blocks in `canonical_graph.rs`, `index_tree.rs`, etc.  The
//! `pub` visibility on items is harmless in test builds because the
//! surrounding module is gated — production builds never compile this
//! file at all.

#![cfg(test)]
/// Open a fresh test database (isolated Dolt branch via
/// `Database::open_in_memory`).
pub fn create_test_db() -> Database {
    Database::open_in_memory().expect("failed to create test database")
}

/// Create a per-test tempdir rooted under `target/test-tmp` so the tests
/// play nice with our standard clean-up script.
pub fn workspace_tempdir(prefix: &str) -> tempfile::TempDir {
    let base = std::env::current_dir()
        .expect("current dir")
        .join("target")
        .join("test-tmp");
    std::fs::create_dir_all(&base).expect("create djinn-graph test tempdir base");
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(base)
        .expect("create djinn-graph test tempdir")
}

/// Build a deterministic on-disk git repository suitable for driving
/// `ensure_canonical_graph` from the integration test target.
///
/// The fixture shape mirrors the spike `fp53` recommendation: a Rust
/// crate + a TypeScript package with deliberate cross-file Calls edges
/// (a real `cargo`-style `Cargo.toml`, a `package.json`, and source
/// files committed in a single initial commit).  All files live on
/// `main`, so `git rev-parse HEAD` returns a stable SHA across runs
/// (the tempdir is regenerated per test, so the SHA is per-test but
/// stable within a single test invocation).
///
/// The fixture is **filesystem-only** — the warm path runs the
/// `IndexTree::ensure` worktree dance on it.  No SCIP indexer is
/// required to consume the fixture: the integration test pre-seeds
/// `repo_graph_cache` directly with a known blob, and the warm
/// path's cache-hit branch is the only branch that fires in CI lanes
/// without `rust-analyzer` / `scip-typescript` on PATH.
pub async fn make_mixed_workspace(parent: PathBuf) -> (PathBuf, String) {
    let project_root = parent.join("repo");
    tokio::fs::create_dir_all(&project_root)
        .await
        .expect("create mixed-workspace project root");

    async fn run_git(cwd: &Path, args: &[&str]) -> String {
        let output = tokio::process::Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .await
            .expect("spawn git");
        assert!(
            output.status.success(),
            "git {args:?} failed in {}: {}",
            cwd.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    run_git(&project_root, &["init", "-q", "-b", "main"]).await;
    run_git(&project_root, &["config", "user.email", "t@t"]).await;
    run_git(&project_root, &["config", "user.name", "t"]).await;

    // Rust crate — a `lib.rs` that calls into a sibling `helper.rs`.
    let rust_dir = project_root.join("rust-crate").join("src");
    tokio::fs::create_dir_all(&rust_dir)
        .await
        .expect("create rust-crate/src");
    tokio::fs::write(
        rust_dir.join("lib.rs"),
        "pub mod helper;\npub fn alpha() -> i32 { helper::beta() + 1 }\n",
    )
    .await
    .expect("write rust lib.rs");
    tokio::fs::write(rust_dir.join("helper.rs"), "pub fn beta() -> i32 { 42 }\n")
        .await
        .expect("write rust helper.rs");
    tokio::fs::write(
        project_root.join("rust-crate").join("Cargo.toml"),
        "[package]\nname = \"rust-crate\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    )
    .await
    .expect("write Cargo.toml");

    // TypeScript package — an `index.ts` that imports from a sibling
    // `util.ts`.  This is the deliberate cross-file Calls edge shape
    // the spike `fp53` pins; the `index.ts -> util.ts::greet` import
    // is the GitNexus failure mode the proposal explicitly forbids
    // dropping.
    let ts_dir = project_root.join("ts-pkg").join("src");
    tokio::fs::create_dir_all(&ts_dir)
        .await
        .expect("create ts-pkg/src");
    tokio::fs::write(
        ts_dir.join("index.ts"),
        "import { greet } from './util';\nexport const hello = (): string => greet('world');\n",
    )
    .await
    .expect("write ts index.ts");
    tokio::fs::write(
        ts_dir.join("util.ts"),
        "export const greet = (name: string): string => `hi ${name}`;\n",
    )
    .await
    .expect("write ts util.ts");
    tokio::fs::write(
        project_root.join("ts-pkg").join("package.json"),
        "{\n  \"name\": \"ts-pkg\",\n  \"version\": \"0.0.0\",\n  \"type\": \"module\"\n}\n",
    )
    .await
    .expect("write package.json");

    run_git(&project_root, &["add", "."]).await;
    run_git(
        &project_root,
        &["commit", "-q", "-m", "seed mixed workspace"],
    )
    .await;
    let head_sha = run_git(&project_root, &["rev-parse", "HEAD"]).await;

    (project_root, head_sha)
}

/// Minimal [`WarmContext`] backed by an in-memory DB + no-op event bus +
/// per-test indexer mutex.  Suitable for unit tests that don't go through
/// the full `AppState` constructor.
pub struct TestWarmContext {
    db: Database,
    indexer_lock: Arc<tokio::sync::Mutex<()>>,
}

impl TestWarmContext {
    pub fn new(db: Database) -> Self {
        Self {
            db,
            indexer_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

impl WarmContext for TestWarmContext {
    fn db(&self) -> &Database {
        &self.db
    }

    fn event_bus(&self) -> EventBus {
        EventBus::noop()
    }

    fn indexer_lock(&self) -> Arc<tokio::sync::Mutex<()>> {
        self.indexer_lock.clone()
    }
}
