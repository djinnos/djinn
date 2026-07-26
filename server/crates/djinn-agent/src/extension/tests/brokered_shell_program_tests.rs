//! The coverage whose absence hid goxi's seventh launcher blocker: a brokered
//! `run_shell` could not start a process at all.
//!
//! # What went wrong, and why nothing caught it
//!
//! `extension::handlers::workspace` builds `Command::new("bash").arg("-lc")` —
//! a **bare** program name, which is what [`std::process::Command`] documents a
//! `PATH` search for. [`CommandSpec`] is not a `Command`; it is an `execve(2)`
//! request, and `execve` performs no search. `CommandSpec::validate` therefore
//! refused it outright:
//!
//! ```text
//! program = "bash"        -> Err(InvalidCommand)
//! program = "/bin/bash"   -> Ok(())
//! ```
//!
//! Every brokered shell command failed `lease invocation failed:
//! InvalidCommand`, and running a shell command is the first thing an armed
//! task pod does. Even past validation the child would have `_exit`ed 127:
//! `spawn.rs` hands `program` straight to `execve`.
//!
//! # Why the existing broker-backed tests were green
//!
//! `shell_dispatch_tests::BrokerShellLauncher` — the adapter behind every
//! broker-backed dispatch test — "deliberately ignores `Command`", by its own
//! doc comment. It never converts, so it never validates, so the one contract
//! that mattered was untested from the side that has to satisfy it: the caller.
//!
//! So nothing below hand-assembles a [`CommandSpec`]. Every proof starts at the
//! real [`call_shell`], and the spec under test comes out of the real
//! [`crate::process::command_spec`] — the production conversion — applied to
//! the real `Command` the handler built.
//!
//! # Non-vacuity
//!
//! [`ProofLauncher`] records, for the same `Command`, what the caller *named*
//! and what the conversion *resolved*, and every proof asserts on both. The
//! control is not a hypothetical: it is the caller's own program put through
//! the same `CommandSpec::validate`, and it must come back
//! `Err(Error::InvalidCommand)` **by name**. If a future change makes the
//! handler name an absolute path itself, that control turns green and
//! [`the_program_the_shell_handler_names_is_one_the_broker_refuses`] fails,
//! loudly, saying so — which is the point. This chain of blockers is made of
//! two sides of a contract drifting apart while each side's tests stayed green.
//!
//! # The one thing the harness supplies that a CI runner cannot
//!
//! `CommandSpec::validate` requires `cwd` to be `/workspace` or beneath it, and
//! a CI runner has no writable `/workspace`. So [`ProofLauncher`] re-points the
//! command's `current_dir` to a production-shaped `/workspace/...` **before**
//! the real conversion — so validation runs against the real constraint — and
//! then executes the validated spec against the equivalent real directory the
//! test created. `program`, `argv` and `environment` are used verbatim; only
//! `cwd` is rebased, and [`the_proof_substitutes_the_cwd_and_nothing_else`]
//! pins that. This is the same materialized-root device
//! `djinn-k8s`'s `launcher_child_filesystem_reachability` uses for the same
//! reason.

use super::handlers::call_shell;
use super::{agent_context_from_db, create_test_db};
use crate::process::{CgroupLauncherClient, LeaseInvocationRunner, ProcessHandle, command_spec};
use djinn_cgroup_launcher::{CommandSpec, CpuStat};
use djinn_core::clock::SystemClock;
use djinn_supervisor::services::{LeaseFencingToken, TaskInvocationLeaseIdentity};
use std::io;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// A production-shaped `cwd`. `CommandSpec::validate` accepts nothing else, and
/// a task-run Pod always renders one (`WORKSPACE_MOUNT_DIR` + the worktree).
const RENDERED_CWD: &str = "/workspace/goxi-brokered-shell-proof";

/// The exact production failure, by name. Compared as the `Debug` spelling of
/// the variant rather than by `PartialEq`, which `djinn_cgroup_launcher::Error`
/// deliberately does not implement (it carries an `io::Error`).
const PRODUCTION_FAILURE: &str = "InvalidCommand";

/// `CommandSpec::validate`'s verdict, reduced to the variant name.
fn verdict(spec: &CommandSpec) -> Result<(), String> {
    spec.validate().map_err(|error| format!("{error:?}"))
}

// ───────────────────────── the real composition seam ─────────────────────────

/// What the real conversion did to the real handler's `Command`.
#[derive(Clone, Debug)]
struct Conversion {
    /// The program the CALLER named, verbatim from `Command::get_program`.
    named: String,
    /// That same program's verdict from the broker's own validator — the
    /// control, derived from the caller rather than written down here.
    named_verdict: Result<(), String>,
    /// The spec the production conversion actually produced.
    spec: CommandSpec,
    /// `cwd` as the handler set it, before the harness rebased it.
    handler_cwd: String,
}

/// A launcher client that runs the REAL conversion and then REALLY executes.
///
/// This is the opposite of `shell_dispatch_tests::BrokerShellLauncher`: that one
/// exists to exercise the lease lifecycle and ignores the `Command`; this one
/// exists to exercise the `Command` and keeps the lifecycle trivial.
#[derive(Clone)]
struct ProofLauncher {
    /// Real directory standing in for [`RENDERED_CWD`].
    workspace: Arc<std::path::PathBuf>,
    conversions: Arc<Mutex<Vec<Conversion>>>,
}

impl ProofLauncher {
    fn new(workspace: &Path) -> Self {
        Self {
            workspace: Arc::new(workspace.to_path_buf()),
            conversions: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn conversion(&self) -> Conversion {
        let recorded = self.conversions.lock().expect("conversions");
        assert_eq!(
            recorded.len(),
            1,
            "the handler must reach the broker exactly once"
        );
        recorded[0].clone()
    }
}

impl CgroupLauncherClient for ProofLauncher {
    fn launch(
        &self,
        mut command: Command,
        _: &TaskInvocationLeaseIdentity,
    ) -> io::Result<Box<dyn ProcessHandle>> {
        let named = command
            .get_program()
            .to_str()
            .expect("program is UTF-8")
            .to_owned();
        let handler_cwd = command
            .get_current_dir()
            .expect("the handler always sets a cwd")
            .to_string_lossy()
            .into_owned();

        // Give the conversion the cwd production gives it. Everything else —
        // program, argv, environment, including whatever the sandbox layered
        // on — is exactly what the handler built.
        command.current_dir(RENDERED_CWD);

        // THE PRODUCTION CONVERSION. Not a copy of it, not a reimplementation.
        let spec = command_spec(command)?;

        // The control: the same validator, on the program the caller named,
        // in an otherwise identical spec.
        let named_verdict = verdict(&CommandSpec {
            program: named.clone(),
            ..spec.clone()
        });

        self.conversions
            .lock()
            .expect("conversions")
            .push(Conversion {
                named,
                named_verdict,
                spec: spec.clone(),
                handler_cwd,
            });

        Ok(Box::new(execute(&spec, &self.workspace)?))
    }
}

/// Run a validated spec the way the launcher runs it.
///
/// `spawn.rs` calls `execve(spec.program, argv, envp)` after `chdir(spec.cwd)`,
/// with the child's environment replaced wholesale. This reproduces all four:
/// `Command::new` performs a `PATH` search **only** for a program with no `/`
/// in it, and `spec.program` is asserted absolute here, so no search can occur
/// and the exec is `execve`-equivalent. `env_clear` gives the child exactly the
/// spec's environment and nothing inherited, which is the launcher's contract.
fn execute(spec: &CommandSpec, workspace: &Path) -> io::Result<ProofChild> {
    assert!(
        Path::new(&spec.program).is_absolute(),
        "a validated spec must carry an absolute program, or this exec would PATH-search \
         and stop modelling `execve`: {:?}",
        spec.program
    );
    let output = Command::new(&spec.program)
        .args(&spec.argv)
        .env_clear()
        .envs(spec.environment.iter().cloned())
        // The one substitution: the validated `/workspace/...` rebased onto the
        // real directory this test created. See the module docs.
        .current_dir(workspace)
        .stdin(Stdio::null())
        .output()?;
    Ok(ProofChild {
        stdout: output.stdout,
        stderr: output.stderr,
        status: output.status,
    })
}

struct ProofChild {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    status: ExitStatus,
}

impl ProcessHandle for ProofChild {
    fn drain_stdout(&mut self) -> io::Result<Vec<u8>> {
        Ok(std::mem::take(&mut self.stdout))
    }
    fn drain_stderr(&mut self) -> io::Result<Vec<u8>> {
        Ok(std::mem::take(&mut self.stderr))
    }
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        Ok(Some(self.status))
    }
    fn wait(&mut self) -> io::Result<ExitStatus> {
        Ok(self.status)
    }
    fn sample_cpu(&mut self) -> io::Result<CpuStat> {
        Ok(CpuStat::default())
    }
    fn fenced_lift(&mut self, _: &LeaseFencingToken) -> io::Result<()> {
        Ok(())
    }
    fn kill(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn wait_empty(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn cleanup(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn shell_args(command: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    Some(
        serde_json::json!({ "command": command })
            .as_object()
            .expect("shell arguments object")
            .clone(),
    )
}

/// Drive the real `call_shell` over the real broker conversion.
async fn brokered_shell(command: &str) -> (serde_json::Value, Conversion) {
    let worktree = crate::test_helpers::test_tempdir("goxi-brokered-shell-");
    let mut state = agent_context_from_db(create_test_db(), CancellationToken::new());
    let launcher = ProofLauncher::new(worktree.path());
    let runner = Arc::new(LeaseInvocationRunner::new(
        Arc::new(djinn_supervisor::services::rpc::UnimplementedRpcServices::new()),
        Arc::new(launcher.clone()),
        Arc::new(SystemClock::new()),
    ));
    state.shell_launch = Some(crate::context::ShellLaunchContext::for_test(
        runner,
        "goxi-task".into(),
        "goxi-task-run".into(),
        "goxi-pod-uid".into(),
    ));

    let response = call_shell(
        &state,
        &shell_args(command),
        worktree.path(),
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect("a brokered shell command must return its terminal output");

    (response, launcher.conversion())
}

// ─────────────────────────────── the proofs ──────────────────────────────────

/// The blocker itself: the program the handler names is one the broker refuses.
///
/// This is the control, and it is derived from the caller rather than asserted
/// about a literal. If it ever passes, the handler has started naming an
/// absolute path and every other proof here has quietly stopped covering the
/// resolution — delete the resolution or keep a bare-name caller, but do not
/// leave this file believing it proves something it no longer does.
#[tokio::test]
async fn the_program_the_shell_handler_names_is_one_the_broker_refuses() {
    let (_, conversion) = brokered_shell("true").await;

    assert!(
        !conversion.named.contains('/'),
        "the shell handler is expected to name a bare program (it builds \
         `Command::new(\"bash\")`); it named {:?}",
        conversion.named
    );
    assert_eq!(
        conversion.named_verdict,
        Err(PRODUCTION_FAILURE.to_owned()),
        "the production failure is `InvalidCommand` on the program the handler names; \
         without that this file's other proofs are vacuous"
    );
}

/// The fix, end to end: a brokered `run_shell` actually runs a process and its
/// output comes back through the tool response.
#[tokio::test]
async fn a_brokered_shell_command_resolves_its_program_and_really_executes() {
    let (response, conversion) = brokered_shell("printf 'goxi-%s\\n' seventh-blocker").await;

    assert_eq!(
        response["ok"],
        serde_json::json!(true),
        "stderr: {}",
        response["stderr"]
    );
    assert_eq!(response["exit_code"], serde_json::json!(0));
    // `contains`, not equality: the handler builds a LOGIN shell (`bash -lc`),
    // which sources `/etc/profile` and `/etc/profile.d/*` — production included.
    // The marker can only appear if a process really ran the agent's command.
    let stdout = response["stdout"].as_str().expect("stdout is a string");
    assert!(
        stdout.contains("goxi-seventh-blocker"),
        "the brokered child's own output must reach the tool response, got {stdout:?}"
    );

    // The resolution is what made that possible.
    let program = &conversion.spec.program;
    assert!(
        Path::new(program).is_absolute(),
        "{program:?} must be absolute: `execve` performs no PATH search"
    );
    assert!(
        program.ends_with(&format!("/{}", conversion.named)),
        "{program:?} must be the caller's own program {:?} resolved, not a substitute",
        conversion.named
    );
    conversion
        .spec
        .validate()
        .expect("the resolved spec must satisfy the broker's own validator");
}

/// The child really is born with the environment the spec carries — the shell
/// that runs is the one the launcher would have `execve`d, in the environment
/// the launcher would have handed it, not the worker's.
///
/// The negative half is asserted on the SPEC rather than on the child's own
/// view, deliberately: `env_clear` in [`execute`] is the harness's code, so a
/// child that could not see a stray variable would prove the harness works.
/// What has to hold in production is that the forwarded set is closed, and
/// `CommandSpec::validate` is the thing that enforces it.
#[tokio::test]
async fn the_executed_child_sees_the_forwarded_environment() {
    let (response, conversion) = brokered_shell("echo \"home=$HOME\"").await;

    let home = conversion
        .spec
        .environment
        .iter()
        .find(|(key, _)| key == "HOME")
        .map(|(_, value)| value.clone())
        .expect("HOME must survive the broker hop");
    let stdout = response["stdout"].as_str().expect("stdout is a string");
    assert!(
        stdout.contains(&format!("home={home}")),
        "the child must see the spec's HOME, got {stdout:?}"
    );

    for (key, _) in &conversion.spec.environment {
        assert!(
            djinn_cgroup_launcher::is_allowed_environment_key(key)
                || key == djinn_cgroup_launcher::env::GIT_TRUST_ANCHOR_KEY,
            "{key} reached a brokered child without being on the closed allow-list"
        );
    }
}

/// The harness substitutes exactly one field, and it is the one a CI runner
/// cannot provide. Everything the blocker is about is used verbatim.
#[tokio::test]
async fn the_proof_substitutes_the_cwd_and_nothing_else() {
    let (_, conversion) = brokered_shell("true").await;

    assert_eq!(
        conversion.spec.cwd, RENDERED_CWD,
        "the conversion must be validated against a production-shaped cwd"
    );
    assert_ne!(
        conversion.handler_cwd, RENDERED_CWD,
        "the handler's own cwd is the test worktree; if these are equal the \
         substitution is not happening and this guard proves nothing"
    );
    assert!(
        CommandSpec {
            cwd: conversion.handler_cwd.clone(),
            ..conversion.spec.clone()
        }
        .validate()
        .is_err(),
        "the substitution is necessary: {:?} is not a cwd the broker accepts",
        conversion.handler_cwd
    );
}

// ────────────────────── the conversion contract, directly ────────────────────

/// `-lc` and the command text must reach the child unchanged. A resolution that
/// rewrote argv would silently change what the agent asked to run.
#[tokio::test]
async fn resolution_changes_the_program_and_leaves_argv_alone() {
    let (_, conversion) = brokered_shell("echo untouched").await;

    assert_eq!(
        conversion.spec.argv,
        vec!["-lc".to_owned(), "echo untouched".to_owned()],
        "only the program is resolved"
    );
}

/// `Command` resolves a bare name against the CHILD's `PATH` when one is set.
/// The conversion must too, or a caller that pins `PATH` would get a different
/// binary through the broker than it would get unbrokered.
///
/// The planted program is necessarily outside the broker's program allow-list
/// (a test cannot write `/bin`), so the resolution is read off the refusal —
/// which names the resolved path precisely because the coarse `InvalidCommand`
/// on its own is what made this blocker expensive to find.
#[test]
fn resolution_uses_the_childs_path_not_the_workers() {
    let planted = crate::test_helpers::test_tempdir("goxi-path-");
    let program = planted.path().join("goxi-only-here");
    std::fs::write(&program, "#!/bin/sh\nexit 0\n").expect("plant program");
    make_executable(&program);

    let mut command = Command::new("goxi-only-here");
    command.current_dir(RENDERED_CWD);
    // `PATH` is on the broker's forward allow-list, so an explicit one wins
    // over the worker's inherited value — exactly as `Command::get_envs` does.
    command.env("PATH", planted.path());

    let message = command_spec(command)
        .expect_err("a planted program is outside the program allow-list")
        .to_string();
    assert!(
        message.contains(&format!("{}", program.display())),
        "the bare name must have been resolved against the CHILD PATH: {message}"
    );
    assert!(
        message.contains(PRODUCTION_FAILURE),
        "and then refused by the allow-list, not by the resolution: {message}"
    );
}

/// A program on no `PATH` entry fails here, named, instead of becoming an
/// `execve` `ENOENT` and a child that `_exit`s 127 with no explanation.
#[test]
fn a_program_that_is_on_no_path_entry_is_refused_by_name() {
    let empty = crate::test_helpers::test_tempdir("goxi-empty-path-");
    let mut command = Command::new("goxi-no-such-program");
    command.current_dir(RENDERED_CWD);
    command.env("PATH", empty.path());

    let error = command_spec(command).expect_err("an unresolvable program must not be sent");
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    let message = error.to_string();
    assert!(
        message.contains("goxi-no-such-program") && message.contains("PATH"),
        "the refusal must name the program and the path it searched: {message}"
    );
}

/// A caller that already names an absolute path keeps it byte for byte — the
/// resolution is a repair for the `Command` contract, not a rewrite policy.
#[test]
fn an_absolute_program_is_passed_through_unchanged() {
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg("true").current_dir(RENDERED_CWD);
    let spec = command_spec(command).expect("an absolute program needs no resolution");
    assert_eq!(spec.program, "/bin/sh");
}

/// A **relative** path is not silently made absolute. `execvp` treats anything
/// containing a `/` as a path, and resolving it against the worker's cwd would
/// invent an authority the caller never asked for; the broker's own validator
/// refuses it a moment later.
#[test]
fn a_relative_program_is_not_silently_promoted() {
    let mut command = Command::new("./bash");
    command.current_dir(RENDERED_CWD);
    let error = command_spec(command).expect_err("a relative program must not be accepted");
    let message = error.to_string();
    assert!(
        message.contains(PRODUCTION_FAILURE),
        "a relative program must be refused as the production failure: {message}"
    );
    assert!(
        message.contains("./bash"),
        "the refusal must name what was rejected: {message}"
    );
}

// ─────────────────────── the allow-list decision, pinned ─────────────────────

/// The program allow-list is left exactly as narrow as it was, and this records
/// the decision so widening it has to be deliberate.
///
/// It is **not** a code-provenance control and must not be sold as one:
/// `/workspace/` is on it and the agent writes `/workspace`, and — decisively —
/// the only program a brokered command ever names is a shell, which then
/// resolves the agent's actual command against a `PATH` that is mostly outside
/// the list. Measured in the rendered task image on the production node,
/// `cargo` is `/usr/local/cargo/bin/cargo` and `node` is `/opt/node/bin/node`,
/// so a brokered `cargo build` already runs a binary from a directory the list
/// does not name.
///
/// Widening it to the two directories the seventh blocker's investigation found
/// it rejects would therefore buy no safety and serve no caller: measured in the
/// same image, `/usr/local/bin` is empty and `/opt/djinn/bin` holds only
/// `djinn-agent-worker` and `djinn-cgroup-launcher`, neither of which is ever a
/// brokered child. The hazard a narrow list really carries — a caller tripping
/// it and failing closed — is fixed in the conversion instead.
#[test]
fn the_program_allow_list_stays_narrow_and_admits_the_shell_the_handler_uses() {
    let spec = |program: &str| CommandSpec {
        program: program.to_owned(),
        argv: vec!["-lc".to_owned(), "true".to_owned()],
        cwd: RENDERED_CWD.to_owned(),
        environment: vec![("PATH".to_owned(), "/usr/bin:/bin".to_owned())],
    };

    // Both spellings of the shell exist in the rendered image (`/bin` is a
    // symlink to `usr/bin`), so resolution may legitimately produce either.
    for admitted in ["/bin/bash", "/usr/bin/bash", "/workspace/tool"] {
        spec(admitted)
            .validate()
            .unwrap_or_else(|error| panic!("{admitted} must be admitted: {error}"));
    }
    for refused in [
        "bash",
        "./bash",
        "/usr/bin/../etc/passwd",
        "/usr//bin/bash",
        // Deliberately still refused; see the doc comment above.
        "/usr/local/bin/anything",
        "/opt/djinn/bin/djinn-agent-worker",
    ] {
        assert!(
            verdict(&spec(refused)) == Err(PRODUCTION_FAILURE.to_owned()),
            "{refused} must stay outside the program allow-list"
        );
    }
}

fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path)
        .expect("plant metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).expect("plant mode");
}
