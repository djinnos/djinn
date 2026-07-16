//! Bounded deterministic scenario execution and per-scenario evidence output.
//!
//! Process execution and database acquisition are injected so scheduling can be
//! tested without wall-clock sleeps or a live database.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc,
    thread,
};

use serde::Serialize;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{Execution, Profile, Scenario, ScenarioInventory, Taxonomy, scenario::resolves};

pub const RUNNER_NAME: &str = "djinn-qa";
pub const RUNNER_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ScenarioEvidenceArtifact {
    pub scenario_id: String,
    pub scenario_version: u32,
    pub taxonomy_version: u32,
    pub requirement_id: String,
    pub covered_ids: Vec<String>,
    pub profile: Profile,
    pub status: RunStatus,
    pub git_sha: String,
    pub runner: RunnerArtifactIdentity,
    pub sources: Vec<crate::SourceIdentifier>,
    pub watch_paths: Vec<String>,
    pub started_at: String,
    pub finished_at: String,
    pub diagnostics: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RunnerArtifactIdentity {
    pub name: String,
    pub version: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunStatus {
    Passed,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunSummary {
    pub outcomes: Vec<ScenarioOutcome>,
}
impl RunSummary {
    pub fn succeeded(&self) -> bool {
        self.outcomes
            .iter()
            .all(|item| item.status == RunStatus::Passed)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScenarioOutcome {
    pub scenario_id: String,
    pub status: RunStatus,
    pub diagnostics: Vec<String>,
    pub started_at: String,
    pub finished_at: String,
}

/// Executes the deterministic adapter declared by the scenario.
pub trait ScenarioExecutor: Sync {
    fn execute(&self, root: &Path, execution: &Execution) -> Result<(), String>;
}
/// Holds a dedicated DB clone through execution. An acquisition error is a failure.
pub trait DatabaseAcquirer: Sync {
    fn acquire(&self) -> Result<Box<dyn Send>, String>;
}

pub struct CargoExecutor;
impl ScenarioExecutor for CargoExecutor {
    fn execute(&self, root: &Path, execution: &Execution) -> Result<(), String> {
        let Execution::CargoPackage { package, test } = execution;
        let workspace = if root.join("server/Cargo.toml").is_file() {
            root.join("server")
        } else {
            root.to_path_buf()
        };
        let mut command = Command::new("cargo");
        command
            .current_dir(workspace)
            .arg("test")
            .arg("-p")
            .arg(package);
        if let Some(test) = test {
            command.arg("--test").arg(test);
        }
        let output = command
            .output()
            .map_err(|e| format!("cargo adapter could not start `{package}`: {e}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "cargo adapter failed for package `{package}` (exit {}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }
}

pub struct TemplateCloneDatabase;
impl DatabaseAcquirer for TemplateCloneDatabase {
    fn acquire(&self) -> Result<Box<dyn Send>, String> {
        let database = djinn_db::Database::open_in_memory()
            .map_err(|e| format!("dedicated test database acquisition failed: {e}"))?;
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("dedicated test database runtime setup failed: {e}"))?
            .block_on(database.ensure_initialized())
            .map_err(|e| format!("dedicated test database acquisition failed: {e}"))?;
        Ok(Box::new(database))
    }
}

/// Inventory is already sorted by ID; blocked entries remain unproven and are not
/// represented by fabricated pass/skip evidence.
pub fn select_scenarios(inventory: &ScenarioInventory, profile: Profile) -> Vec<Scenario> {
    inventory
        .scenarios
        .iter()
        .filter(|s| s.enabled && s.profiles.contains(&profile) && s.blocked_dependency.is_none())
        .cloned()
        .collect()
}

/// Runs batches no larger than `concurrency`, collecting outcomes in stable ID order.
pub fn execute_selected(
    root: &Path,
    selected: &[Scenario],
    concurrency: usize,
    executor: &dyn ScenarioExecutor,
    databases: &dyn DatabaseAcquirer,
) -> Result<Vec<ScenarioOutcome>, String> {
    if concurrency == 0 {
        return Err("--concurrency must be a positive integer".into());
    }
    if selected.is_empty() {
        return Err(
            "no enabled, executable scenarios are eligible for the requested profile".into(),
        );
    }
    let mut outcomes = Vec::with_capacity(selected.len());
    for batch in selected.chunks(concurrency) {
        let (sender, receiver) = mpsc::channel();
        thread::scope(|scope| {
            for scenario in batch {
                let sender = sender.clone();
                scope.spawn(move || {
                    let started_at = now();
                    let (status, diagnostics) = if !resolves(&scenario.execution, root) {
                        (
                            RunStatus::Failed,
                            vec![format!(
                                "declared executable target cannot be resolved from repository root `{}`",
                                root.display()
                            )],
                        )
                    } else {
                        match databases.acquire() {
                            Ok(_db) => match executor.execute(root, &scenario.execution) {
                                Ok(()) => (
                                    RunStatus::Passed,
                                    vec!["declared deterministic adapter completed".into()],
                                ),
                                Err(error) => (RunStatus::Failed, vec![error]),
                            },
                            Err(error) => (RunStatus::Failed, vec![error]),
                        }
                    };
                    let _ = sender.send(ScenarioOutcome {
                        scenario_id: scenario.id.clone(),
                        status,
                        diagnostics,
                        started_at,
                        finished_at: now(),
                    });
                });
            }
        });
        drop(sender);
        outcomes.extend(receiver);
    }
    outcomes.sort_by(|a, b| a.scenario_id.cmp(&b.scenario_id));
    Ok(outcomes)
}

#[allow(clippy::too_many_arguments)]
pub fn run_inventory(
    root: &Path,
    taxonomy: &Taxonomy,
    inventory: &ScenarioInventory,
    profile: Profile,
    concurrency: usize,
    evidence_dir: &Path,
    git_sha: &str,
    executor: &dyn ScenarioExecutor,
    databases: &dyn DatabaseAcquirer,
) -> Result<RunSummary, String> {
    let selected = select_scenarios(inventory, profile);
    let outcomes = execute_selected(root, &selected, concurrency, executor, databases)?;
    for outcome in &outcomes {
        let scenario = selected
            .iter()
            .find(|s| s.id == outcome.scenario_id)
            .expect("outcome originated from selection");
        write_artifact(
            evidence_dir,
            scenario,
            &artifact(scenario, taxonomy, profile, git_sha, outcome),
        )?;
    }
    Ok(RunSummary { outcomes })
}

fn artifact(
    s: &Scenario,
    taxonomy: &Taxonomy,
    profile: Profile,
    git_sha: &str,
    outcome: &ScenarioOutcome,
) -> ScenarioEvidenceArtifact {
    let mut covered_ids = vec![s.primary_coverage.clone()];
    covered_ids.extend(s.secondary_coverage.clone());
    ScenarioEvidenceArtifact {
        scenario_id: s.id.clone(),
        scenario_version: s.version,
        taxonomy_version: taxonomy.version,
        requirement_id: s.primary_coverage.clone(),
        covered_ids,
        profile,
        status: outcome.status,
        git_sha: git_sha.into(),
        runner: RunnerArtifactIdentity {
            name: RUNNER_NAME.into(),
            version: RUNNER_VERSION.into(),
        },
        sources: s.sources.clone(),
        watch_paths: s.watch_paths.clone(),
        started_at: outcome.started_at.clone(),
        finished_at: outcome.finished_at.clone(),
        diagnostics: outcome.diagnostics.clone(),
    }
}
fn now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("RFC3339 formatting is supported")
}
fn artifact_path(dir: &Path, scenario: &Scenario) -> PathBuf {
    dir.join(format!("{}.json", scenario.id))
}
fn write_artifact(
    dir: &Path,
    scenario: &Scenario,
    artifact: &ScenarioEvidenceArtifact,
) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| {
        format!(
            "could not create evidence directory `{}`: {e}",
            dir.display()
        )
    })?;
    let path = artifact_path(dir, scenario);
    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    fs::write(
        &temp,
        serde_json::to_vec_pretty(artifact).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("could not write evidence `{}`: {e}", temp.display()))?;
    fs::rename(&temp, &path)
        .map_err(|e| format!("could not finalize evidence `{}`: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc, Barrier, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    fn scenario(id: &str) -> Scenario {
        Scenario {
            id: id.into(),
            version: 1,
            enabled: true,
            profiles: vec![Profile::SmokeCi],
            sources: vec![],
            primary_coverage: "task.state-machine.legal-transitions".into(),
            secondary_coverage: vec![],
            execution: Execution::CargoPackage {
                package: "djinn-qa".into(),
                test: None,
            },
            isolation: crate::Isolation {
                database: crate::IsolationMode::Isolated,
                providers: crate::IsolationMode::Isolated,
                channel: crate::IsolationMode::Isolated,
                live_credentials: false,
                live_providers: false,
                kubernetes: false,
                external_network: false,
                wall_clock_sleep: false,
            },
            watch_paths: vec!["src/lib.rs".into()],
            blocked_dependency: None,
        }
    }
    struct Executor {
        calls: Mutex<usize>,
        peak: AtomicUsize,
    }
    impl ScenarioExecutor for Executor {
        fn execute(&self, _: &Path, _: &Execution) -> Result<(), String> {
            let calls = self.calls.lock().unwrap();
            self.peak.fetch_max(*calls + 1, Ordering::SeqCst);
            drop(calls);
            *self.calls.lock().unwrap() += 1;
            Ok(())
        }
    }
    struct Db;
    impl DatabaseAcquirer for Db {
        fn acquire(&self) -> Result<Box<dyn Send>, String> {
            Ok(Box::new(()))
        }
    }
    fn taxonomy() -> Taxonomy {
        Taxonomy::from_yaml(include_str!("../tests/fixtures/valid-taxonomy.yaml")).unwrap()
    }
    fn repository_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .unwrap()
            .to_path_buf()
    }
    #[test]
    fn selection_excludes_blocked_and_disabled() {
        let mut blocked = scenario("a");
        blocked.blocked_dependency = Some("pending".into());
        let mut disabled = scenario("b");
        disabled.enabled = false;
        let inventory = ScenarioInventory {
            version: 1,
            scenarios: vec![blocked, disabled, scenario("z")],
        };
        assert_eq!(select_scenarios(&inventory, Profile::SmokeCi).len(), 1);
    }
    #[test]
    fn rejects_vacuous_and_zero_bound() {
        let e = Executor {
            calls: Mutex::new(0),
            peak: AtomicUsize::new(0),
        };
        assert!(execute_selected(Path::new("."), &[], 1, &e, &Db).is_err());
        assert!(execute_selected(Path::new("."), &[scenario("x")], 0, &e, &Db).is_err());
    }
    struct BadDb;
    impl DatabaseAcquirer for BadDb {
        fn acquire(&self) -> Result<Box<dyn Send>, String> {
            Err("dedicated test database acquisition failed".into())
        }
    }
    #[test]
    fn db_failure_is_not_a_skip() {
        let e = Executor {
            calls: Mutex::new(0),
            peak: AtomicUsize::new(0),
        };
        let result = execute_selected(&repository_root(), &[scenario("x")], 1, &e, &BadDb).unwrap();
        assert_eq!(result[0].status, RunStatus::Failed);
        assert_eq!(*e.calls.lock().unwrap(), 0);
    }

    #[test]
    fn unresolved_target_writes_failed_artifact_and_skips_adapter() {
        let temp = tempfile::tempdir().unwrap();
        let evidence = temp.path().join("evidence");
        let executor = Executor {
            calls: Mutex::new(0),
            peak: AtomicUsize::new(0),
        };
        let inventory = ScenarioInventory {
            version: 1,
            scenarios: vec![scenario("missing.target")],
        };

        let summary = run_inventory(
            temp.path(),
            &taxonomy(),
            &inventory,
            Profile::SmokeCi,
            1,
            &evidence,
            "a",
            &executor,
            &Db,
        )
        .unwrap();

        assert!(!summary.succeeded());
        assert_eq!(*executor.calls.lock().unwrap(), 0);
        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(evidence.join("missing.target.json")).unwrap())
                .unwrap();
        assert_eq!(json["status"], "failed");
        assert!(json["diagnostics"][0]
            .as_str()
            .unwrap()
            .contains("cannot be resolved"));
    }

    #[test]
    fn resolved_target_invokes_adapter_and_aggregate_passes() {
        let executor = Executor {
            calls: Mutex::new(0),
            peak: AtomicUsize::new(0),
        };
        let mut item = scenario("adapter.invocation");
        item.execution = Execution::CargoPackage {
            package: "djinn-qa".into(),
            test: None,
        };
        let result = execute_selected(&repository_root(), &[item], 1, &executor, &Db).unwrap();
        assert_eq!(result[0].status, RunStatus::Passed);
        assert_eq!(*executor.calls.lock().unwrap(), 1);
        let summary = RunSummary { outcomes: result };
        assert!(summary.succeeded());
    }

    struct SynchronizedExecutor {
        entered: AtomicUsize,
        peak: AtomicUsize,
        barrier: Barrier,
    }
    impl ScenarioExecutor for SynchronizedExecutor {
        fn execute(&self, _: &Path, _: &Execution) -> Result<(), String> {
            let now = self.entered.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            self.barrier.wait();
            self.entered.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn bounded_execution_uses_logical_synchronization_not_sleep() {
        let executor = Arc::new(SynchronizedExecutor {
            entered: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            barrier: Barrier::new(2),
        });
        let scenarios = vec![scenario("a"), scenario("b"), scenario("c"), scenario("d")];
        let outcomes =
            execute_selected(&repository_root(), &scenarios, 2, executor.as_ref(), &Db).unwrap();
        assert_eq!(outcomes.len(), 4);
        assert_eq!(executor.peak.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn artifact_has_stable_scenario_path_and_freshness_fields() {
        let temp = tempfile::tempdir().unwrap();
        let mut item = scenario("theme.scenario");
        item.sources = vec![crate::SourceIdentifier {
            kind: crate::SourceKind::Memory,
            id: "pitfalls/source".into(),
        }];
        let artifact = ScenarioEvidenceArtifact {
            scenario_id: item.id.clone(),
            scenario_version: 1,
            taxonomy_version: 1,
            requirement_id: item.primary_coverage.clone(),
            covered_ids: vec![item.primary_coverage.clone()],
            profile: Profile::SmokeCi,
            status: RunStatus::Passed,
            git_sha: "a".repeat(40),
            runner: RunnerArtifactIdentity {
                name: RUNNER_NAME.into(),
                version: RUNNER_VERSION.into(),
            },
            sources: item.sources.clone(),
            watch_paths: item.watch_paths.clone(),
            started_at: "2026-01-01T00:00:00Z".into(),
            finished_at: "2026-01-01T00:00:01Z".into(),
            diagnostics: vec!["declared deterministic adapter completed".into()],
        };
        write_artifact(temp.path(), &item, &artifact).unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(artifact_path(temp.path(), &item)).unwrap()).unwrap();
        assert_eq!(value["scenario_id"], "theme.scenario");
        assert_eq!(value["sources"][0]["id"], "pitfalls/source");
        assert_eq!(value["watch_paths"][0], "src/lib.rs");
        assert_eq!(value["status"], "passed");
    }
}
