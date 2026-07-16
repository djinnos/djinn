//! Bounded deterministic scenario execution and per-scenario evidence output.
//!
//! Process execution and database acquisition are injected so scheduling can be
//! tested without wall-clock sleeps or a live database.

use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use wait_timeout::ChildExt;

use crate::{Execution, Profile, Scenario, ScenarioInventory, Taxonomy, scenario::resolves};

pub const RUNNER_NAME: &str = "djinn-qa";
pub const RUNNER_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerArtifactIdentity {
    pub name: String,
    pub version: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
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
const CARGO_EXECUTION_TIMEOUT: Duration = Duration::from_secs(30 * 60);

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
        execute_command(&mut command, CARGO_EXECUTION_TIMEOUT, package)
    }
}

fn drain<T: Read + Send + 'static>(stream: Option<T>) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut stream) = stream {
            let _ = stream.read_to_end(&mut bytes);
        }
        bytes
    })
}

fn join_drain(handle: thread::JoinHandle<Vec<u8>>) -> Vec<u8> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let _ = sender.send(handle.join().unwrap_or_default());
    });
    receiver
        .recv_timeout(Duration::from_secs(2))
        .unwrap_or_default()
}

#[cfg(unix)]
fn isolate_process_group(command: &mut Command) {
    // SAFETY: this callback only invokes the async-signal-safe setpgid syscall
    // between fork and exec. The separate group includes cargo's descendants.
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        });
    }
}

#[cfg(not(unix))]
fn isolate_process_group(_: &mut Command) {}

#[cfg(unix)]
fn terminate_descendants(child: &mut std::process::Child) {
    let process_group = -(child.id() as i32);
    // SAFETY: the child was placed in a process group whose id is its pid.
    unsafe {
        libc::kill(process_group, libc::SIGTERM);
    }
    thread::sleep(Duration::from_millis(200));
    // Always escalate the group. Cargo may exit after SIGTERM while a test
    // descendant ignores it; checking only the direct child would orphan that
    // descendant and keep its output pipes open.
    // SAFETY: as above; SIGKILL makes termination non-cooperative.
    unsafe {
        libc::kill(process_group, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn terminate_descendants(child: &mut std::process::Child) {
    let _ = child.kill();
}

fn execute_command(command: &mut Command, timeout: Duration, package: &str) -> Result<(), String> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    isolate_process_group(command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("cargo adapter could not start `{package}`: {error}"))?;
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());

    let status = match child.wait_timeout(timeout) {
        Ok(Some(status)) => status,
        Ok(None) => {
            terminate_descendants(&mut child);
            // Reaping is bounded too, so uninterruptible I/O cannot replace the
            // execution hang with a cleanup hang.
            let _ = child.wait_timeout(Duration::from_secs(3));
            let _ = join_drain(stdout);
            let stderr = join_drain(stderr);
            let detail = String::from_utf8_lossy(&stderr);
            return Err(format!(
                "cargo adapter timed out for package `{package}` after {} seconds{}{}",
                timeout.as_secs_f64(),
                if detail.trim().is_empty() { "" } else { ": " },
                detail.trim()
            ));
        }
        Err(error) => {
            terminate_descendants(&mut child);
            let _ = child.wait_timeout(Duration::from_secs(3));
            let _ = join_drain(stdout);
            let _ = join_drain(stderr);
            return Err(format!(
                "cargo adapter wait failed for package `{package}`: {error}"
            ));
        }
    };
    let _ = join_drain(stdout);
    let stderr = join_drain(stderr);
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "cargo adapter failed for package `{package}` (exit {status}): {}",
            String::from_utf8_lossy(&stderr).trim()
        ))
    }
}

pub struct TemplateCloneDatabase;
impl DatabaseAcquirer for TemplateCloneDatabase {
    fn acquire(&self) -> Result<Box<dyn Send>, String> {
        // The SQLx pool created by `Database::open_in_memory` spawns maintenance
        // tasks (`PoolInner::new` → `spawn_maintenance_tasks`) at construction
        // time. Those tasks must find a live Tokio handle via
        // `Handle::try_current`, otherwise sqlx panics with "this functionality
        // requires a Tokio context". Build the owning runtime first, enter its
        // context, then create and initialize the database inside it.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("dedicated test database runtime setup failed: {e}"))?;
        let database = {
            let _enter = runtime.enter();
            let database = djinn_db::Database::open_in_memory()
                .map_err(|e| format!("dedicated test database acquisition failed: {e}"))?;
            runtime
                .block_on(database.ensure_initialized())
                .map_err(|e| format!("dedicated test database acquisition failed: {e}"))?;
            database
        };
        Ok(Box::new(TemplateCloneDatabaseGuard { database, runtime }))
    }
}

/// Owns the dedicated Tokio runtime that backs a test-database SQLx pool so the
/// pool's maintenance tasks stay associated with a live runtime for the guard's
/// entire lifetime.
///
/// Field declaration order is load-bearing: `database` (and its `PgPool`) is
/// dropped **before** `runtime`, ensuring pool cleanup and `TestDbInit` teardown
/// happen while the owning runtime is still valid. The fields are never read —
/// they exist solely for their drop-time cleanup ordering.
#[allow(dead_code)]
struct TemplateCloneDatabaseGuard {
    database: djinn_db::Database,
    runtime: tokio::runtime::Runtime,
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

/// Runs distinct execution targets in bounded batches and fans each result out to
/// every declaring scenario, collecting outcomes in stable scenario-ID order.
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
    let mut planned = BTreeMap::<(&str, Option<&str>), Vec<&Scenario>>::new();
    for scenario in selected {
        let Execution::CargoPackage { package, test } = &scenario.execution;
        planned
            .entry((package.as_str(), test.as_deref()))
            .or_default()
            .push(scenario);
    }
    let planned = planned.into_values().collect::<Vec<_>>();
    let mut outcomes = Vec::with_capacity(selected.len());
    for batch in planned.chunks(concurrency) {
        let (sender, receiver) = mpsc::channel();
        thread::scope(|scope| {
            for scenarios in batch {
                let sender = sender.clone();
                scope.spawn(move || {
                    let scenario = scenarios[0];
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
                    let finished_at = now();
                    for scenario in scenarios {
                        let _ = sender.send(ScenarioOutcome {
                            scenario_id: scenario.id.clone(),
                            status,
                            diagnostics: diagnostics.clone(),
                            started_at: started_at.clone(),
                            finished_at: finished_at.clone(),
                        });
                    }
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
    fn with_package(mut scenario: Scenario, package: &str) -> Scenario {
        scenario.execution = Execution::CargoPackage {
            package: package.into(),
            test: None,
        };
        scenario
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
        assert!(
            json["diagnostics"][0]
                .as_str()
                .unwrap()
                .contains("cannot be resolved")
        );
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

    #[test]
    fn identical_targets_execute_once_and_fan_out_in_scenario_id_order() {
        let executor = Executor {
            calls: Mutex::new(0),
            peak: AtomicUsize::new(0),
        };
        let scenarios = ["h", "f", "d", "b", "g", "e", "c", "a"]
            .map(|id| with_package(scenario(id), "djinn-coordinator"));

        let outcomes = execute_selected(&repository_root(), &scenarios, 8, &executor, &Db).unwrap();

        assert_eq!(*executor.calls.lock().unwrap(), 1);
        assert_eq!(
            outcomes
                .iter()
                .map(|outcome| outcome.scenario_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b", "c", "d", "e", "f", "g", "h"]
        );
        assert!(outcomes.windows(2).all(|pair| {
            pair[0].started_at == pair[1].started_at
                && pair[0].finished_at == pair[1].finished_at
                && pair[0].diagnostics == pair[1].diagnostics
        }));
    }

    struct TrackingDatabase {
        acquisitions: AtomicUsize,
        active: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
    }
    struct TrackingGuard {
        active: Arc<AtomicUsize>,
    }
    impl Drop for TrackingGuard {
        fn drop(&mut self) {
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
    }
    impl DatabaseAcquirer for TrackingDatabase {
        fn acquire(&self) -> Result<Box<dyn Send>, String> {
            self.acquisitions.fetch_add(1, Ordering::SeqCst);
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            Ok(Box::new(TrackingGuard {
                active: Arc::clone(&self.active),
            }))
        }
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
        let database = TrackingDatabase {
            acquisitions: AtomicUsize::new(0),
            active: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
        };
        let scenarios = vec![
            with_package(scenario("d"), "djinn-slot"),
            with_package(scenario("b"), "djinn-db"),
            with_package(scenario("c"), "djinn-coordinator"),
            with_package(scenario("a"), "djinn-qa"),
        ];
        let outcomes = execute_selected(
            &repository_root(),
            &scenarios,
            2,
            executor.as_ref(),
            &database,
        )
        .unwrap();
        assert_eq!(outcomes.len(), 4);
        assert_eq!(executor.peak.load(Ordering::SeqCst), 2);
        assert_eq!(database.acquisitions.load(Ordering::SeqCst), 4);
        assert_eq!(database.peak.load(Ordering::SeqCst), 2);
        assert_eq!(database.active.load(Ordering::SeqCst), 0);
        assert_eq!(
            outcomes
                .iter()
                .map(|outcome| outcome.scenario_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b", "c", "d"]
        );
    }

    struct TimeoutExecutor;
    impl ScenarioExecutor for TimeoutExecutor {
        fn execute(&self, _: &Path, _: &Execution) -> Result<(), String> {
            let mut command = Command::new("sh");
            command.args(["-c", "sleep 10"]);
            execute_command(&mut command, Duration::from_millis(20), "djinn-qa")
        }
    }

    #[cfg(unix)]
    #[test]
    fn timeout_is_failed_scenario_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let evidence = temp.path().join("evidence");
        let inventory = ScenarioInventory {
            version: 1,
            scenarios: vec![scenario("adapter.timeout")],
        };
        let summary = run_inventory(
            &repository_root(),
            &taxonomy(),
            &inventory,
            Profile::SmokeCi,
            1,
            &evidence,
            "a",
            &TimeoutExecutor,
            &Db,
        )
        .unwrap();

        assert!(!summary.succeeded());
        assert!(summary.outcomes[0].diagnostics[0].contains("timed out"));
        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(evidence.join("adapter.timeout.json")).unwrap())
                .unwrap();
        assert_eq!(json["status"], "failed");
        assert!(
            json["diagnostics"][0]
                .as_str()
                .unwrap()
                .contains("timed out")
        );
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
