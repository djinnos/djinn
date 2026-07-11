//! SCIP indexer fan-out plus artifact collection.
//!
//! Previously this code was nested under `repo_map` because an aider-style
//! rendered repo map was the sole consumer. The map was never wired up to
//! production readers and has been replaced by a commit-based file-coupling
//! index (see [`crate::coupling_index`]); the indexer half lives on because
//! the canonical graph pipeline (`crate::canonical_graph::ensure_canonical_graph`)
//! still needs SCIP artifacts to feed [`crate::scip_parser`].
//!
//! The public surface is deliberately narrow — `SupportedIndexer`,
//! `ScipArtifact`, and `run_indexers_already_locked`. Detection /
//! workspace discovery / command planning live as submodule internals
//! because their only call site is the warmer pipeline.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

mod budget;
pub(crate) mod cache;
mod indexing;
#[cfg(test)]
mod warm_cost_regression;
pub mod workspaces;

pub(crate) use indexing::{append_graph_cache_shrink_warning, run_indexers_already_locked};
pub use workspaces::workspace_slug;

/// Tracks `(project_root, indexer)` pairs we have already logged a
/// "missing indexer binary" notice for, so the periodic warmer does
/// not spam the same info line every cycle.
static MISSING_INDEXER_LOGGED: Mutex<Option<HashSet<(PathBuf, SupportedIndexer)>>> =
    Mutex::new(None);

fn note_missing_indexer_once(project_root: &Path, indexer: SupportedIndexer) -> bool {
    let mut guard = match MISSING_INDEXER_LOGGED.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let set = guard.get_or_insert_with(HashSet::new);
    set.insert((project_root.to_path_buf(), indexer))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SupportedIndexer {
    RustAnalyzer,
    TypeScript,
    Python,
    Go,
    Java,
    Clang,
    Ruby,
    DotNet,
}

impl SupportedIndexer {
    pub const ALL: [Self; 8] = [
        Self::RustAnalyzer,
        Self::TypeScript,
        Self::Python,
        Self::Go,
        Self::Java,
        Self::Clang,
        Self::Ruby,
        Self::DotNet,
    ];

    pub fn binary_name(self) -> &'static str {
        match self {
            Self::RustAnalyzer => "rust-analyzer",
            Self::TypeScript => "scip-typescript",
            Self::Python => "scip-python",
            Self::Go => "scip-go",
            Self::Java => "scip-java",
            Self::Clang => "scip-clang",
            Self::Ruby => "scip-ruby",
            Self::DotNet => "scip-dotnet",
        }
    }

    pub fn language(self) -> &'static str {
        match self {
            Self::RustAnalyzer => "rust",
            Self::TypeScript => "typescript",
            Self::Python => "python",
            Self::Go => "go",
            Self::Java => "java",
            Self::Clang => "cpp",
            Self::Ruby => "ruby",
            Self::DotNet => "csharp",
        }
    }

    /// Parse the stable per-language storage key produced by [`Self::language`]
    /// back into a [`SupportedIndexer`]. Used to reload persisted timing rows
    /// (`scip_indexer_timing.indexer`) into the budget's prior-timing map.
    /// Returns `None` for unknown / retired keys so stale rows are ignored.
    pub fn from_language_key(key: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|indexer| indexer.language() == key)
    }

    /// Wall-clock cap for a single invocation of this indexer.
    ///
    /// `rust-analyzer scip` drives a full `cargo metadata` + analysis pass.
    /// On a COLD `/cache` (the *first* warm of a large edition-2024 workspace
    /// like `platform`) that routinely needs several minutes — the old flat
    /// 120s cap timed it out on every run, so the warm never produced an index
    /// and never populated the build cache that would make later runs fast →
    /// the warm Pod exited 1 forever and re-dispatched every ~60s. Give Rust a
    /// generous cap (still well under the 1800s warm-Job `activeDeadline`); the
    /// other indexers are comparatively cheap, so keep them on the short cap so
    /// a genuinely hung indexer can't pin the warm Job for ten minutes.
    pub(crate) fn timeout(self) -> std::time::Duration {
        let secs = match self {
            // rust-analyzer drives a full cargo metadata + analysis pass.
            // Measured on djinnos/djinn (≈10MB of Rust, warm /cache, 6-CPU
            // warm pod): 527s — within 3% of the old 600s cap, so any CPU
            // contention from co-scheduled task-run pods tipped it over and
            // the server workspace silently dropped out of the graph.
            // 1200s keeps real headroom while staying under the warm-Job
            // activeDeadline with room for the TS targets.
            Self::RustAnalyzer => 1200,
            // scip-typescript at a monorepo root indexes the whole tree
            // (alt-front-end root = ~60s of input files) — needs headroom.
            Self::TypeScript => 600,
            _ => 120,
        };
        std::time::Duration::from_secs(secs)
    }

    fn marker_files(self) -> &'static [&'static str] {
        match self {
            Self::RustAnalyzer => &["Cargo.toml"],
            Self::TypeScript => &["tsconfig.json", "package.json"],
            Self::Python => &["pyproject.toml", "setup.py"],
            Self::Go => &["go.mod"],
            Self::Java => &["build.gradle", "pom.xml"],
            Self::Clang => &["CMakeLists.txt", "compile_commands.json"],
            Self::Ruby => &["Gemfile"],
            Self::DotNet => &["*.csproj", "*.sln"],
        }
    }

    fn default_output_path(
        self,
        project_root: &Path,
        output_root: &Path,
        workspace_slug: &str,
    ) -> PathBuf {
        let project_name = project_root
            .file_name()
            .and_then(OsStr::to_str)
            .filter(|name| !name.is_empty())
            .unwrap_or("project");
        output_root.join(format!(
            "{project_name}-{}-{workspace_slug}.scip",
            self.language()
        ))
    }

    fn command_args(self, output_path: &Path) -> Vec<String> {
        let output = output_path.to_string_lossy().into_owned();
        match self {
            Self::RustAnalyzer => vec![
                "scip".to_string(),
                ".".to_string(),
                "--output".to_string(),
                output,
            ],
            // `--output` is REQUIRED: scip-typescript's positional args are
            // *projects* to index, not the output path (which defaults to
            // `index.scip`). Passing the planned `.scip` path positionally made
            // scip-typescript try to index that path as a TS project → every
            // target failed "missing tsconfig.json" / "no files got indexed".
            Self::TypeScript => vec!["index".to_string(), "--output".to_string(), output],
            // Same `--output` requirement as TypeScript: `scip-python index`
            // takes NO positional and writes to `--output` (default
            // `index.scip` relative to cwd). Passing the planned path
            // positionally made scip-python ignore it and dump `index.scip`
            // into the workspace, so djinn's collector found nothing in the
            // output dir → exit 0 but `node_count=0` (empty graph).
            Self::Python => vec!["index".to_string(), "--output".to_string(), output],
            // scip-go: positional non-flag args are package patterns (default
            // `./...`). Giving the output path positionally silently narrows
            // indexing to ~nothing and dumps ./index.scip in cwd, which the
            // artifact collector never looks at. `-o` routes the SCIP output
            // to the planner-chosen path.
            Self::Go => vec!["index".to_string(), "-o".to_string(), output],
            // scip-java `index` writes to `--output` (default `index.scip`);
            // the planned path must be passed via the flag, same as TS/Python.
            // NOTE: not exercised in this deployment (no Java projects) — fix
            // matches the upstream `--output` convention but is unverified
            // against a live scip-java run.
            Self::Java => vec!["index".to_string(), "--output".to_string(), output],
            Self::Clang => vec![
                "--compdb-path".to_string(),
                ".".to_string(),
                "--index-output-path".to_string(),
                output,
            ],
            // FIXME(ruby): scip-ruby has NO `index` subcommand — it runs as
            // `bundle exec scip-ruby --index-file <out> --gem-metadata <name@ver>
            // <dirs>` inside the project's bundler context. This `["index",
            // output]` form is wrong on every axis (subcommand, output flag,
            // missing bundler env) and will not produce an index. Left as-is
            // pending a proper rewrite — no Ruby projects use it today.
            Self::Ruby => vec!["index".to_string(), output],
            // scip-dotnet `index` writes to `--output` (default `index.scip`),
            // same pattern as TS/Python. NOTE: unverified locally (no .NET
            // toolchain here); matches the documented scip-dotnet CLI.
            Self::DotNet => vec!["index".to_string(), "--output".to_string(), output],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredWorkspace {
    pub indexer: SupportedIndexer,
    pub root: PathBuf,
    pub slug: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexerAvailability {
    pub indexer: SupportedIndexer,
    pub binary: String,
    pub path: Option<PathBuf>,
}

impl IndexerAvailability {
    pub fn is_available(&self) -> bool {
        self.path.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedIndexerCommand {
    pub indexer: SupportedIndexer,
    pub binary_path: PathBuf,
    pub args: Vec<String>,
    pub working_directory: PathBuf,
    pub workspace_root: PathBuf,
    /// Workspace root RELATIVE to the project root (empty for the repo
    /// root itself). SCIP document paths come out relative to the
    /// indexer's working directory, so this prefix is what re-roots them
    /// to repo-relative at parse time — see `parse_scip_artifacts`.
    pub workspace_rel_root: PathBuf,
    pub workspace_slug: String,
    pub output_path: PathBuf,
}

/// Ceiling on `CARGO_BUILD_JOBS` for a SCIP indexer subprocess. Preserves the
/// historical ADR-050 §3 cap of 4 so a large host can never fan `cargo check`
/// out past what the warm pipeline was tuned for.
const MAX_CARGO_BUILD_JOBS: usize = 4;

/// Derive the `CARGO_BUILD_JOBS` value for a SCIP indexer subprocess.
///
/// SCIP indexers (notably `rust-analyzer scip`) invoke `cargo check`, which
/// fans out into a parallel `cc` build for native deps (openssl-sys et al).
/// The warm Pod is CPU-capped (`warm_cpu_limit`), runs its indexers at
/// `nice 10`, and runs the Rust indexer concurrently with the scip-typescript
/// indexers, so hardcoding 4 build jobs on a 2-CPU pod *oversubscribed* the
/// budget: 4 jobs fighting over 2 throttled CPUs is slower than 2 jobs, and
/// that contention is one of the causes of the Rust workspace hitting its
/// wall-clock cap on every warm.
///
/// `std::thread::available_parallelism()` reads the process's cgroup CPU quota
/// on Linux (stable since Rust 1.64; the pinned toolchain is `stable`), so
/// inside the warm Pod it reflects the container's `warm_cpu_limit` rather than
/// the node's core count. We cap the result at [`MAX_CARGO_BUILD_JOBS`] to keep
/// the historical ceiling and fall back to it when detection fails, and floor
/// at 1 so a mis-detected 0 can never disable parallelism entirely.
fn cargo_build_jobs() -> String {
    let jobs = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(MAX_CARGO_BUILD_JOBS)
        .clamp(1, MAX_CARGO_BUILD_JOBS);
    jobs.to_string()
}

impl PlannedIndexerCommand {
    fn build_command(&self) -> Command {
        let mut command = Command::new(&self.binary_path);
        command.current_dir(&self.working_directory);
        command.args(&self.args);
        // ADR-050 §3 cap: SCIP indexers commonly invoke `cargo check` which
        // fans out into a parallel `cc` build for native deps (openssl-sys
        // et al). Two simultaneous indexer runs are already serialized by
        // `IndexerLock`, but a single run can still saturate the host without
        // a cap. Derive the job count from the CPU actually available to this
        // process (cgroup-aware) rather than hardcoding 4 — on the CPU-capped
        // warm Pod, 4 jobs oversubscribed the quota and made warms slower.
        command.env("CARGO_BUILD_JOBS", cargo_build_jobs());
        // NOTE: do NOT force `RUSTUP_TOOLCHAIN=stable` here. The image's
        // install-rust.sh installs the `rust-analyzer` component into the
        // project's *pinned* toolchain (derived from rust-toolchain.toml
        // at build time, e.g. 1.94.1) and runs `rustup default <that>`.
        // `stable` is never installed, so forcing it made rustup try to
        // download a fresh `stable` (which lacks rust-analyzer under the
        // minimal profile) → "Unknown binary 'rust-analyzer' in toolchain
        // 'stable'", failing every Rust index. Leaving RUSTUP_TOOLCHAIN
        // unset lets the rust-analyzer shim resolve the repo pin / image
        // default — exactly the toolchain that has the component.
        command
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutedIndexerCommand {
    pub plan: PlannedIndexerCommand,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScipArtifact {
    pub path: PathBuf,
    pub indexer: Option<SupportedIndexer>,
    pub workspace_slug: String,
    /// Workspace root RELATIVE to the project root (empty for the repo
    /// root). Used by `parse_scip_artifacts` to re-root the index's
    /// workspace-relative document paths to repo-relative — without it,
    /// every consumer that joins node `file_path`s onto the project root
    /// (complexity, route extraction, `symbols_at`, `diff_touches`,
    /// file_glob filters) silently misses sub-workspace files.
    pub workspace_root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceWarmStatus {
    pub workspace_slug: String,
    pub indexer: SupportedIndexer,
    pub status: String,
    pub detail: Option<String>,
}

/// Outcome class of a single indexer invocation, as recorded for the adaptive
/// timeout budget. Only actual invocations produce one — cache hits and
/// deadline-skipped plans never ran, so they yield no observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexerRunOutcome {
    Success,
    Failed,
    TimedOut,
}

impl IndexerRunOutcome {
    /// Stable string key persisted in `scip_indexer_timing.last_status`; must
    /// match the `djinn_db::TIMING_STATUS_*` constants.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
        }
    }
}

/// One indexer invocation's timing, surfaced on [`IndexingRun`] so the warm
/// caller can persist it (keyed by workspace slug + indexer) for the next
/// warm's adaptive budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexerTimingObservation {
    pub workspace_slug: String,
    pub indexer: SupportedIndexer,
    pub outcome: IndexerRunOutcome,
    pub elapsed_ms: u64,
}

/// Prior-timing evidence for one (workspace, indexer) key, loaded from
/// persisted [`IndexerTimingObservation`]s and fed back into the budget model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexerPriorTiming {
    /// Elapsed (ms) of the last successful run.
    pub last_success_ms: Option<u64>,
    /// High-water elapsed (ms) of timed-out runs since the last success.
    pub last_timed_out_ms: Option<u64>,
}

/// Map of prior timings keyed by `(workspace_slug, indexer)`, passed into the
/// warm run so each plan can size its timeout from history.
pub type PriorTimingMap = std::collections::HashMap<(String, SupportedIndexer), IndexerPriorTiming>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexingRun {
    pub project_root: PathBuf,
    pub output_root: PathBuf,
    pub commands: Vec<ExecutedIndexerCommand>,
    pub artifacts: Vec<ScipArtifact>,
    pub workspace_statuses: Vec<WorkspaceWarmStatus>,
    /// Per-invocation timings from THIS run, for persistence into the adaptive
    /// budget history. Empty when no indexer actually executed (all cache hits
    /// / deadline-skipped / no indexers on PATH).
    pub timings: Vec<IndexerTimingObservation>,
}

#[cfg(test)]
mod build_command_tests {
    use super::*;

    /// `CARGO_BUILD_JOBS` is derived from available parallelism, floored at 1
    /// and capped at the historical ADR-050 §3 ceiling of 4. It must always be
    /// a parseable positive integer within that band so the value passed to
    /// cargo is never `0`, negative, or over the cap regardless of host CPU.
    #[test]
    fn cargo_build_jobs_is_bounded_and_parseable() {
        let jobs: usize = cargo_build_jobs()
            .parse()
            .expect("CARGO_BUILD_JOBS must be a positive integer");
        assert!(
            (1..=MAX_CARGO_BUILD_JOBS).contains(&jobs),
            "expected 1..={MAX_CARGO_BUILD_JOBS} build jobs, got {jobs}"
        );
        // It should also match the cgroup-aware detection clamped to the band,
        // so on the CPU-capped warm Pod it tracks the CPU limit rather than the
        // node core count.
        let expected = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(MAX_CARGO_BUILD_JOBS)
            .clamp(1, MAX_CARGO_BUILD_JOBS);
        assert_eq!(jobs, expected);
    }

    /// The built command carries the derived `CARGO_BUILD_JOBS` env override.
    #[test]
    fn build_command_sets_cargo_build_jobs_env() {
        let plan = PlannedIndexerCommand {
            indexer: SupportedIndexer::RustAnalyzer,
            binary_path: PathBuf::from("rust-analyzer"),
            args: vec!["scip".into()],
            working_directory: PathBuf::from("/tmp"),
            workspace_root: PathBuf::from("/tmp"),
            workspace_rel_root: PathBuf::new(),
            workspace_slug: "root".into(),
            output_path: PathBuf::from("/tmp/out.scip"),
        };
        let command = plan.build_command();
        let jobs = command
            .get_envs()
            .find(|(k, _)| *k == OsStr::new("CARGO_BUILD_JOBS"))
            .and_then(|(_, v)| v)
            .map(|v| v.to_string_lossy().into_owned());
        assert_eq!(jobs.as_deref(), Some(cargo_build_jobs().as_str()));
    }
}
