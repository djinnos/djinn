// Linux Landlock sandbox backend.
//
// ADR-013: OS-Level Shell Sandboxing — Landlock + Seatbelt
// ADR-017: Shell Sandbox Implementation — Worktree Injection and Landlock Crate

#![cfg(target_os = "linux")]

use std::io;
use std::path::{Path, PathBuf};

use anyhow::Result;
use landlock::{
    ABI, Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
};

use crate::confidential;
use crate::{Sandbox, SandboxScope, djinn_cache_dir, git_dir, git_metadata_dir};

/// Landlock-based filesystem sandbox for Linux ≥ 5.13.
///
/// Restricts the agent child process to read-almost-everywhere, write only to
/// the task worktree, its git metadata directory, `/var/tmp`, a dedicated djinn
/// agent scratch dir (`$XDG_CACHE_HOME/djinn` or `$HOME/.cache/djinn`), the
/// shared cross-task cache PVC (`/cache`, where toolchain caches like
/// `GOMODCACHE`/`GOCACHE`/sccache live, task-run Cargo targets use private
/// `/cache/cargo-target-runs/<task_run_id>` dirs, and warm jobs maintain
/// `/cache/cargo-target/<project_id>` bases), and the usual `/dev/{null,zero,urandom}`
/// nodes. `/tmp` is intentionally not
/// writable: on typical Linux it's tmpfs, and allowing writes there caused a
/// 3.8 GB cargo-artifact leak into RAM-backed storage.
///
/// "Almost" everywhere: file CONTENT reads are withheld beneath
/// [`confidential::CONFIDENTIAL_ROOTS`] — the per-task-run Secret mount that
/// carries the org's provider credentials, and the projected ServiceAccount
/// token mount (task jqvg). Directory listing and execute stay granted on all
/// of `/`.
pub struct LandlockSandbox;

impl Sandbox for LandlockSandbox {
    fn apply(&self, scope: SandboxScope<'_>, cmd: &mut std::process::Command) -> Result<()> {
        self.apply_with_confidential_roots(
            scope,
            cmd,
            &confidential::present_confidential_roots(confidential::CONFIDENTIAL_ROOTS),
        )
    }
}

impl LandlockSandbox {
    /// [`Sandbox::apply`] with an explicit set of unreadable roots.
    ///
    /// Production always passes [`confidential::CONFIDENTIAL_ROOTS`]; tests
    /// pass a fixture tree so the denial can be exercised for real on a host
    /// where the Pod's `/var/run/djinn` mount does not exist.
    pub(crate) fn apply_with_confidential_roots(
        &self,
        scope: SandboxScope<'_>,
        cmd: &mut std::process::Command,
        confidential_roots: &[PathBuf],
    ) -> Result<()> {
        use std::os::unix::process::CommandExt;

        scope.validate()?;

        // Redirect temp to a Landlock-writable, disk-backed dir. The K8s task-run
        // Pod sets TMPDIR=/workspace (job.rs) so the host supervisor's TempDir
        // lands on the big writable `/workspace` emptyDir — but that's the PVC
        // mount ROOT, and the rules below grant the agent write access only to
        // its worktree SUBDIR (`/workspace/<project>`). So any sandboxed tool
        // that honors `$TMPDIR` — go's git codehost (`go-codehost-*`), cargo/cc
        // linker scratch, etc. — was creating temp files directly under
        // `/workspace` and hitting `permission denied`. Point sandboxed commands
        // at `/var/tmp`, which is already in the writable allowlist (the
        // `/var/tmp` rule below requires it to exist). `GOTMPDIR` is unset in the
        // image, so Go falls back to `$TMPDIR` — this covers it without a
        // Go-specific knob. The supervisor's own TempDir is unaffected: it is not
        // spawned through this sandbox, so it keeps using the `/workspace`
        // emptyDir for large mirror clones.
        //
        // This override also crosses the broker: `TMPDIR` is on
        // `djinn_cgroup_launcher::is_allowed_environment_key`'s forward list and
        // `process_broker::child_environment` overlays `Command::get_envs()`
        // onto the inherited set, so the value set HERE — not the pod's
        // `TMPDIR=/workspace` — is what a brokered child is born with. The
        // launcher container must therefore make it writable; see
        // [`crate::SANDBOX_TMPDIR`].
        cmd.env("TMPDIR", crate::SANDBOX_TMPDIR);

        let (writable_worktree, git_meta) = match scope {
            SandboxScope::Worktree(path) => (Some(path.to_path_buf()), git_metadata_dir(path)),
            SandboxScope::ReadSource { .. } => (None, None),
        };

        // Resolve + create the djinn cache dir in the PARENT process, before
        // fork. `create_dir_all` and `tracing::warn!` are not async-signal-safe,
        // so they must not run inside `pre_exec` — doing so risks deadlocking
        // a forked child if another thread in the tokio-based parent held a
        // malloc/tracing mutex at fork time. Only the Landlock ruleset
        // construction runs post-fork in pre_exec.
        let cache_dir_for_rule = prepare_cache_dir();

        // Same reasoning for the confidential-path cover: computing it walks
        // the filesystem with `read_dir`, which allocates. Resolve it here in
        // the parent so `pre_exec` only has to open the resulting paths.
        // Recomputed per spawn so a directory that appears mid-run is picked
        // up on the next command rather than being silently ungranted.
        let read_file_cover = confidential::read_file_cover(confidential_roots);
        let cargo_build_dir = std::env::var("CARGO_HOME")
            .or_else(|_| std::env::var("HOME").map(|home| format!("{home}/.cargo")))
            .ok()
            .map(|base| PathBuf::from(base).join("build"));
        let writable_roots = writable_sandbox_roots(
            writable_worktree.as_deref(),
            git_meta.as_deref(),
            cache_dir_for_rule.as_deref(),
            cargo_build_dir.as_deref(),
        );
        ensure_confidential_roots_do_not_overlap_writable_roots(
            confidential_roots,
            &writable_roots,
        )?;

        // Safety: pre_exec runs in the forked child process. The closure only
        // performs Landlock syscalls and open(2) calls, both of which are
        // async-signal-safe per POSIX.
        unsafe {
            cmd.pre_exec(move || {
                apply_policy(&read_file_cover, &writable_roots)
                    .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e.to_string()))
            });
        }
        Ok(())
    }
}

/// Resolve the djinn agent scratch directory and ensure it exists.
///
/// Runs in the parent process only. Returns `Some(path)` if the directory
/// exists (either already present or successfully created), `None` otherwise.
/// On creation failure, logs a warning and returns `None` so the sandbox
/// setup can continue without the cache-dir allowance.
fn prepare_cache_dir() -> Option<PathBuf> {
    let dir = djinn_cache_dir()?;
    match std::fs::create_dir_all(&dir) {
        Ok(()) => Some(dir),
        Err(e) => {
            tracing::warn!(
                path = %dir.display(),
                error = %e,
                "sandbox: failed to create djinn cache dir; skipping Landlock rule"
            );
            None
        }
    }
}

/// A `full_access` rule and whether failure to open it must abort sandbox
/// installation. This collection is shared with the confidential-root
/// invariant so a new writable rule cannot bypass it.
#[derive(Debug)]
struct WritableSandboxRoot {
    path: PathBuf,
    required: bool,
}

/// Enumerate every path that can receive a `full_access` Landlock rule.
fn writable_sandbox_roots(
    worktree: Option<&Path>,
    git_meta: Option<&Path>,
    cache_dir: Option<&Path>,
    cargo_build_dir: Option<&Path>,
) -> Vec<WritableSandboxRoot> {
    let mut roots = vec![
        WritableSandboxRoot {
            path: PathBuf::from(crate::SANDBOX_TMPDIR),
            required: true,
        },
        WritableSandboxRoot {
            path: PathBuf::from("/dev/null"),
            required: true,
        },
        WritableSandboxRoot {
            path: PathBuf::from("/dev/zero"),
            required: true,
        },
        WritableSandboxRoot {
            path: PathBuf::from("/dev/urandom"),
            required: true,
        },
    ];
    if let Some(worktree) = worktree {
        roots.push(WritableSandboxRoot {
            path: worktree.to_path_buf(),
            required: true,
        });
    }
    if let Some(dir) = cargo_build_dir.filter(|dir| dir.is_dir()) {
        roots.push(WritableSandboxRoot {
            path: dir.to_path_buf(),
            required: true,
        });
    }
    if let Some(dir) = cache_dir {
        roots.push(WritableSandboxRoot {
            path: dir.to_path_buf(),
            required: false,
        });
    }
    roots.push(WritableSandboxRoot {
        path: PathBuf::from("/cache"),
        required: false,
    });
    if let Some(dot_git) = worktree.and_then(git_dir).filter(|dir| dir.is_dir()) {
        roots.push(WritableSandboxRoot {
            path: dot_git,
            required: true,
        });
    } else if let Some(meta) = git_meta {
        roots.push(WritableSandboxRoot {
            path: meta.to_path_buf(),
            required: true,
        });
    }
    roots
}

/// Landlock rules are additive, so a confidential root cannot be carved out of
/// a writable ancestor that grants `ReadFile` through `full_access`.
fn ensure_confidential_roots_do_not_overlap_writable_roots(
    confidential_roots: &[PathBuf],
    writable_roots: &[WritableSandboxRoot],
) -> Result<()> {
    for confidential_root in confidential_roots {
        for writable_root in writable_roots {
            anyhow::ensure!(
                !confidential_root.starts_with(&writable_root.path),
                "confidential root {} sits under writable sandbox root {}, whose full-access rule would grant ReadFile back",
                confidential_root.display(),
                writable_root.path.display(),
            );
        }
    }
    Ok(())
}

/// Build and apply the Landlock policy in the current process.
///
/// Called inside `pre_exec` (forked child) so it takes effect before exec.
/// Only async-signal-safe operations are performed here: Landlock syscalls
/// and `open(2)` via `PathFd::new`. Path resolution, directory creation,
/// logging, and any allocator-heavy work must happen in the parent before
/// fork — see `LandlockSandbox::apply` and `prepare_cache_dir`.
fn apply_policy(
    read_file_cover: &[PathBuf],
    writable_roots: &[WritableSandboxRoot],
) -> anyhow::Result<()> {
    // Use V3 (Linux 5.19+). The probe in mod.rs verified the kernel supports
    // Landlock; V3 covers all practical kernels in 2026.
    let abi = ABI::V3;
    let full_access = AccessFs::from_all(abi);

    // Read-only subset: allow read and execute, deny all write operations.
    let read_exec = AccessFs::Execute | AccessFs::ReadFile | AccessFs::ReadDir;

    // Everything the blanket `/` grant may still carry once file-content reads
    // are withheld from the confidential mounts: traversal, directory listing
    // and execute. None of these expose the bytes of a secret file, and
    // dropping them would break `ls /` and every toolchain lookup.
    let traverse_exec = AccessFs::Execute | AccessFs::ReadDir;

    let mut ruleset = Ruleset::default()
        .handle_access(full_access)?
        .create()?
        // Traverse, list and execute everywhere on the filesystem. File-content
        // reads are granted separately from `read_file_cover` below, which
        // covers all of `/` minus the confidential mounts (task jqvg).
        .add_rule(PathBeneath::new(PathFd::new("/")?, traverse_exec))?;

    // File-content reads. `read_file_cover` is `["/"]` on any host without the
    // Pod secret mounts, which reproduces the historical blanket grant exactly;
    // in the task-run Pod it is every entry of every ancestor of the
    // confidential mounts, minus those mounts. A path that vanished between the
    // parent's `read_dir` and this `open` is skipped rather than failing the
    // spawn — we cannot log from `pre_exec`, and a missing path grants nothing.
    //
    // NOTE: the `full_access` rules below (worktree, /var/tmp, /cache, the
    // scratch dir, the cargo build dir, `.git/`) include `ReadFile`, so a
    // confidential root nested beneath one of them would be granted back.
    // `confidential_roots_do_not_overlap_writable_sandbox_roots` guards that.
    for path in read_file_cover {
        if let Ok(fd) = PathFd::new(path) {
            ruleset = ruleset.add_rule(PathBeneath::new(fd, read_exec))?;
        }
    }

    // These are the exact roots checked against confidential paths before
    // fork. Required roots retain the historical fail-closed open behaviour;
    // optional cache roots simply grant nothing when absent.
    for writable_root in writable_roots {
        match PathFd::new(&writable_root.path) {
            Ok(fd) => ruleset = ruleset.add_rule(PathBeneath::new(fd, full_access))?,
            Err(error) if writable_root.required => return Err(error.into()),
            Err(_) => {}
        }
    }

    // Shared cross-task cache PVC (`/cache`). The K8s task-run Pod env
    // (djinn-k8s/src/job.rs) routes the toolchain caches here at runtime —
    // CARGO_HOME=/cache/cargo,
    // CARGO_TARGET_DIR=/cache/cargo-target-runs/<task_run_id> for private
    // per-run Cargo target dirs seeded from warm bases when available,
    // SCCACHE_DIR=/cache/sccache/<project> — and the image bakes the Go cache
    // (GOMODCACHE/GOCACHE) onto /cache too. Warm jobs maintain shared base
    // targets under /cache/cargo-target/<project_id>. The broad /cache rule is
    // compatible with both path families and lets build/test commands populate
    // their assigned cache locations (`go mod download` → /cache/go/mod, cargo
    // registry → /cache/cargo, private cargo target artifacts →
    // /cache/cargo-target-runs, warm base maintenance → /cache/cargo-target,
    // sccache → /cache/sccache, etc.). Only present in the K8s task-run Pod
    // (the PVC mount); a no-op elsewhere since the open fails. Guarded:
    // if the dir is absent we silently skip, same as the scratch dir above.
    ruleset.restrict_self()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{OsStr, OsString};

    fn write_file(scope: SandboxScope<'_>, path: &Path) -> std::process::Output {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "printf x > \"$1\"", "--"]).arg(path);
        LandlockSandbox
            .apply(scope, &mut cmd)
            .expect("scope should configure Landlock");
        cmd.output().expect("sandboxed shell should spawn")
    }

    /// Run `sh -c <script>` under the real sandbox policy with an explicit
    /// confidential-root set, exactly as production does with
    /// [`confidential::CONFIDENTIAL_ROOTS`].
    fn run_sandboxed(
        scope: SandboxScope<'_>,
        confidential_roots: &[PathBuf],
        script: &str,
        arg: &Path,
    ) -> std::process::Output {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", script, "--"]).arg(arg);
        LandlockSandbox
            .apply_with_confidential_roots(scope, &mut cmd, confidential_roots)
            .expect("scope should configure Landlock");
        cmd.output().expect("sandboxed shell should spawn")
    }

    /// The task-run Pod inherits `TMPDIR=/workspace` (the read-only PVC mount
    /// root). The sandbox must override it to a Landlock-writable dir, or every
    /// sandboxed tool that honors `$TMPDIR` (go codehost, cargo/cc linker) hits
    /// `permission denied` writing temp under `/workspace`.
    #[test]
    fn apply_redirects_tmpdir_to_var_tmp() {
        let mut cmd = std::process::Command::new("true");
        cmd.env("TMPDIR", "/workspace");

        LandlockSandbox
            .apply(SandboxScope::Worktree(Path::new("/tmp")), &mut cmd)
            .expect("apply should succeed");

        let tmpdir = cmd
            .get_envs()
            .find(|(k, _)| *k == OsStr::new("TMPDIR"))
            .and_then(|(_, v)| v)
            .map(|v| v.to_owned());
        assert_eq!(
            tmpdir,
            Some(OsString::from(crate::SANDBOX_TMPDIR)),
            "sandboxed commands must use a Landlock-writable TMPDIR, not the inherited /workspace"
        );
    }

    /// The override is not a private detail of this backend: it rides the broker
    /// into the launcher's mount namespace, so the Pod renderer has to mount it.
    /// Assert the exported constant is what `apply` actually sets, since that
    /// constant is what `djinn-k8s` renders against.
    #[test]
    fn the_exported_tmpdir_constant_is_the_value_apply_sets() {
        let mut cmd = std::process::Command::new("true");
        LandlockSandbox
            .apply(SandboxScope::Worktree(Path::new("/tmp")), &mut cmd)
            .expect("apply should succeed");
        let injected: Vec<(String, String)> = cmd
            .get_envs()
            .filter_map(|(key, value)| {
                Some((key.to_str()?.to_owned(), value?.to_str()?.to_owned()))
            })
            .collect();
        assert!(
            injected.contains(&("TMPDIR".to_owned(), crate::SANDBOX_TMPDIR.to_owned())),
            "the spawn-time injection set must carry the exported constant; got {injected:?}"
        );
        assert!(
            Path::new(crate::SANDBOX_TMPDIR).is_absolute(),
            "the renderer treats this as a mount path"
        );
    }

    /// Exercise the actual Landlock policy: an owner-cache source is readable
    /// but neither its files nor its Git metadata can be changed, while a task
    /// worktree remains writable.
    #[test]
    fn read_source_policy_denies_content_and_git_writes_but_allows_worktree() {
        if !crate::probe_landlock() {
            return;
        }
        let source = tempfile::tempdir_in(std::env::current_dir().expect("test directory"))
            .expect("read source");
        let source_git = source.path().join(".git");
        std::fs::create_dir(&source_git).expect("source git directory");
        let worktree = tempfile::tempdir_in("/var/tmp").expect("worktree");

        let source_content = source.path().join("source-write");
        assert!(
            !write_file(
                SandboxScope::ReadSource {
                    root: source.path(),
                    cwd: source.path(),
                },
                &source_content,
            )
            .status
            .success(),
            "Landlock must deny writes to read-source content"
        );
        assert!(!source_content.exists());

        let source_metadata = source_git.join("metadata-write");
        assert!(
            !write_file(
                SandboxScope::ReadSource {
                    root: source.path(),
                    cwd: source.path(),
                },
                &source_metadata,
            )
            .status
            .success(),
            "Landlock must deny writes to read-source Git metadata"
        );
        assert!(!source_metadata.exists());

        let worktree_content = worktree.path().join("worktree-write");
        assert!(
            write_file(SandboxScope::Worktree(worktree.path()), &worktree_content)
                .status
                .success(),
            "Landlock must retain task-worktree write access"
        );
        assert!(worktree_content.exists());
    }

    const SPEC_CANARY: &str = "TASK-SPEC-CANARY-jqvg";
    const CREDENTIAL_CANARY: &str = "PROVIDER-CREDENTIAL-CANARY-jqvg";
    const TOKEN_CANARY: &str = "SERVICE-ACCOUNT-TOKEN-CANARY-jqvg";

    /// jqvg — the regression this whole change exists for.
    ///
    /// This does NOT assert on configuration. It builds a fixture with the same
    /// shape as the task-run Pod's secret mounts, applies the real production
    /// `apply_policy` through the real `LandlockSandbox`, spawns a real
    /// `sh -c 'cat ...'`, and fails if the bytes come back. Before this change
    /// the blanket `PathBeneath("/", ReadFile)` grant let both reads through.
    #[test]
    fn shell_sandbox_denies_reading_confidential_mount_contents() {
        if !crate::probe_landlock() {
            return;
        }

        // The fixture must live outside every writable root the policy grants
        // (`/var/tmp`, `/cache`, the worktree, the cargo build dir, the scratch
        // dir): those rules carry `ReadFile` and would grant the secret back
        // through a broader ancestor. The crate directory satisfies that, and
        // is where the read-source test already puts its fixtures.
        let fixture = tempfile::tempdir_in(std::env::current_dir().expect("test directory"))
            .expect("confidential fixture");
        let root = fixture.path().canonicalize().expect("canonical fixture");

        // Mirror the Pod layout: `/var/run/djinn` + `/var/run/secrets/tokens`.
        let credentials_dir = root.join("var/run/djinn");
        let token_dir = root.join("var/run/secrets/tokens");
        std::fs::create_dir_all(&credentials_dir).expect("credentials mount");
        std::fs::create_dir_all(&token_dir).expect("token mount");
        let spec = credentials_dir.join("spec.bin");
        let credentials = credentials_dir.join("credentials.bin");
        let token = token_dir.join("djinn");
        std::fs::write(&spec, SPEC_CANARY).expect("spec");
        std::fs::write(&credentials, CREDENTIAL_CANARY).expect("credentials");
        std::fs::write(&token, TOKEN_CANARY).expect("token");

        // A neighbour of both mounts, one level up: it must stay readable, or
        // the exclusion is over-broad.
        let neighbour = root.join("var/run/neighbour.txt");
        std::fs::write(&neighbour, "NEIGHBOUR").expect("neighbour");

        let confidential_roots = vec![credentials_dir.clone(), root.join("var/run/secrets")];
        let worktree = tempfile::tempdir_in("/var/tmp").expect("worktree");
        let scope = SandboxScope::Worktree(worktree.path());

        for (label, secret, canary) in [
            ("task spec", &spec, SPEC_CANARY),
            ("provider credentials", &credentials, CREDENTIAL_CANARY),
            ("projected ServiceAccount token", &token, TOKEN_CANARY),
        ] {
            let direct = run_sandboxed(scope, &confidential_roots, "cat \"$1\"", secret);
            assert!(
                !direct.status.success(),
                "sandboxed shell read the {label} at {}",
                secret.display()
            );
            assert!(
                !String::from_utf8_lossy(&direct.stdout).contains(canary),
                "the {label} canary leaked to a sandboxed shell"
            );

            // Landlock matches the resolved dentry, not the path string, so the
            // `/proc/self/root` magic symlink must not launder the read.
            let laundered = PathBuf::from("/proc/self/root")
                .join(secret.strip_prefix("/").expect("absolute secret path"));
            let via_proc = run_sandboxed(scope, &confidential_roots, "cat \"$1\"", &laundered);
            assert!(
                !String::from_utf8_lossy(&via_proc.stdout).contains(canary),
                "the {label} canary leaked via /proc/self/root"
            );
        }

        // Legitimate reads must survive. A neighbour inside the excluded dirs'
        // parent, and a workspace file — plus the fact that `sh` itself
        // executed at all, which needs `Execute` and `ReadFile` on the
        // interpreter and its shared libraries.
        let allowed = run_sandboxed(scope, &confidential_roots, "cat \"$1\"", &neighbour);
        assert!(
            allowed.status.success(),
            "sandboxed shell lost a legitimate read next to the secret mounts: {}",
            String::from_utf8_lossy(&allowed.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&allowed.stdout), "NEIGHBOUR");

        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let source_read = run_sandboxed(scope, &confidential_roots, "cat \"$1\"", &manifest);
        assert!(
            source_read.status.success(),
            "sandboxed shell lost a legitimate workspace read: {}",
            String::from_utf8_lossy(&source_read.stderr)
        );
        assert!(String::from_utf8_lossy(&source_read.stdout).contains("djinn-sandbox"));

        // Cargo build scripts are repository-controlled children of Cargo, not
        // shell text. Each fixture succeeds only if its own mounted-path read
        // was denied. A widened cover or omitted confidential root therefore
        // prints the unique canary in Cargo output and fails this test.
        for (index, (label, secret, canary)) in [
            ("task spec", &spec, SPEC_CANARY),
            ("provider credentials", &credentials, CREDENTIAL_CANARY),
            ("projected ServiceAccount token", &token, TOKEN_CANARY),
        ]
        .into_iter()
        .enumerate()
        {
            let package = worktree.path().join(format!("build-script-{index}"));
            std::fs::create_dir_all(package.join("src")).expect("build-script package");
            std::fs::write(
                package.join("Cargo.toml"),
                format!("[package]\nname = \"landlock-canary-{index}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"),
            )
            .expect("manifest");
            std::fs::write(package.join("src/lib.rs"), "pub fn fixture() {}\n")
                .expect("library source");
            std::fs::write(
                package.join("build.rs"),
                "fn main() {\n    let path = std::env::var(\"DJINN_CANARY_PATH\").unwrap();\n    if let Ok(contents) = std::fs::read_to_string(path) {\n        println!(\"cargo:warning={contents}\");\n        panic!(\"confidential build-script read succeeded\");\n    }\n}\n",
            )
            .expect("build script");

            let mut cargo = std::process::Command::new("cargo");
            cargo
                .arg("build")
                .current_dir(&package)
                .env("CARGO_TARGET_DIR", package.join("target"))
                .env("DJINN_CANARY_PATH", secret);
            LandlockSandbox
                .apply_with_confidential_roots(scope, &mut cargo, &confidential_roots)
                .expect("Cargo should receive the production Landlock policy");
            let output = cargo.output().expect("sandboxed Cargo should spawn");
            let captured = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                output.status.success(),
                "Cargo build script read the {label}: {captured}"
            );
            assert!(
                !captured.contains(canary),
                "the {label} canary leaked through Cargo build-script output"
            );
        }

        // Directory listing stays granted everywhere, including at `/` and
        // inside the excluded mount — a filename is not the secret.
        for dir in ["/", "/usr"] {
            let listing = run_sandboxed(scope, &confidential_roots, "ls \"$1\"", Path::new(dir));
            assert!(
                listing.status.success(),
                "sandboxed shell lost `ls {dir}`: {}",
                String::from_utf8_lossy(&listing.stderr)
            );
        }

        // Writes to the secret mounts stay denied, as before.
        assert!(
            !run_sandboxed(
                scope,
                &confidential_roots,
                "printf x > \"$1\"",
                &credentials
            )
            .status
            .success(),
            "Landlock must still deny writes to the credential mount"
        );
    }

    /// The same denial, asserted against the REAL production mount paths on
    /// any host that actually has them — which includes the task-run Pod djinn
    /// verifies in, where `/var/run/djinn/credentials.bin` is the org's live
    /// credential bundle. Self-skips elsewhere; the fixture test above carries
    /// the guarantee on a developer laptop and in a plain CI container.
    #[test]
    fn shell_sandbox_denies_reading_the_real_pod_secret_mounts() {
        if !crate::probe_landlock() {
            return;
        }
        let roots = confidential::present_confidential_roots(confidential::CONFIDENTIAL_ROOTS);
        if roots.is_empty() {
            return;
        }

        let worktree = tempfile::tempdir_in("/var/tmp").expect("worktree");
        let scope = SandboxScope::Worktree(worktree.path());

        // Mirrors `djinn_k8s::job::{CREDENTIALS_MOUNT_FILE, TOKEN_MOUNT_FILE}`.
        for secret in [
            "/var/run/djinn/credentials.bin",
            "/var/run/secrets/tokens/djinn",
        ] {
            let path = Path::new(secret);
            if !path.exists() {
                continue;
            }
            let output = run_sandboxed(scope, &roots, "cat \"$1\"", path);
            assert!(
                !output.status.success(),
                "sandboxed shell read the live secret at {secret}"
            );
            assert!(
                output.stdout.is_empty(),
                "sandboxed shell got {} bytes out of the live secret at {secret}",
                output.stdout.len()
            );
        }

        // The same environment must still serve an ordinary read.
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        assert!(
            run_sandboxed(scope, &roots, "cat \"$1\"", &manifest)
                .status
                .success(),
            "the production confidential-root set broke an ordinary read"
        );
    }

    /// A confidential root nested under one of the policy's `full_access`
    /// roots would be granted `ReadFile` back through that broader rule. The
    /// production set must never overlap them.
    #[test]
    fn confidential_roots_do_not_overlap_writable_sandbox_roots() {
        let roots = confidential::CONFIDENTIAL_ROOTS
            .iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        ensure_confidential_roots_do_not_overlap_writable_roots(
            &roots,
            &writable_sandbox_roots(None, None, None, None),
        )
        .expect("production confidential roots must not sit below a full-access rule");
    }

    /// Every returned root is both validated and installed as `full_access`.
    /// This is intentionally lexical: it catches a mutation such as changing
    /// `SANDBOX_TMPDIR` from `/var/tmp` to `/var` even on a CI host that has no
    /// live `/var/run` secret mounts.
    #[test]
    fn every_full_access_root_rejects_a_confidential_descendant() {
        let fixture = tempfile::tempdir_in("/var/tmp").expect("writable-root fixture");
        let worktree = fixture.path().join("worktree");
        let cache = fixture.path().join("scratch");
        let cargo_build = fixture.path().join("cargo-build");
        let git_meta = fixture.path().join("git-meta");
        for directory in [&worktree, &cache, &cargo_build, &git_meta] {
            std::fs::create_dir_all(directory).expect("fixture directory");
        }

        let writable_roots = writable_sandbox_roots(
            Some(&worktree),
            Some(&git_meta),
            Some(&cache),
            Some(&cargo_build),
        );
        for writable_root in &writable_roots {
            let confidential = vec![writable_root.path.join("confidential-canary")];
            assert!(
                ensure_confidential_roots_do_not_overlap_writable_roots(
                    &confidential,
                    &writable_roots,
                )
                .is_err(),
                "the full-access root {} escaped overlap validation",
                writable_root.path.display(),
            );
        }
    }
}
