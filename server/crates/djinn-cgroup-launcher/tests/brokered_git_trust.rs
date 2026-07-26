//! The coverage whose absence hid goxi's sixth launcher blocker: a brokered
//! child could not run `git` at all.
//!
//! # What went wrong, and why nothing caught it
//!
//! A task workspace is created by the worker (uid 1000). A brokered command runs
//! as `CHILD_UID` (1001). git compares the repository directory's **owner uid**
//! to the process uid — group ownership and mode do not enter into it — so the
//! very first `git` command an armed pod ran was going to die:
//!
//! ```text
//! # setpriv --reuid=1001 --regid=1000 --clear-groups git -C /workspace/wt status
//! fatal: detected dubious ownership in repository at '/workspace/wt'
//! ```
//!
//! `djinn-k8s`'s `job.rs` renders `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_0`/
//! `GIT_CONFIG_VALUE_0` = `safe.directory=*` onto the pod *specifically to fix
//! this*, and the broker's environment allow-list stripped all three. The
//! defect was two contracts disagreeing, and each side's tests were green.
//!
//! Five blockers preceded this one and **every one was invisible to a rendered
//! manifest assertion**. So this file does not assert that a key appears in an
//! env map. It builds a real repository, takes the environment out of the real
//! `Launcher::create_command`, and runs real `git`.
//!
//! # Making the ownership check fire without being root
//!
//! git ships `GIT_TEST_ASSUME_DIFFERENT_OWNER`, the hook its own suite uses to
//! force `ensure_valid_ownership()` down the failing branch. It reaches the same
//! code — and the same `safe.directory` lookup in protected scope — that a real
//! uid mismatch does, so the always-on proofs below run everywhere and fail
//! rather than skip. The genuinely cross-uid version, with a real chowned
//! worktree and a real `setresuid` to 1001, is the `#[ignore]`d half that the
//! privileged `launcher-kernel-boundary` lane executes; the always-on
//! [`the_privileged_git_proofs_are_wired_and_cannot_silently_skip`] guard makes
//! a lane that runs zero of them a red build.

use std::collections::BTreeSet;
use std::io;
use std::os::fd::RawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use djinn_cgroup_launcher::child::{ARTIFACT_GID, CHILD_UID};
use djinn_cgroup_launcher::{
    CgroupFs, CgroupMode, ChildProcess, CommandSpec, Error, Invocation, Launcher, LauncherConfig,
    Readiness, SpawnIntoCgroup, git_trust, is_allowed_environment_entry,
    is_allowed_environment_key,
};

/// The workflow job that must execute the `#[ignore]`d proofs below.
const PRIVILEGED_LANE_JOB: &str = "launcher-kernel-boundary";
/// Marker the lane uses to declare how many of them it expects to execute.
const EXPECTED_PROOFS_KEY: &str = "GIT_TRUST_EXPECTED_PROOFS";

/// git's own hook for exercising the dubious-ownership branch as any uid.
const ASSUME_DIFFERENT_OWNER: &str = "GIT_TEST_ASSUME_DIFFERENT_OWNER";

/// The exact production failure, by name.
const PRODUCTION_FAILURE: &str = "detected dubious ownership";

// ───────────────────────── the real composition seam ─────────────────────────

/// Minimal cgroup seam: the environment under test must come out of the real
/// `Launcher::create_command`, not be hand-assembled by the test.
struct FakeFs {
    next: RawFd,
}

impl CgroupFs for FakeFs {
    fn readiness(&self) -> Result<Readiness, Error> {
        Ok(Readiness {
            mode: CgroupMode::V2,
            root_writable: true,
            owner_uid: unsafe { libc::geteuid() },
            delegated_controllers: BTreeSet::from(["cpu".to_owned()]),
        })
    }
    fn create_direct_child(&mut self, _: &str) -> Result<RawFd, Error> {
        self.next += 1;
        Ok(self.next)
    }
    fn write_leaf(&mut self, _: RawFd, _: &str, _: &str) -> Result<(), Error> {
        Ok(())
    }
    fn read_leaf(&mut self, _: RawFd, _: &str) -> Result<String, Error> {
        Ok("populated 0\n".to_owned())
    }
    fn remove_leaf(&mut self, _: RawFd, _: &str) -> Result<(), Error> {
        Ok(())
    }
}

/// Captures the environment the launcher decided to exec the child with.
#[derive(Default)]
struct CaptureSpawn {
    environments: Vec<Vec<(String, String)>>,
}

impl SpawnIntoCgroup for CaptureSpawn {
    fn spawn_into_cgroup(
        &mut self,
        _: RawFd,
        _: &Invocation,
        command: &CommandSpec,
    ) -> Result<ChildProcess, Error> {
        command.validate()?;
        self.environments.push(command.environment.clone());
        Ok(ChildProcess {
            pid: 4242,
            stdout: -1,
            stderr: -1,
        })
    }
}

/// The environment a brokered child is really born with, for a command whose
/// `cwd` is `workspace`.
///
/// `caller` is what the worker relayed — the same shape
/// `process_broker::child_environment` produces, already filtered through the
/// broker's own key predicate.
fn brokered_environment(workspace: &Path, caller: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut launcher = Launcher::new(
        FakeFs { next: 10 },
        CaptureSpawn::default(),
        LauncherConfig::new(Some(250), Some(4_000), unsafe { libc::geteuid() })
            .expect("launcher config"),
    )
    .expect("launcher");
    let spec = CommandSpec {
        program: "/usr/bin/git".to_owned(),
        argv: vec!["status".to_owned()],
        // The rendered `cwd` is always under the workspace; the harness's real
        // repository stands in for it.
        cwd: "/workspace".to_owned(),
        environment: caller
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect(),
    };
    launcher
        .create_command("git-trust", invocation(), &spec)
        .expect("the launcher must accept a conforming spec");
    let (_, spawn) = launcher.into_parts();
    let mut environment = spawn.environments.into_iter().next().expect("one spawn");
    // The harness runs the child here rather than in a pod, so the one path the
    // spec names has to point at the real fixture.
    environment.push(("PWD".to_owned(), workspace.display().to_string()));
    environment
}

fn invocation() -> Invocation {
    Invocation {
        id: "git-trust".to_owned(),
        fence: 0x9017,
    }
}

/// What `job.rs` renders onto a task-run pod today, filtered exactly as
/// `process_broker::child_environment` filters it. Everything `GIT_*` falls out
/// here — that IS the blocker.
fn pod_environment(home: &Path) -> Vec<(String, String)> {
    [
        ("PATH", "/usr/local/bin:/usr/bin:/bin"),
        ("HOME", &home.display().to_string()),
        ("GIT_CONFIG_COUNT", "1"),
        ("GIT_CONFIG_KEY_0", "safe.directory"),
        ("GIT_CONFIG_VALUE_0", "*"),
    ]
    .into_iter()
    .filter(|(key, _)| is_allowed_environment_key(key))
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect()
}

// ───────────────────────────── real git harness ──────────────────────────────

struct Fixture {
    root: PathBuf,
    repo: PathBuf,
    home: PathBuf,
    /// Stand-in for the launcher container's `/etc/gitconfig`, which was
    /// measured on the production node to **not exist**. See
    /// [`Fixture::baseline`].
    empty_system: PathBuf,
    /// Private root for anchors this fixture materializes.
    anchor_root: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "djinn-brokered-git-{}-{name}-{}",
            std::process::id(),
            unsafe { libc::geteuid() }
        ));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("wt");
        let home = root.join("home");
        let anchor_root = root.join("anchor-root");
        std::fs::create_dir_all(&repo).expect("create worktree");
        std::fs::create_dir_all(&home).expect("create home");
        std::fs::create_dir_all(&anchor_root).expect("create anchor root");
        let empty_system = root.join("etc-gitconfig");
        std::fs::write(&empty_system, "").expect("write the empty system baseline");
        std::fs::set_permissions(&empty_system, std::fs::Permissions::from_mode(0o644))
            .expect("chmod baseline");
        // A real repository, not a fixture directory: the ownership check runs
        // during repository discovery and nowhere else.
        let init = Command::new("git")
            .args(["init", "--quiet", "."])
            .current_dir(&repo)
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("HOME", &home)
            .output()
            .expect("git must be installed to prove the git contract");
        assert!(init.status.success(), "git init failed: {init:?}");
        Self {
            root,
            repo,
            home,
            empty_system,
            anchor_root,
        }
    }

    /// Run `git <args>` in the worktree with exactly `environment`, plus the
    /// hook that makes the ownership check fire for the current uid.
    fn git(&self, environment: &[(String, String)], args: &[&str]) -> std::process::Output {
        let mut command = Command::new("git");
        command
            .args(args)
            .current_dir(&self.repo)
            .env_clear()
            .env(ASSUME_DIFFERENT_OWNER, "1");
        for (key, value) in environment {
            command.env(key, value);
        }
        command.output().expect("run git")
    }

    /// The system-scope baseline every behavioural arm below starts from.
    ///
    /// **This is not the fix being disabled — it is the HOST being excluded.**
    /// A GitHub Actions runner ships `/etc/gitconfig` containing
    /// `[safe] directory = *`, which trusts every repository for every process
    /// on the machine. Inherit it and the non-vacuity control passes with exit
    /// 0 and empty stderr while the blocker is fully present, and the positive
    /// control succeeds for a reason the launcher had no part in — which is
    /// exactly what happened on this change's first CI run.
    ///
    /// An empty file is the faithful model: measured inside a rendered launcher
    /// container on the production node, `/etc/gitconfig` **does not exist**, so
    /// the only system-scope configuration a brokered child can ever see is the
    /// one the launcher hands it.
    fn baseline(&self, home_only: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut environment: Vec<(String, String)> = home_only
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        environment.push((
            git_trust::GIT_TRUST_ANCHOR_KEY.to_owned(),
            self.empty_system.display().to_string(),
        ));
        environment
    }

    /// An anchor written by the production writer, chained to this fixture's
    /// controlled baseline instead of the host's `/etc/gitconfig`.
    fn hermetic_anchor(&self) -> PathBuf {
        git_trust::materialize_in_with_system(&self.anchor_root, Some(&self.empty_system))
            .expect("materialize a hermetic anchor")
    }

    /// [`Self::baseline`] with the anchor swapped in for the empty file — the
    /// single-variable difference the behavioural arms turn on.
    fn anchored(&self, home_only: &[(&str, &str)]) -> Vec<(String, String)> {
        let anchor = self.hermetic_anchor();
        let mut environment = self.baseline(home_only);
        for entry in &mut environment {
            if entry.0 == git_trust::GIT_TRUST_ANCHOR_KEY {
                entry.1 = anchor.display().to_string();
            }
        }
        environment
    }

    fn environment(&self, caller: &[(&str, &str)]) -> Vec<(String, String)> {
        brokered_environment(&self.repo, caller)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

// ═════════ always-on: the real failure, and the real fix, with real git ══════

/// NON-VACUITY. With the fix reverted, real git reproduces the production
/// failure BY NAME.
///
/// "Reverted" is not a mock: `pod_environment` is what `job.rs` renders put
/// through the broker's own key predicate, and the first assertion is that the
/// predicate drops every `GIT_` variable — that IS the blocker. The system-scope
/// baseline is [`Fixture::baseline`]'s empty file, modelling the launcher
/// container's measured absence of `/etc/gitconfig`; without it a CI runner's
/// machine-wide `[safe] directory = *` masks the failure entirely.
#[test]
fn without_the_launchers_anchor_real_git_reproduces_the_production_failure() {
    let fixture = Fixture::new("reverted");
    let pod = pod_environment(&fixture.home);
    assert!(
        !pod.iter().any(|(key, _)| key.starts_with("GIT_")),
        "the blocker itself: the broker's key predicate drops every GIT_ variable \
         the pod renders, so a brokered child gets no trust rule at all"
    );

    let mut reverted = fixture.baseline(&[("HOME", &fixture.home.display().to_string())]);
    reverted.extend(pod);
    let output = fixture.git(&reverted, &["status", "--porcelain"]);
    assert!(
        !output.status.success() && stderr(&output).contains(PRODUCTION_FAILURE),
        "the reverted environment must reproduce `{PRODUCTION_FAILURE}`; got \
         status={:?} stderr={:?}",
        output.status,
        stderr(&output)
    );
}

/// THE FIX. The anchor the production writer produces lets real git operate on
/// a worktree the process does not own — the single-variable flip against the
/// non-vacuity control above.
#[test]
fn the_launchers_anchor_lets_real_git_work_in_a_foreign_worktree() {
    let fixture = Fixture::new("fixed");
    let environment = fixture.anchored(&[
        ("PATH", "/usr/local/bin:/usr/bin:/bin"),
        ("HOME", &fixture.home.display().to_string()),
    ]);

    let output = fixture.git(&environment, &["status", "--porcelain"]);
    assert!(
        output.status.success(),
        "the anchor must let git run: status={:?} stderr={:?}",
        output.status,
        stderr(&output)
    );

    // And it is OUR rule doing it, in protected scope — the baseline it chains
    // to is empty, so nothing here can come from the host.
    let scopes = fixture.git(&environment, &["config", "--list", "--show-scope"]);
    let scopes = String::from_utf8_lossy(&scopes.stdout).into_owned();
    assert!(
        scopes
            .lines()
            .any(|line| line == "system\tsafe.directory=*"),
        "safe.directory must arrive in SYSTEM scope — the only scope git honours \
         it in, and the only one that survives into `git clone --local`'s inner \
         `git-upload-pack` child (nurw). Scopes seen: {scopes}"
    );
}

/// COMPOSITION. The environment the real `Launcher::create_command` produces
/// names that anchor, and carries no other git configuration at all.
///
/// Separated from the behavioural arm above on purpose: this is the half that
/// must speak about the *production* anchor path, and it needs no git run.
#[test]
fn the_brokered_child_environment_names_the_launchers_own_anchor() {
    let fixture = Fixture::new("composition");
    let environment = fixture.environment(&[
        ("PATH", "/usr/local/bin:/usr/bin:/bin"),
        ("HOME", &fixture.home.display().to_string()),
    ]);
    let anchor = git_trust::anchor_path()
        .expect("anchor")
        .display()
        .to_string();
    let git_entries: Vec<&(String, String)> = environment
        .iter()
        .filter(|(key, _)| key.starts_with("GIT"))
        .collect();
    assert_eq!(
        git_entries,
        vec![&(git_trust::GIT_TRUST_ANCHOR_KEY.to_string(), anchor)],
        "exactly one git variable may reach a brokered child, and it must be the \
         launcher's own anchor"
    );
}

/// git ignores a `GIT_CONFIG_SYSTEM` that points at a missing file **silently**.
/// A temp reaper would therefore bring the blocker back days into a pod's life
/// with nothing in the logs, which is why the launcher re-materializes.
#[test]
fn a_reaped_anchor_fails_silently_and_is_re_materialized() {
    let fixture = Fixture::new("reaper");
    // A private anchor root: the process-global one is shared with every other
    // test in this binary, and this proof deletes the file out from under it.
    let environment = fixture.anchored(&[("HOME", &fixture.home.display().to_string())]);
    let anchor = fixture.hermetic_anchor();
    assert!(
        fixture.git(&environment, &["status"]).status.success(),
        "control: the anchor works before it is reaped"
    );

    std::fs::remove_file(&anchor).expect("simulate a temp-directory reaper");
    let reaped = fixture.git(&environment, &["status", "--porcelain"]);
    assert!(
        !reaped.status.success() && stderr(&reaped).contains(PRODUCTION_FAILURE),
        "git ignores a GIT_CONFIG_SYSTEM pointing at a missing file SILENTLY, so the \
         blocker must come back with nothing in git's own output about the anchor: {:?}",
        stderr(&reaped)
    );

    // Which is why every call re-establishes it, with no operator action.
    let healed = fixture.hermetic_anchor();
    assert_eq!(healed, anchor);
    let after = fixture.git(&environment, &["status", "--porcelain"]);
    assert!(
        after.status.success(),
        "the launcher must heal its own anchor: {:?}",
        stderr(&after)
    );
}

/// `GIT_CONFIG_NOSYSTEM` makes git ignore the anchor outright. Its absence from
/// the allow-list is load-bearing, not tidiness — proven by making git do it.
#[test]
fn nosystem_defeats_the_anchor_which_is_why_it_is_not_forwardable() {
    let fixture = Fixture::new("nosystem");
    let mut environment = fixture.anchored(&[("HOME", &fixture.home.display().to_string())]);
    assert!(fixture.git(&environment, &["status"]).status.success());

    environment.push(("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()));
    let defeated = fixture.git(&environment, &["status", "--porcelain"]);
    assert!(
        !defeated.status.success() && stderr(&defeated).contains(PRODUCTION_FAILURE),
        "GIT_CONFIG_NOSYSTEM must be shown to defeat the anchor, or the allow-list \
         exclusion below is unmotivated: {:?}",
        stderr(&defeated)
    );
    assert!(
        !is_allowed_environment_key("GIT_CONFIG_NOSYSTEM")
            && !is_allowed_environment_entry("GIT_CONFIG_NOSYSTEM", "1"),
        "and therefore it must never be forwardable"
    );
}

// ══════════════════ always-on: the security half, with real git ══════════════

/// SECURITY. `core.sshCommand` and `core.pager` are arbitrary command
/// execution. Neither can reach a brokered child through any git mechanism:
/// not the env form, not a caller-chosen config file, and not the anchor.
#[test]
fn a_command_execution_config_key_cannot_reach_a_brokered_child() {
    let fixture = Fixture::new("injection");
    let marker = fixture.root.join("pwned");
    // What an attacker would want the child's git to load.
    let hostile = fixture.root.join("hostile-gitconfig");
    std::fs::write(
        &hostile,
        format!(
            "[safe]\n\tdirectory = *\n[core]\n\tsshCommand = /bin/touch {0}\n\tpager = /bin/touch {0}\n",
            marker.display()
        ),
    )
    .expect("write the hostile config");

    // 1. The env form is refused outright, before any leaf or child exists.
    for (key, value) in [
        ("GIT_CONFIG_COUNT", "1"),
        ("GIT_CONFIG_KEY_0", "core.sshCommand"),
        ("GIT_CONFIG_VALUE_0", "/bin/touch /tmp/pwned"),
        ("GIT_CONFIG_KEY_0", "core.pager"),
    ] {
        assert!(
            !is_allowed_environment_entry(key, value),
            "{key}={value} must not be forwardable"
        );
    }

    // 2. Naming a file is refused too — that is the whole reason the anchor key
    //    is admitted by VALUE and not by name.
    assert!(
        !is_allowed_environment_entry(
            git_trust::GIT_TRUST_ANCHOR_KEY,
            &hostile.display().to_string()
        ),
        "a caller must not be able to aim the child's SYSTEM git config at a file \
         it wrote; that is arbitrary command execution"
    );

    // 3. Behaviourally: under the anchor the launcher really writes, the hostile
    //    file is inert even though it exists and is readable.
    let environment = fixture.anchored(&[
        ("PATH", "/usr/local/bin:/usr/bin:/bin"),
        ("HOME", &fixture.home.display().to_string()),
    ]);
    for key in ["core.sshCommand", "core.pager"] {
        let resolved = fixture.git(&environment, &["config", "--get", key]);
        assert!(
            String::from_utf8_lossy(&resolved.stdout).trim().is_empty(),
            "{key} must be unset for a brokered child, got {:?}",
            String::from_utf8_lossy(&resolved.stdout)
        );
    }
    assert!(!marker.exists(), "the hostile config must never have run");

    // 4. And a caller cannot smuggle it in through the value the launcher pins:
    //    a spec naming the hostile file is rejected, not silently corrected.
    let rejected = brokered_spec_error(&[(
        git_trust::GIT_TRUST_ANCHOR_KEY,
        &hostile.display().to_string(),
    )]);
    assert!(
        matches!(rejected, Error::InvalidCommand),
        "expected InvalidCommand, got {rejected:?}"
    );
}

/// The launcher's own anchor grants `safe.directory` and nothing else — proven
/// by asking real git what the whole SYSTEM scope resolves to.
#[test]
fn the_anchor_grants_safe_directory_and_no_other_protected_setting() {
    let fixture = Fixture::new("scope");
    let environment = fixture.anchored(&[("HOME", &fixture.home.display().to_string())]);
    let listed = fixture.git(&environment, &["config", "--list", "--show-scope"]);
    let system: Vec<String> = String::from_utf8_lossy(&listed.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("system\t").map(str::to_owned))
        .collect();
    // The chained baseline is empty, so every SYSTEM entry here is the
    // launcher's own contribution. `include.path` is the chain directive itself
    // — the mechanism by which the real /etc/gitconfig is preserved — not a
    // setting the anchor introduces; everything else must be the trust rule.
    let settings: Vec<&String> = system
        .iter()
        .filter(|entry| !entry.starts_with("include.path="))
        .collect();
    assert_eq!(
        settings,
        vec![&"safe.directory=*".to_owned()],
        "the anchor must add the trust rule and NOTHING else to protected scope; \
         saw {system:?}"
    );
}

/// `GIT_CONFIG_SYSTEM` REPLACES `/etc/gitconfig` rather than adding to it, so
/// the generated file has to chain. Proven with real git against a real chained
/// file rather than by reading the generated text back.
#[test]
fn the_anchor_chains_the_real_system_config_instead_of_shadowing_it() {
    let fixture = Fixture::new("chain");
    let real_system = fixture.root.join("etc-gitconfig");
    std::fs::write(&real_system, "[core]\n\tabbrev = 12\n")
        .expect("write a stand-in system config");
    let chained = fixture.root.join("chained-gitconfig");
    std::fs::write(
        &chained,
        git_trust::anchor_contents(std::slice::from_ref(&real_system)),
    )
    .expect("write the chained anchor");
    std::fs::set_permissions(&chained, std::fs::Permissions::from_mode(0o644)).expect("chmod");

    let environment = vec![
        ("HOME".to_owned(), fixture.home.display().to_string()),
        (
            git_trust::GIT_TRUST_ANCHOR_KEY.to_owned(),
            chained.display().to_string(),
        ),
    ];
    let output = fixture.git(
        &environment,
        &["config", "--show-scope", "--get", "core.abbrev"],
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "system\t12",
        "the chained system config must still be read; a bare anchor would have \
         silently dropped everything the image put in /etc/gitconfig"
    );
    assert!(
        fixture.git(&environment, &["status"]).status.success(),
        "and the trust rule must still apply through the chain"
    );
}

fn brokered_spec_error(caller: &[(&str, &str)]) -> Error {
    let mut launcher = Launcher::new(
        FakeFs { next: 10 },
        CaptureSpawn::default(),
        LauncherConfig::new(Some(250), Some(4_000), unsafe { libc::geteuid() })
            .expect("launcher config"),
    )
    .expect("launcher");
    let spec = CommandSpec {
        program: "/usr/bin/git".to_owned(),
        argv: vec!["status".to_owned()],
        cwd: "/workspace".to_owned(),
        environment: caller
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect(),
    };
    launcher
        .create_command("hostile", invocation(), &spec)
        .expect_err("a hostile spec must be refused")
}

// ═══════════════ always-on: the privileged lane cannot silently skip ═════════

#[test]
fn the_privileged_git_proofs_are_wired_and_cannot_silently_skip() {
    let source = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/brokered_git_trust.rs"),
    )
    .expect("read this test file to count its privileged proofs");
    let declared = source.matches("\n#[ignore").count();
    assert!(declared > 0, "an empty privileged suite proves nothing");

    let workflow =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.github/workflows/quality-gate.yml");
    let workflow = std::fs::read_to_string(&workflow)
        .unwrap_or_else(|error| panic!("read {}: {error}", workflow.display()));
    let lane = workflow
        .split(&format!("\n  {PRIVILEGED_LANE_JOB}:"))
        .nth(1)
        .unwrap_or_else(|| panic!("the {PRIVILEGED_LANE_JOB} lane must exist"));

    assert!(
        lane.contains("--test brokered_git_trust"),
        "the privileged lane must build and run THIS test binary"
    );
    assert!(
        lane.contains(&format!("{EXPECTED_PROOFS_KEY}: \"{declared}\"")),
        "the lane must declare `{EXPECTED_PROOFS_KEY}: \"{declared}\"` so a run that \
         executes fewer than the {declared} declared proofs fails instead of passing"
    );
    assert!(
        !lane.contains("continue-on-error"),
        "a security proof lane may not swallow its own failure"
    );
}

// ═════════════ privileged: the genuinely cross-uid reproduction ══════════════

/// The real thing: a worktree owned by the WORKER uid, and `git status` run by a
/// real process that has `setresuid`'d to [`CHILD_UID`]. No test hook.
#[ignore = "privileged: needs uid 0 to create a foreign-owned worktree and drop to \
            CHILD_UID (CI job launcher-kernel-boundary)"]
#[test]
fn a_real_child_uid_runs_git_in_a_worker_owned_worktree_only_with_the_anchor() {
    require_root();
    let fixture = Fixture::new("cross-uid");

    // The anchor is written before the tree is handed to the worker uid, and it
    // lives outside it, exactly as the launcher's does.
    let anchored = fixture.anchored(&[
        ("PATH", "/usr/local/bin:/usr/bin:/bin"),
        ("HOME", &fixture.home.display().to_string()),
    ]);
    let mut reverted = fixture.baseline(&[("HOME", &fixture.home.display().to_string())]);
    reverted.extend(pod_environment(&fixture.home));

    chown_tree(&fixture.root, WORKER_UID, ARTIFACT_GID);
    set_mode_tree(&fixture.root, 0o2775);
    chown_tree(&fixture.home, CHILD_UID, ARTIFACT_GID);

    let failed = run_git_as_child(&fixture, &reverted);
    assert!(
        failed.1.contains(PRODUCTION_FAILURE),
        "NON-VACUITY: a real uid-{CHILD_UID} child in a uid-{WORKER_UID}-owned worktree \
         must reproduce `{PRODUCTION_FAILURE}`; got exit={} stderr={:?}",
        failed.0,
        failed.1
    );

    let succeeded = run_git_as_child(&fixture, &anchored);
    assert_eq!(
        succeeded.0, 0,
        "the launcher's anchor must let a real uid-{CHILD_UID} child run git in a \
         uid-{WORKER_UID}-owned worktree; stderr={:?}",
        succeeded.1
    );
}

/// The anchor is only half of it: the child also has to be able to WRITE the
/// worktree, and it can only do that through the artifact group. That holds
/// because the worker pins `umask 0002` at startup — at the container default
/// `022` the same directory renders `2755` and the child is locked out. This
/// repo shipped a umask-022 regression in the warm path recently, so the
/// dependency is measured rather than assumed.
#[ignore = "privileged: needs uid 0 to create a foreign-owned worktree and drop to \
            CHILD_UID (CI job launcher-kernel-boundary)"]
#[test]
fn the_child_can_write_the_worktree_only_under_the_artifact_umask() {
    require_root();
    let fixture = Fixture::new("umask");
    for (umask, expected_mode, writable) in [(0o002, 0o2775, true), (0o022, 0o2755, false)] {
        let directory = fixture.root.join(format!("wt-{umask:04o}"));
        // SAFETY: `umask` cannot fail; restored immediately.
        let previous = unsafe { libc::umask(umask) };
        std::fs::create_dir(&directory).expect("create worktree");
        unsafe { libc::umask(previous) };
        // setgid is the volume contract's half; the umask supplies the mode.
        let mode = std::fs::symlink_metadata(&directory)
            .expect("stat")
            .permissions()
            .mode()
            | 0o2000;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(mode))
            .expect("apply setgid");
        chown(&directory, WORKER_UID, ARTIFACT_GID);
        assert_eq!(
            std::fs::symlink_metadata(&directory)
                .expect("stat")
                .permissions()
                .mode()
                & 0o7777,
            expected_mode,
            "umask {umask:04o} must render {expected_mode:04o}"
        );

        let created = create_as_child(&directory.join("artifact"));
        assert_eq!(
            created == 0,
            writable,
            "under umask {umask:04o} the uid-{CHILD_UID} child writing a \
             uid-{WORKER_UID}-owned worktree must {}; got exit {created}",
            if writable { "succeed" } else { "be denied" }
        );
    }
}

// ───────────────────────── privileged-only helpers ───────────────────────────

/// The worker's rendered uid. Duplicated from `broker::WORKER_UID` only to keep
/// the privileged assertions readable.
const WORKER_UID: u32 = djinn_cgroup_launcher::broker::WORKER_UID;

fn require_root() {
    assert_eq!(
        unsafe { libc::geteuid() },
        0,
        "the cross-uid git proofs need uid 0 to create a foreign-owned worktree and \
         drop to uid {CHILD_UID}. They run in the `{PRIVILEGED_LANE_JOB}` CI lane; \
         reproduce locally with `docker run --rm -v \"$PWD:$PWD\" ubuntu:24.04` after \
         installing git."
    );
}

fn chown(path: &Path, uid: u32, gid: u32) {
    let raw = std::ffi::CString::new(path.to_string_lossy().as_bytes()).expect("path");
    assert_eq!(
        unsafe { libc::chown(raw.as_ptr(), uid, gid) },
        0,
        "chown {} to {uid}:{gid}: {}",
        path.display(),
        io::Error::last_os_error()
    );
}

fn chown_tree(root: &Path, uid: u32, gid: u32) {
    chown(root, uid, gid);
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir()) {
                chown_tree(&path, uid, gid);
            } else {
                chown(&path, uid, gid);
            }
        }
    }
}

fn set_mode_tree(root: &Path, mode: u32) {
    let _ = std::fs::set_permissions(root, std::fs::Permissions::from_mode(mode));
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir()) {
                set_mode_tree(&path, mode);
            }
        }
    }
}

/// Fork, take the child's real credentials (`setgroups(0)` + `setresgid` +
/// `setresuid`, the same order `child::prepare_child` uses), and run `git
/// status` with `environment`. Returns `(exit code, stderr)`.
fn run_git_as_child(fixture: &Fixture, environment: &[(String, String)]) -> (i32, String) {
    let mut command = Command::new("git");
    command
        .args(["status", "--porcelain"])
        .current_dir(&fixture.repo)
        .env_clear();
    for (key, value) in environment {
        command.env(key, value);
    }
    // No `GIT_TEST_ASSUME_DIFFERENT_OWNER` here: the uid mismatch is real.
    unsafe {
        command.pre_exec(|| {
            libc::umask(0o002);
            if libc::setgroups(0, std::ptr::null()) != 0
                || libc::setresgid(ARTIFACT_GID, ARTIFACT_GID, ARTIFACT_GID) != 0
                || libc::setresuid(CHILD_UID, CHILD_UID, CHILD_UID) != 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = command.output().expect("run git as the child uid");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Create `path` as the real child credentials. Returns the child's exit code
/// (0 = created, 1 = denied).
fn create_as_child(path: &Path) -> i32 {
    let raw = std::ffi::CString::new(path.to_string_lossy().as_bytes()).expect("path");
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork: {}", io::Error::last_os_error());
    if pid == 0 {
        unsafe {
            libc::umask(0o002);
            if libc::setgroups(0, std::ptr::null()) != 0
                || libc::setresgid(ARTIFACT_GID, ARTIFACT_GID, ARTIFACT_GID) != 0
                || libc::setresuid(CHILD_UID, CHILD_UID, CHILD_UID) != 0
            {
                libc::_exit(2);
            }
            let fd = libc::open(raw.as_ptr(), libc::O_CREAT | libc::O_WRONLY, 0o666);
            libc::_exit(if fd < 0 { 1 } else { 0 });
        }
    }
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        -1
    }
}
