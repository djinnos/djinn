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

mod indexing;
mod workspaces;

pub(crate) use indexing::run_indexers_already_locked;

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
            Self::TypeScript => vec!["index".to_string(), output],
            Self::Python => vec!["index".to_string(), output],
            // scip-go: positional non-flag args are package patterns (default
            // `./...`). Giving the output path positionally silently narrows
            // indexing to ~nothing and dumps ./index.scip in cwd, which the
            // artifact collector never looks at. `-o` routes the SCIP output
            // to the planner-chosen path.
            Self::Go => vec!["index".to_string(), "-o".to_string(), output],
            Self::Java => vec!["index".to_string(), output],
            Self::Clang => vec![
                "--compdb-path".to_string(),
                ".".to_string(),
                "--output-path".to_string(),
                output,
            ],
            Self::Ruby => vec!["index".to_string(), output],
            Self::DotNet => vec!["index".to_string(), output],
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
    pub output_path: PathBuf,
}

impl PlannedIndexerCommand {
    fn build_command(&self) -> Command {
        let mut command = Command::new(&self.binary_path);
        command.current_dir(&self.working_directory);
        command.args(&self.args);
        // ADR-050 §3 non-negotiable cap: SCIP indexers commonly invoke
        // `cargo check` which fans out into a parallel `cc` build for
        // native deps (openssl-sys et al).  Two simultaneous indexer
        // runs are already serialized by `IndexerLock`, but a single
        // run can still saturate the host without this cap.  4 jobs is
        // empirically sufficient to keep `rust-analyzer scip` warm
        // without melting the box.
        command.env("CARGO_BUILD_JOBS", "4");
        // For Rust: rust-analyzer is a rustup shim that honors the
        // repo's `rust-toolchain.toml`. If the pin points at a version
        // whose `rust-analyzer` component we didn't install in the
        // image, rustup errors with "Unknown binary 'rust-analyzer'".
        // Force rustup to use the image's default toolchain (where we
        // did install rust-analyzer) regardless of the repo's pin.
        // The SCIP output is toolchain-independent for our purposes —
        // we want symbol resolution, not edition-sensitive type
        // inference — so this is safe.
        if self.indexer == SupportedIndexer::RustAnalyzer {
            command.env("RUSTUP_TOOLCHAIN", "stable");
        }
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexingRun {
    pub project_root: PathBuf,
    pub output_root: PathBuf,
    pub commands: Vec<ExecutedIndexerCommand>,
    pub artifacts: Vec<ScipArtifact>,
}
