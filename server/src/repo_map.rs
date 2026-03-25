use std::collections::HashSet;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::process;

const SCIP_ARTIFACT_EXTENSION: &str = "scip";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SupportedIndexer {
    RustAnalyzer,
    TypeScript,
    Python,
    Go,
    Java,
}

impl SupportedIndexer {
    pub const ALL: [Self; 5] = [
        Self::RustAnalyzer,
        Self::TypeScript,
        Self::Python,
        Self::Go,
        Self::Java,
    ];

    pub fn binary_name(self) -> &'static str {
        match self {
            Self::RustAnalyzer => "rust-analyzer",
            Self::TypeScript => "scip-typescript",
            Self::Python => "scip-python",
            Self::Go => "scip-go",
            Self::Java => "scip-java",
        }
    }

    pub fn language(self) -> &'static str {
        match self {
            Self::RustAnalyzer => "rust",
            Self::TypeScript => "typescript",
            Self::Python => "python",
            Self::Go => "go",
            Self::Java => "java",
        }
    }

    fn default_output_path(self, project_root: &Path, output_root: &Path) -> PathBuf {
        let project_name = project_root
            .file_name()
            .and_then(OsStr::to_str)
            .filter(|name| !name.is_empty())
            .unwrap_or("project");
        output_root.join(format!("{project_name}-{}.scip", self.language()))
    }

    fn command_args(self, project_root: &Path, output_root: &Path) -> Vec<String> {
        let output_path = self.default_output_path(project_root, output_root);
        let output = output_path.to_string_lossy().into_owned();
        match self {
            Self::RustAnalyzer => vec!["scip".to_string(), output],
            Self::TypeScript => vec!["index".to_string(), output],
            Self::Python => vec!["index".to_string(), output],
            Self::Go => vec!["index".to_string(), output],
            Self::Java => vec!["index".to_string(), output],
        }
    }
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
    pub output_path: PathBuf,
}

impl PlannedIndexerCommand {
    fn build_command(&self) -> Command {
        let mut command = Command::new(&self.binary_path);
        command.current_dir(&self.working_directory);
        command.args(&self.args);
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

pub fn detect_indexers() -> Vec<IndexerAvailability> {
    let path_var = env::var("PATH").unwrap_or_default();
    SupportedIndexer::ALL
        .into_iter()
        .map(|indexer| IndexerAvailability {
            indexer,
            binary: indexer.binary_name().to_string(),
            path: which_in_path(indexer.binary_name(), &path_var),
        })
        .collect()
}

pub fn plan_indexer_commands(
    project_root: impl AsRef<Path>,
    output_root: impl AsRef<Path>,
    available_indexers: &[IndexerAvailability],
) -> Vec<PlannedIndexerCommand> {
    let project_root = project_root.as_ref();
    let output_root = output_root.as_ref();

    available_indexers
        .iter()
        .filter_map(|availability| {
            availability.path.as_ref().map(|binary_path| {
                let output_path = availability
                    .indexer
                    .default_output_path(project_root, output_root);
                PlannedIndexerCommand {
                    indexer: availability.indexer,
                    binary_path: binary_path.clone(),
                    args: availability.indexer.command_args(project_root, output_root),
                    working_directory: project_root.to_path_buf(),
                    output_path,
                }
            })
        })
        .collect()
}

pub async fn run_indexers(
    project_root: impl AsRef<Path>,
    output_root: impl AsRef<Path>,
) -> Result<IndexingRun> {
    let project_root = project_root.as_ref().to_path_buf();
    let output_root = output_root.as_ref().to_path_buf();
    fs::create_dir_all(&output_root)
        .with_context(|| format!("create SCIP output dir {}", output_root.display()))?;

    let available = detect_indexers();
    let plans = plan_indexer_commands(&project_root, &output_root, &available);
    let mut commands = Vec::with_capacity(plans.len());

    for plan in plans {
        let output = process::output(plan.build_command())
            .await
            .with_context(|| format!("run {}", plan.indexer.binary_name()))?;

        if !output.status.success() {
            return Err(anyhow!(
                "SCIP indexer {} failed with status {:?}: {}",
                plan.indexer.binary_name(),
                output.status.code(),
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        commands.push(ExecutedIndexerCommand {
            plan,
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    let artifacts = collect_scip_artifacts(&output_root, &commands)?;

    Ok(IndexingRun {
        project_root,
        output_root,
        commands,
        artifacts,
    })
}

pub fn collect_scip_artifacts(
    output_root: impl AsRef<Path>,
    commands: &[ExecutedIndexerCommand],
) -> Result<Vec<ScipArtifact>> {
    let output_root = output_root.as_ref();
    let mut seen = HashSet::new();
    let mut artifacts = Vec::new();

    let expected_paths: Vec<(PathBuf, SupportedIndexer)> = commands
        .iter()
        .map(|command| (command.plan.output_path.clone(), command.plan.indexer))
        .collect();

    for path in discover_scip_files(output_root)? {
        if seen.insert(path.clone()) {
            let indexer = expected_paths
                .iter()
                .find_map(|(expected, indexer)| (expected == &path).then_some(*indexer));
            artifacts.push(ScipArtifact { path, indexer });
        }
    }

    artifacts.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(artifacts)
}

fn discover_scip_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut artifacts = Vec::new();
    visit_dirs(root, &mut |path| {
        if path
            .extension()
            .and_then(OsStr::to_str)
            .is_some_and(|ext| ext == SCIP_ARTIFACT_EXTENSION)
        {
            artifacts.push(path.to_path_buf());
        }
        Ok(())
    })?;
    Ok(artifacts)
}

fn visit_dirs(root: &Path, visitor: &mut dyn FnMut(&Path) -> Result<()>) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }

    let metadata =
        fs::metadata(root).with_context(|| format!("read metadata for {}", root.display()))?;
    if metadata.is_file() {
        visitor(root)?;
        return Ok(());
    }

    for entry in fs::read_dir(root).with_context(|| format!("read dir {}", root.display()))? {
        let entry = entry.with_context(|| format!("read dir entry under {}", root.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("read file type for {}", path.display()))?;
        if file_type.is_dir() {
            visit_dirs(&path, visitor)?;
        } else if file_type.is_file() {
            visitor(&path)?;
        }
    }

    Ok(())
}

fn which_in_path(binary: &str, path_var: &str) -> Option<PathBuf> {
    for dir in env::split_paths(path_var) {
        let candidate = dir.join(binary);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }

        let nested_candidate = dir.join("bin").join(binary);
        if is_executable_file(&nested_candidate) {
            return Some(nested_candidate);
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => is_executable(metadata),
        _ => false,
    }
}

#[cfg(unix)]
fn is_executable(metadata: fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;

    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: fs::Metadata) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tempdir_in_tmp() -> TempDir {
        tempfile::Builder::new()
            .prefix("djinn-repo-map-")
            .tempdir_in(std::env::temp_dir())
            .expect("create test tempdir")
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("set permissions");
    }

    #[test]
    fn detect_indexers_reports_supported_binaries() {
        let tmp = tempdir_in_tmp();
        for indexer in SupportedIndexer::ALL {
            let path = tmp.path().join(indexer.binary_name());
            fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write binary");
            #[cfg(unix)]
            make_executable(&path);
        }

        let previous_path = env::var_os("PATH");
        env::set_var("PATH", tmp.path());
        let detections = detect_indexers();
        if let Some(previous_path) = previous_path {
            env::set_var("PATH", previous_path);
        } else {
            env::remove_var("PATH");
        }

        assert_eq!(detections.len(), SupportedIndexer::ALL.len());
        for detection in detections {
            assert!(detection.is_available(), "{detection:?}");
            assert_eq!(detection.path, Some(tmp.path().join(detection.binary)));
        }
    }

    #[test]
    fn plan_indexer_commands_only_includes_available_indexers() {
        let project_root = PathBuf::from("/tmp/example-project");
        let output_root = PathBuf::from("/tmp/example-project/.djinn/scip");
        let available = vec![
            IndexerAvailability {
                indexer: SupportedIndexer::RustAnalyzer,
                binary: "rust-analyzer".to_string(),
                path: Some(PathBuf::from("/tooling/rust-analyzer")),
            },
            IndexerAvailability {
                indexer: SupportedIndexer::Python,
                binary: "scip-python".to_string(),
                path: None,
            },
            IndexerAvailability {
                indexer: SupportedIndexer::TypeScript,
                binary: "scip-typescript".to_string(),
                path: Some(PathBuf::from("/tooling/scip-typescript")),
            },
        ];

        let plans = plan_indexer_commands(&project_root, &output_root, &available);

        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].indexer, SupportedIndexer::RustAnalyzer);
        assert_eq!(
            plans[0].args,
            vec![
                "scip",
                "/tmp/example-project/.djinn/scip/example-project-rust.scip"
            ]
        );
        assert_eq!(plans[1].indexer, SupportedIndexer::TypeScript);
        assert_eq!(
            plans[1].args,
            vec![
                "index",
                "/tmp/example-project/.djinn/scip/example-project-typescript.scip"
            ]
        );
    }

    #[test]
    fn collect_scip_artifacts_finds_nested_files_and_tags_known_outputs() {
        let tmp = tempdir_in_tmp();
        let output_root = tmp.path().join("out");
        fs::create_dir_all(output_root.join("nested")).expect("create output dirs");

        let planned = PlannedIndexerCommand {
            indexer: SupportedIndexer::Go,
            binary_path: PathBuf::from("/tooling/scip-go"),
            args: vec![
                "index".to_string(),
                output_root.join("example-go.scip").display().to_string(),
            ],
            working_directory: PathBuf::from("/tmp/project"),
            output_path: output_root.join("example-go.scip"),
        };
        fs::write(&planned.output_path, b"go-index").expect("write planned output");
        let nested = output_root.join("nested").join("manual.scip");
        fs::write(&nested, b"nested").expect("write nested output");

        let artifacts = collect_scip_artifacts(
            &output_root,
            &[ExecutedIndexerCommand {
                plan: planned,
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            }],
        )
        .expect("collect artifacts");

        assert_eq!(artifacts.len(), 2);
        assert_eq!(artifacts[0].indexer, Some(SupportedIndexer::Go));
        assert_eq!(artifacts[1].indexer, None);
    }

    #[test]
    fn command_planning_covers_all_supported_indexers() {
        let project_root = PathBuf::from("/workspace/repo");
        let output_root = PathBuf::from("/workspace/repo/.djinn/scip");

        let available: Vec<_> = SupportedIndexer::ALL
            .into_iter()
            .enumerate()
            .map(|(idx, indexer)| IndexerAvailability {
                indexer,
                binary: indexer.binary_name().to_string(),
                path: Some(PathBuf::from(format!(
                    "/tool/{idx}/{}",
                    indexer.binary_name()
                ))),
            })
            .collect();

        let plans = plan_indexer_commands(&project_root, &output_root, &available);
        assert_eq!(plans.len(), SupportedIndexer::ALL.len());
        assert_eq!(
            plans.iter().map(|plan| plan.indexer).collect::<Vec<_>>(),
            SupportedIndexer::ALL
        );
        assert_eq!(
            plans[0].args,
            vec!["scip", "/workspace/repo/.djinn/scip/repo-rust.scip"]
        );
        assert_eq!(
            plans[1].args,
            vec!["index", "/workspace/repo/.djinn/scip/repo-typescript.scip"]
        );
        assert_eq!(
            plans[2].args,
            vec!["index", "/workspace/repo/.djinn/scip/repo-python.scip"]
        );
        assert_eq!(
            plans[3].args,
            vec!["index", "/workspace/repo/.djinn/scip/repo-go.scip"]
        );
        assert_eq!(
            plans[4].args,
            vec!["index", "/workspace/repo/.djinn/scip/repo-java.scip"]
        );
    }

    #[test]
    fn collect_scip_artifacts_ignores_missing_root() {
        let missing = PathBuf::from("/tmp/does-not-exist-djinn-scip");
        let artifacts = collect_scip_artifacts(&missing, &[]).expect("collect artifacts");
        assert!(artifacts.is_empty());
    }

    #[test]
    fn io_result_type_is_used() {
        let _ = io::Result::Ok(());
    }
}
