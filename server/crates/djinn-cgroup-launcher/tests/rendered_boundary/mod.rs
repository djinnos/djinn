//! Privileged harness for the adversarial kernel-boundary proof (task zf13,
//! goxi AC2). `tests/kernel_boundary_under_rendered_context.rs` holds the
//! assertions; this module only builds the production-shaped topology:
//!
//! ```text
//!   test process (uid 0)  = the launcher sidecar: NativeCgroupFs + Broker +
//!                           UnixBrokerServer over a real Unix socket
//!   forked worker (1000)  = the trusted, non-dumpable worker: real
//!                           SO_PEERCRED authentication, real controls
//!   launched child (1001) = forked, placed in the invocation cgroup by a
//!                           `cgroup.procs` write and released only once the
//!                           placement holds, then through the production
//!                           close_range + `child::prepare_child` boundary
//! ```
//!
//! Nothing here silently degrades: every precondition is a panic naming the
//! missing capability, so an environment that cannot host the proof can never
//! be mistaken for a boundary that held.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::path::{Path, PathBuf};

use djinn_cgroup_launcher::broker::{Broker, BrokerConfig, ChildStatus, OsNonceSource};
use djinn_cgroup_launcher::child::{
    ChildMounts, NativeChildSyscalls, NativeWorkerDumpability, prepare_child,
    prepare_worker_readiness,
};
use djinn_cgroup_launcher::transport::{UnixBrokerClient, UnixBrokerServer};
use djinn_cgroup_launcher::{
    ChildProcess, CommandSpec, Error, Invocation, Launcher, LauncherConfig, LeaseAuthority,
    NativeCgroupFs, NativeCgroupSpawn, SpawnIntoCgroup,
};

/// Invocation whose child is the in-cgroup adversary rather than an exec.
pub const ATTACK_INVOCATION: &str = "zf13-attack";
/// Invocation the legitimate worker owns; the positive-control target.
pub const LEGIT_INVOCATION: &str = "zf13-legit";
/// Invocation that really `execve`s an attacking `/bin/sh`.
pub const EXECED_INVOCATION: &str = "zf13-execed";
/// Durable fencing token the worker holds for [`LEGIT_INVOCATION`].
pub const LEGIT_FENCE: u64 = 0x5a5a_f13d;
/// `cpu.max` an unleased invocation must carry (250m of a 100ms period).
pub const UNLEASED_CPU_MAX: &str = "25000 100000";
/// `cpu.max` after a matching fencing token lifts the quota.
///
/// Task 7deu: this used to be `"max 100000"`, i.e. no quota at all. That was
/// only ever harmless because an ancestor still clamped it — the launcher
/// container's own 250m CPU limit. With that limit removed (it was what made the
/// whole feature a no-op) an unbounded lift would let one build take the entire
/// node, so the lift now writes the pod's declared CPU budget.
pub const LIFTED_CPU_MAX: &str = "400000 100000";

// ───────────────────────────── rendered context ─────────────────────────────

/// The rendered enforcement-Pod security context, loaded from the fixture that
/// `djinn-k8s` asserts against its real manifest builders.
pub struct RenderedContext {
    values: BTreeMap<String, String>,
}

impl RenderedContext {
    pub fn load() -> Self {
        let path = fixture_path();
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("rendered security-context fixture {path:?}: {e}"));
        let mut values = BTreeMap::new();
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .unwrap_or_else(|| panic!("malformed fixture line: {line}"));
            values.insert(key.trim().to_owned(), value.trim().to_owned());
        }
        Self { values }
    }

    pub fn get(&self, key: &str) -> &str {
        self.values
            .get(key)
            .unwrap_or_else(|| panic!("rendered security context is missing `{key}`"))
    }

    /// Whether the rendered contract names a key. This makes removal of the
    /// old bootstrap-capability contract explicit rather than treating a missing
    /// fixture value as an accidental parse failure.
    pub fn contains(&self, key: &str) -> bool {
        self.values.contains_key(key)
    }

    pub fn u32(&self, key: &str) -> u32 {
        self.get(key)
            .parse()
            .unwrap_or_else(|e| panic!("rendered `{key}` is not a number: {e}"))
    }
}

pub fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rendered-security-context.env")
}

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .expect("canonicalize repository root")
}

// ─────────────────────────── privileged preconditions ───────────────────────

/// Refuse to run — loudly — unless this process can actually host the proof.
///
/// Every branch panics with the concrete missing capability. There is no early
/// `return` and no boolean a caller could mistake for success: an unavailable
/// environment fails the test rather than skipping it.
pub fn require_privileged_environment(context: &RenderedContext) -> Environment {
    let euid = unsafe { libc::geteuid() };
    assert_eq!(
        euid,
        context.u32("launcher_expected_uid"),
        "the adversarial kernel-boundary proof must run as the rendered launcher uid \
         (uid {euid} cannot create real UID-1000/1001 processes). It runs in the \
         `launcher-kernel-boundary` CI lane; reproduce it locally with: \
         `cargo test -p djinn-cgroup-launcher --test kernel_boundary_under_rendered_context \
         --no-run` then `docker run --rm --privileged --cgroupns=private -v \"$PWD:$PWD\" \
         ubuntu:24.04 bash -c 'mkdir -p /workspace && exec \"$0\" --ignored --test-threads 1' \
         <the built test binary>`"
    );

    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
        .expect("read /proc/self/mountinfo for the cgroup delegation mode");
    let mounted = |name: &str| {
        let prefix = format!("{name} ");
        mountinfo.lines().any(|line| {
            line.split(" - ")
                .nth(1)
                .is_some_and(|tail| tail.starts_with(&prefix))
        })
    };
    assert!(
        mounted("cgroup2"),
        "no cgroup2 mount: the rendered delegation profile is {} and the launcher's \
         readiness contract rejects anything else",
        context.get("cgroup_delegation_profile")
    );
    assert!(
        !mounted("cgroup"),
        "a cgroup v1 mount is present, so this host is hybrid and the launcher rejects \
         it at readiness. The boundary cannot be proven here"
    );
    assert!(
        Path::new("/proc/sys/kernel/seccomp/actions_avail").exists(),
        "seccomp filtering is unavailable, so the child syscall boundary cannot install"
    );
    assert!(
        Path::new("/workspace").is_dir(),
        "/workspace is missing: the rendered CommandSpec cwd must exist before a child \
         can exec (the privileged lane creates it)"
    );

    let slot = unique_slot();
    let root = enable_cpu_delegation(&slot);
    let ipc = PathBuf::from(format!("/tmp/djinn-zf13-{slot}"));
    let _ = std::fs::remove_dir_all(&ipc);
    std::fs::create_dir(&ipc).expect("create launcher IPC directory");
    set_mode(&ipc, 0o755);

    // The worker owns its private credential exactly as the real handshake
    // publishes it: 0600, worker-owned, so uid 1001 can never read it.
    let credential = random_bytes(32);
    let credential_path = ipc.join("credential");
    std::fs::write(&credential_path, &credential).expect("publish worker credential");
    set_mode(&credential_path, 0o600);
    chown(
        &credential_path,
        context.u32("worker_run_as_user"),
        context.u32("worker_run_as_group"),
    );

    Environment {
        root,
        socket: ipc.join("broker.sock"),
        credential_path,
        credential,
        ipc,
    }
}

/// Create a delegated cgroup-v2 root owned by uid 0 with exactly the `cpu`
/// controller enabled — the only profile the launcher's readiness accepts.
fn enable_cpu_delegation(slot: &str) -> PathBuf {
    let base = PathBuf::from("/sys/fs/cgroup");
    let root = base.join(format!("djinn-zf13-{slot}"));
    let _ = std::fs::remove_dir(&root);
    std::fs::create_dir(&root).unwrap_or_else(|e| panic!("create delegated cgroup {root:?}: {e}"));
    if let Err(first) = delegate_cpu(&base, &root) {
        // cgroup v2 exempts only the TRUE root from the "no internal processes"
        // rule. On a systemd host `/sys/fs/cgroup` is that root and the first
        // attempt succeeds; inside a container it is a namespaced view of an
        // ordinary cgroup that still holds the container's own processes, so
        // delegation is refused. Roll the parent back first — a cgroup whose
        // subtree_control is already populated will not let its members move —
        // then empty it and retry.
        let _ = std::fs::write(base.join("cgroup.subtree_control"), "-cpu");
        vacate_cgroup_root(&base);
        delegate_cpu(&base, &root).unwrap_or_else(|second| {
            panic!(
                "cannot delegate the cpu controller to {root:?} ({first}; after vacating the \
                 cgroup root: {second}); the launcher boundary cannot be proven on this host"
            )
        });
    }
    root
}

fn delegate_cpu(base: &Path, root: &Path) -> std::io::Result<()> {
    let available = std::fs::read_to_string(root.join("cgroup.controllers")).unwrap_or_default();
    if !available.split_ascii_whitespace().any(|c| c == "cpu") {
        std::fs::write(base.join("cgroup.subtree_control"), "+cpu")?;
    }
    std::fs::write(root.join("cgroup.subtree_control"), "+cpu")
}

/// Move every process sitting directly in the cgroup root into a holding leaf.
/// A no-op on a systemd host (the root is already empty).
fn vacate_cgroup_root(base: &Path) {
    let holding = base.join("djinn-zf13-holding");
    let _ = std::fs::create_dir(&holding);
    let procs = std::fs::read_to_string(base.join("cgroup.procs")).unwrap_or_default();
    for pid in procs.split_ascii_whitespace() {
        let _ = std::fs::write(holding.join("cgroup.procs"), pid);
    }
}

/// Live, root-owned topology plus best-effort teardown.
pub struct Environment {
    pub root: PathBuf,
    pub ipc: PathBuf,
    pub socket: PathBuf,
    pub credential_path: PathBuf,
    pub credential: Vec<u8>,
}

impl Environment {
    /// Direct children of the delegated cgroup root — the authority on whether
    /// an unauthorized cgroup appeared.
    pub fn cgroup_children(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.root)
            .expect("read delegated cgroup root")
            .filter_map(|entry| {
                let entry = entry.ok()?;
                entry
                    .file_type()
                    .ok()?
                    .is_dir()
                    .then(|| entry.file_name().to_string_lossy().into_owned())
            })
            .collect();
        names.sort();
        names
    }

    /// The applied `cpu.max` of one invocation leaf.
    pub fn quota(&self, leaf: &str) -> String {
        std::fs::read_to_string(self.root.join(leaf).join("cpu.max"))
            .unwrap_or_else(|e| panic!("read cpu.max of leaf {leaf}: {e}"))
            .trim()
            .to_owned()
    }
}

impl Drop for Environment {
    fn drop(&mut self) {
        while unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) } > 0 {}
        if self.root.is_dir() {
            for leaf in self.cgroup_children() {
                let path = self.root.join(&leaf);
                let _ = std::fs::write(path.join("cgroup.kill"), "1");
                for _ in 0..100 {
                    if std::fs::remove_dir(&path).is_ok() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        }
        let _ = std::fs::remove_dir(&self.root);
        let _ = std::fs::remove_dir_all(&self.ipc);
    }
}

// ─────────────────────────────── the worker ─────────────────────────────────

/// A live UID-1000 worker process holding an authenticated broker connection.
pub struct Worker {
    pub pid: i32,
    to_worker: std::fs::File,
    from_worker: BufReader<std::fs::File>,
}

impl Worker {
    /// Fork the worker BEFORE any broker thread exists, so the fork happens in
    /// a single-threaded process.
    pub fn fork(environment: &Environment, context: &RenderedContext) -> Self {
        let (up_read, up_write) = pipe();
        let (down_read, down_write) = pipe();
        let uid = context.u32("worker_run_as_user");
        let gid = context.u32("worker_run_as_group");
        let socket = environment.socket.clone();
        let credential = environment.credential.clone();

        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork the worker: {}", last_error());
        if pid == 0 {
            unsafe {
                libc::close(up_read);
                libc::close(down_write);
            }
            worker_main(uid, gid, &socket, &credential, down_read, up_write);
        }
        unsafe {
            libc::close(up_write);
            libc::close(down_read);
        }
        let mut worker = Self {
            pid,
            to_worker: unsafe { std::fs::File::from_raw_fd(down_write) },
            from_worker: BufReader::new(unsafe { std::fs::File::from_raw_fd(up_read) }),
        };
        assert_eq!(
            worker.expect_line(),
            "dropped",
            "the worker must reach the rendered worker identity and be non-dumpable"
        );
        worker
    }

    pub fn send(&mut self, line: &str) {
        writeln!(self.to_worker, "{line}").expect("signal the worker");
        self.to_worker.flush().expect("flush worker signal");
    }

    pub fn expect_line(&mut self) -> String {
        let mut line = String::new();
        let read = self
            .from_worker
            .read_line(&mut line)
            .expect("read the worker report");
        assert!(read > 0, "the worker died before reporting");
        let line = line.trim_end().to_owned();
        assert!(
            !line.starts_with("fail:"),
            "the legitimate worker could not complete a step it must be able to: {line}"
        );
        line
    }

    /// Collect worker lines until `terminator`, returning the ones before it.
    pub fn collect_until(&mut self, terminator: &str) -> Vec<String> {
        let mut lines = Vec::new();
        loop {
            let line = self.expect_line();
            if line == terminator {
                return lines;
            }
            lines.push(line);
        }
    }

    /// The worker is still scheduled and still owns its connection.
    pub fn is_alive(&self) -> bool {
        unsafe { libc::kill(self.pid, 0) == 0 }
    }
}

/// The forked worker: drop to the rendered worker identity, prove
/// non-dumpability, authenticate, and drive real controls on demand.
fn worker_main(
    uid: u32,
    gid: u32,
    socket: &Path,
    credential: &[u8],
    from_parent: RawFd,
    to_parent: RawFd,
) -> ! {
    let groups = [gid];
    let dropped = unsafe {
        libc::setgroups(1, groups.as_ptr()) == 0
            && libc::setresgid(gid, gid, gid) == 0
            && libc::setresuid(uid, uid, uid) == 0
    };
    let mut out = unsafe { std::fs::File::from_raw_fd(to_parent) };
    let mut input = BufReader::new(unsafe { std::fs::File::from_raw_fd(from_parent) });
    macro_rules! say {
        ($($arg:tt)*) => {{
            let _ = writeln!(out, $($arg)*);
            let _ = out.flush();
        }};
    }
    macro_rules! wait_for {
        ($expected:expr, $code:expr) => {{
            let mut line = String::new();
            let _ = input.read_line(&mut line);
            if line.trim_end() != $expected {
                unsafe { libc::_exit($code) };
            }
        }};
    }
    if !dropped {
        say!("fail:credential drop: {}", last_error());
        unsafe { libc::_exit(11) };
    }
    // A real prctl on a real process: from here the worker is non-dumpable.
    if prepare_worker_readiness(
        &mut NativeWorkerDumpability,
        djinn_cgroup_launcher::LauncherAuthorityProtocol::LeafV1,
    )
    .is_err()
    {
        say!("fail:non-dumpable readiness");
        unsafe { libc::_exit(12) };
    }
    say!("dropped");
    wait_for!("connect", 13);

    let mut client = match UnixBrokerClient::connect_path(socket, credential) {
        Ok(client) => client,
        Err(error) => {
            say!("fail:connect broker as uid {uid}: {error}");
            unsafe { libc::_exit(14) };
        }
    };
    match prepare_worker_readiness(
        &mut NativeWorkerDumpability,
        djinn_cgroup_launcher::LauncherAuthorityProtocol::LeafV1,
    )
    .map(|r| client.ready(r))
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) | Err(error) => {
            say!("fail:worker readiness: {error}");
            unsafe { libc::_exit(15) };
        }
    }
    say!("connected");

    // POSITIVE CONTROL 1: the legitimate connection creates its own invocations.
    let worker_pid = unsafe { libc::getpid() };
    for (id, fence, leaf, command) in [
        (LEGIT_INVOCATION, LEGIT_FENCE, "legit", sleep_command()),
        (ATTACK_INVOCATION, 7, "attack", sleep_command()),
        (
            EXECED_INVOCATION,
            8,
            "execed",
            execed_command(worker_pid, socket),
        ),
    ] {
        let invocation = Invocation {
            id: id.to_owned(),
            fence,
        };
        if let Err(error) = client.begin(invocation) {
            say!("fail:begin {id}: {error}");
            unsafe { libc::_exit(17) };
        }
        if let Err(error) = client.create(id, leaf, LeaseAuthority::Armed, &command) {
            say!("fail:create {id}: {error}");
            unsafe { libc::_exit(18) };
        }
    }
    say!("created");

    // Drain the exec'd adversary's own report through the broker stdout path.
    let mut execed = Vec::new();
    for _ in 0..400 {
        match client.stdout(EXECED_INVOCATION) {
            Ok((bytes, eof, status)) => {
                execed.extend(bytes);
                if eof || !matches!(status, ChildStatus::Running) {
                    break;
                }
            }
            Err(error) => {
                say!("fail:execed stdout: {error}");
                unsafe { libc::_exit(19) };
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    for line in String::from_utf8_lossy(&execed).lines() {
        let line = line.trim();
        if !line.is_empty() {
            say!("execed {line}");
        }
    }
    say!("execed-done");

    wait_for!("controls", 20);
    // POSITIVE CONTROL 2: only one matching durable fencing token lifts quota.
    // Every control is separated by a parent round-trip so it can read `cpu.max`
    // after the real lift and again after the replay without racing either call.
    let mismatched = client.lift(LEGIT_INVOCATION, LEGIT_FENCE ^ 1);
    say!("mismatched-fence-rejected {}", mismatched.is_err());
    wait_for!("lift", 21);
    let matching = client.lift(LEGIT_INVOCATION, LEGIT_FENCE);
    say!("matching-fence-lifted {}", matching.is_ok());
    wait_for!("replay", 22);
    let replayed_matching = client.lift(LEGIT_INVOCATION, LEGIT_FENCE);
    say!(
        "replayed-matching-fence-rejected {}",
        replayed_matching.is_err()
    );
    say!("controls-done");

    wait_for!("exit", 0);
    unsafe { libc::_exit(0) }
}

fn sleep_command() -> CommandSpec {
    CommandSpec {
        program: "/bin/sleep".to_owned(),
        argv: vec!["45".to_owned()],
        cwd: "/workspace".to_owned(),
        environment: vec![],
    }
}

/// A genuinely `execve`d adversary. It proves the credential drop and the
/// seccomp filter survive exec, which an in-process child alone cannot show.
fn execed_command(worker_pid: i32, socket: &Path) -> CommandSpec {
    let socket = socket.display();
    let script = format!(
        "p={worker_pid}\n\
         for f in environ maps; do if cat /proc/$p/$f >/dev/null 2>&1; \
           then echo $f=leaked; else echo $f=denied; fi; done\n\
         for d in fd root cwd; do if ls /proc/$p/$d >/dev/null 2>&1; \
           then echo $d=leaked; else echo $d=denied; fi; done\n\
         if kill -STOP $p 2>/dev/null; then echo signal=leaked; else echo signal=denied; fi\n\
         if cat {socket} >/dev/null 2>&1; then echo socket=leaked; else echo socket=denied; fi\n\
         echo uid=$(id -u)\n\
         echo gid=$(id -g)\n"
    );
    CommandSpec {
        program: "/bin/sh".to_owned(),
        argv: vec!["-c".to_owned(), script],
        cwd: "/workspace".to_owned(),
        environment: vec![("PATH".to_owned(), "/usr/bin:/bin".to_owned())],
    }
}

// ─────────────────────────── the launched adversary ─────────────────────────

/// Pre-built, allocation-free attack inputs. Everything the cloned child
/// touches is constructed in the parent, so the child performs only
/// async-signal-safe work after `clone3`.
struct Probes {
    worker_pid: i32,
    proc_paths: Vec<(&'static str, CString, i32)>,
    fd_link: CString,
    credential: CString,
    socket: CString,
    cgroup_procs: CString,
    unauthorized_cgroup: CString,
    attack_quota: CString,
}

/// Clone seam that births a real UID-1001 adversary inside the invocation
/// cgroup. Every other invocation is delegated to the production
/// `NativeCgroupSpawn`, so the legitimate and exec'd children take the shipped path.
pub struct AdversaryClone {
    report: RawFd,
    delegate: NativeCgroupSpawn,
    probes: Probes,
}

impl AdversaryClone {
    pub fn new(environment: &Environment, worker_pid: i32, report: RawFd) -> Self {
        let root = environment.root.display();
        let proc_paths = vec![
            (
                "proc_mem",
                path_cstring(&format!("/proc/{worker_pid}/mem")),
                libc::O_RDONLY,
            ),
            (
                "proc_environ",
                path_cstring(&format!("/proc/{worker_pid}/environ")),
                libc::O_RDONLY,
            ),
            (
                "proc_maps",
                path_cstring(&format!("/proc/{worker_pid}/maps")),
                libc::O_RDONLY,
            ),
            (
                "proc_root",
                path_cstring(&format!("/proc/{worker_pid}/root")),
                libc::O_RDONLY | libc::O_DIRECTORY,
            ),
            (
                "proc_fd",
                path_cstring(&format!("/proc/{worker_pid}/fd")),
                libc::O_RDONLY | libc::O_DIRECTORY,
            ),
        ];
        Self {
            report,
            delegate: NativeCgroupSpawn,
            probes: Probes {
                worker_pid,
                proc_paths,
                fd_link: path_cstring(&format!("/proc/{worker_pid}/fd/3")),
                credential: path_cstring(&environment.credential_path.to_string_lossy()),
                socket: path_cstring(&environment.socket.to_string_lossy()),
                cgroup_procs: path_cstring("cgroup.procs"),
                unauthorized_cgroup: path_cstring(&format!("{root}/zf13-pwned")),
                attack_quota: path_cstring(&format!("{root}/attack/cpu.max")),
            },
        }
    }
}

impl SpawnIntoCgroup for AdversaryClone {
    fn spawn_into_cgroup(
        &mut self,
        cgroup: RawFd,
        invocation: &Invocation,
        command: &CommandSpec,
    ) -> Result<ChildProcess, Error> {
        if invocation.id != ATTACK_INVOCATION {
            return self.delegate.spawn_into_cgroup(cgroup, invocation, command);
        }
        let (go_read, go_write) = pipe();
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(Error::SpawnDenied);
        }
        if pid == 0 {
            unsafe { libc::close(go_write) };
            // Stand at the gate exactly as the production child does: no work
            // whatsoever until the parent has placed us in the invocation
            // cgroup and said so.
            let mut byte = [0_u8; 1];
            let read = unsafe { libc::read(go_read, byte.as_mut_ptr().cast(), 1) };
            if read != 1 {
                unsafe { libc::_exit(91) };
            }
            unsafe { libc::close(go_read) };
            attack(&self.probes, self.report, cgroup);
        }
        unsafe { libc::close(go_read) };
        place_in_cgroup(cgroup, pid).expect("place the adversary in the invocation cgroup");
        let go = *b"G";
        assert_eq!(
            unsafe { libc::write(go_write, go.as_ptr().cast(), 1) },
            1,
            "release the adversary once it is inside the invocation cgroup"
        );
        unsafe { libc::close(go_write) };
        Ok(ChildProcess {
            pid,
            stdout: -1,
            stderr: -1,
        })
    }
}

/// Runs in the freshly cloned child. It crosses the production isolation and
/// credential boundary exactly as `NativeCgroupSpawn` does — `close_range` over
/// every inherited descriptor (sparing only stdio and the report pipe) then
/// `prepare_child` — and only then attacks the worker.
fn attack(probes: &Probes, report: RawFd, cgroup: RawFd) -> ! {
    unsafe {
        libc::syscall(libc::SYS_close_range, 3_u32, (report - 1) as u32, 0_u32);
        libc::syscall(libc::SYS_close_range, (report + 1) as u32, u32::MAX, 0_u32);
    }
    if prepare_child(&mut NativeChildSyscalls, &[], &ChildMounts::isolated()).is_err() {
        unsafe { libc::_exit(90) };
    }

    // Control-descriptor reuse: the launcher's own cgroup descriptor number.
    emit(report, "control_fd_reuse", unsafe {
        i64::from(libc::openat(
            cgroup,
            probes.cgroup_procs.as_ptr(),
            libc::O_WRONLY,
        ))
    });

    // The identity the boundary itself produced.
    emit(report, "identity_uid", i64::from(unsafe { libc::getuid() }));
    emit(report, "identity_gid", i64::from(unsafe { libc::getgid() }));
    emit(
        report,
        "identity_groups",
        i64::from(unsafe { libc::getgroups(0, std::ptr::null_mut()) }),
    );
    let previous = unsafe { libc::umask(0o022) };
    emit(report, "identity_umask", i64::from(previous));
    unsafe { libc::umask(previous) };

    // /proc/<worker>/… against a non-dumpable worker.
    for (key, path, flags) in &probes.proc_paths {
        emit(
            report,
            key,
            i64::from(unsafe { libc::open(path.as_ptr(), *flags) }),
        );
    }
    let mut link = [0 as libc::c_char; 256];
    emit(report, "proc_fd_link", unsafe {
        libc::readlink(probes.fd_link.as_ptr(), link.as_mut_ptr(), link.len()) as i64
    });

    // Cross-process memory and control.
    let pid = i64::from(probes.worker_pid);
    emit(report, "ptrace_attach", unsafe {
        libc::syscall(
            libc::SYS_ptrace,
            libc::PTRACE_ATTACH as i64,
            pid,
            0_i64,
            0_i64,
        )
    });
    emit(report, "ptrace_seize", unsafe {
        libc::syscall(
            libc::SYS_ptrace,
            libc::PTRACE_SEIZE as i64,
            pid,
            0_i64,
            0_i64,
        )
    });
    let mut scratch = [0_u8; 64];
    let local = libc::iovec {
        iov_base: scratch.as_mut_ptr().cast(),
        iov_len: scratch.len(),
    };
    let remote = libc::iovec {
        iov_base: std::ptr::without_provenance_mut(0x1000),
        iov_len: scratch.len(),
    };
    for (key, number) in [
        ("process_vm_readv", libc::SYS_process_vm_readv),
        ("process_vm_writev", libc::SYS_process_vm_writev),
    ] {
        emit(report, key, unsafe {
            libc::syscall(
                number,
                pid,
                &raw const local,
                1_i64,
                &raw const remote,
                1_i64,
                0_i64,
            )
        });
    }
    for (key, signal) in [
        ("signal_stop", libc::SIGSTOP),
        ("signal_kill", libc::SIGKILL),
        ("signal_usr1", libc::SIGUSR1),
    ] {
        emit(
            report,
            key,
            i64::from(unsafe { libc::kill(probes.worker_pid, signal) }),
        );
    }
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0_i64) };
    emit(
        report,
        "pidfd_getfd",
        if pidfd < 0 {
            pidfd
        } else {
            unsafe { libc::syscall(libc::SYS_pidfd_getfd, pidfd, 3_i64, 0_i64) }
        },
    );

    // Broker surface: the worker-private credential and the control socket.
    emit(
        report,
        "broker_credential_read",
        i64::from(unsafe { libc::open(probes.credential.as_ptr(), libc::O_RDONLY) }),
    );
    emit(
        report,
        "broker_socket_connect",
        connect_unix(&probes.socket),
    );

    // Cgroup authority by path, not by the (already closed) descriptor.
    emit(
        report,
        "cgroup_create",
        i64::from(unsafe { libc::mkdir(probes.unauthorized_cgroup.as_ptr(), 0o755) }),
    );
    emit(
        report,
        "cgroup_quota_write",
        i64::from(unsafe { libc::open(probes.attack_quota.as_ptr(), libc::O_WRONLY) }),
    );

    // Re-privileging attempts the seccomp filter must refuse.
    for (key, number, argument) in [
        ("regain_uid", libc::SYS_setresuid, 0_i64),
        ("regain_caps", libc::SYS_capset, 0),
        (
            "unshare_userns",
            libc::SYS_unshare,
            i64::from(libc::CLONE_NEWUSER),
        ),
        ("mount_any", libc::SYS_mount, 0),
    ] {
        emit(report, key, unsafe {
            libc::syscall(number, argument, 0_i64, 0_i64, 0_i64, 0_i64)
        });
    }

    emit(report, "end", 0);
    unsafe { libc::_exit(0) }
}

fn connect_unix(path: &CString) -> i64 {
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return i64::from(fd);
    }
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (slot, byte) in address.sun_path.iter_mut().zip(path.as_bytes()) {
        *slot = *byte as libc::c_char;
    }
    let rc = unsafe {
        libc::connect(
            fd,
            (&raw const address).cast(),
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    let saved = errno_value();
    unsafe {
        libc::close(fd);
        *libc::__errno_location() = saved;
    }
    i64::from(rc)
}

/// Place `pid` in the invocation cgroup by writing `cgroup.procs` — the same
/// placement the production seam performs, so the adversary really is a member
/// of the invocation cgroup and the boundary being proven is the shipped one.
///
/// Task 7deu: this used to be `clone3(CLONE_INTO_CGROUP)`. Proving a boundary
/// with a spawn mechanism production does not use proves the wrong thing, and
/// the `clone3` flag constant this harness took from `libc` was the one that
/// silently truncated to zero.
fn place_in_cgroup(cgroup: RawFd, pid: i32) -> std::io::Result<()> {
    let name = CString::new("cgroup.procs").expect("literal");
    let fd = unsafe { libc::openat(cgroup, name.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let text = pid.to_string();
    let written = unsafe { libc::write(fd, text.as_ptr().cast(), text.len()) };
    let error = std::io::Error::last_os_error();
    unsafe { libc::close(fd) };
    if written == text.len() as isize {
        Ok(())
    } else {
        Err(error)
    }
}

// ───────────────────────────── broker plumbing ──────────────────────────────

/// Run the real broker over the real Unix transport in a background thread,
/// exactly as the launcher sidecar's serve loop does.
pub fn serve_broker(
    environment: &Environment,
    context: &RenderedContext,
    worker_pid: i32,
    report: RawFd,
) -> std::thread::JoinHandle<()> {
    let expected_uid = context.u32("launcher_expected_uid");
    let unleased: u16 = context
        .get("unleased_millicores")
        .parse()
        .expect("rendered unleased millicores");
    let leased: u32 = context
        .get("leased_millicores")
        .parse()
        .expect("rendered leased millicores");
    let fs = NativeCgroupFs::open(&environment.root, expected_uid)
        .unwrap_or_else(|e| panic!("delegated cgroup readiness: {e}"));
    let launcher = Launcher::new(
        fs,
        AdversaryClone::new(environment, worker_pid, report),
        LauncherConfig::new(Some(unleased), Some(leased), expected_uid).expect("launcher config"),
    )
    .expect("launcher");
    let broker = Broker::new(
        launcher,
        BrokerConfig::worker(
            u32::try_from(worker_pid).expect("worker pid"),
            environment.credential.clone(),
        )
        .expect("broker config"),
        OsNonceSource,
    )
    .expect("broker");
    let mut server = UnixBrokerServer::bind(&environment.socket, broker).expect("bind broker");
    std::thread::spawn(move || {
        let _ = server.serve_once();
    })
}

// ───────────────────────────────── helpers ──────────────────────────────────

/// One reported adversary attempt: the raw return value and errno.
#[derive(Clone, Copy, Debug)]
pub struct Attempt {
    pub rc: i64,
    pub errno: i32,
}

/// Read the adversary's report lines until its `end` marker. The write end
/// stays open for the test's lifetime, so EOF is never the terminator.
pub fn read_report(read_fd: RawFd) -> BTreeMap<String, Attempt> {
    let mut reader = BufReader::new(unsafe { std::fs::File::from_raw_fd(read_fd) });
    let mut attempts = BTreeMap::new();
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .expect("read the adversary report");
        assert!(
            read > 0,
            "the UID-1001 adversary died before completing its report: {attempts:?}"
        );
        let line = line.trim();
        let (key, value) = line.split_once('=').expect("report line is key=rc:errno");
        if key == "end" {
            return attempts;
        }
        let (rc, errno) = value.split_once(':').expect("report value is rc:errno");
        attempts.insert(
            key.to_owned(),
            Attempt {
                rc: rc.parse().expect("report rc"),
                errno: errno.parse().expect("report errno"),
            },
        );
    }
}

pub fn pipe() -> (RawFd, RawFd) {
    let mut fds = [0; 2];
    assert_eq!(
        unsafe { libc::pipe(fds.as_mut_ptr()) },
        0,
        "create pipe: {}",
        last_error()
    );
    (fds[0], fds[1])
}

pub fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .unwrap_or_else(|e| panic!("set mode {mode:o} on {path:?}: {e}"));
}

pub fn chown(path: &Path, uid: u32, gid: u32) {
    let raw = path_cstring(&path.to_string_lossy());
    assert_eq!(
        unsafe { libc::chown(raw.as_ptr(), uid, gid) },
        0,
        "chown {path:?} to {uid}:{gid}: {}",
        last_error()
    );
}

pub fn random_bytes(count: usize) -> Vec<u8> {
    use std::io::Read;
    let mut bytes = vec![0_u8; count];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .expect("read /dev/urandom");
    bytes
}

/// Fork a child that becomes `(uid, gid)` under umask 0002 and opens `path`
/// with `flags`. Returns the child's exit code; only async-signal-safe libc
/// calls run after the fork.
pub fn run_as(path: &Path, uid: u32, gid: u32, flags: i32) -> i32 {
    let raw = path_cstring(&path.to_string_lossy());
    let payload = b"edit\n";
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork must succeed: {}", last_error());
    if pid == 0 {
        unsafe {
            libc::umask(0o002);
            let group = [gid];
            if libc::setgroups(1, group.as_ptr()) != 0 {
                libc::_exit(101);
            }
            if libc::setresgid(gid, gid, gid) != 0 {
                libc::_exit(102);
            }
            if libc::setresuid(uid, uid, uid) != 0 {
                libc::_exit(103);
            }
            let fd = libc::open(raw.as_ptr(), flags, 0o666);
            if fd < 0 {
                libc::_exit(104);
            }
            let written = libc::write(fd, payload.as_ptr().cast(), payload.len());
            libc::close(fd);
            if written != payload.len() as isize {
                libc::_exit(105);
            }
            libc::_exit(0);
        }
    }
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pid, &mut status, 0) },
        pid,
        "waitpid must reap the child"
    );
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        -1
    }
}

/// A per-test-instance path suffix. Distinct even if a runner executes several
/// proofs inside one process, so two live topologies never collide.
pub fn unique_slot() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

fn path_cstring(value: &str) -> CString {
    CString::new(value).expect("path has an interior NUL")
}

fn errno_value() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn last_error() -> std::io::Error {
    std::io::Error::last_os_error()
}

/// Allocation-free `key=rc:errno` line writer for the post-clone child.
fn emit(fd: RawFd, key: &str, rc: i64) {
    let errno = if rc < 0 { i64::from(errno_value()) } else { 0 };
    let mut buffer = [0_u8; 96];
    let mut length = 0;
    for byte in key.as_bytes() {
        buffer[length] = *byte;
        length += 1;
    }
    buffer[length] = b'=';
    length += 1;
    length += write_int(&mut buffer[length..], rc);
    buffer[length] = b':';
    length += 1;
    length += write_int(&mut buffer[length..], errno);
    buffer[length] = b'\n';
    length += 1;
    unsafe { libc::write(fd, buffer.as_ptr().cast(), length) };
}

fn write_int(out: &mut [u8], value: i64) -> usize {
    let mut digits = [0_u8; 20];
    let negative = value < 0;
    let mut magnitude = value.unsigned_abs();
    let mut count = 0;
    loop {
        digits[count] = b'0' + (magnitude % 10) as u8;
        magnitude /= 10;
        count += 1;
        if magnitude == 0 {
            break;
        }
    }
    let mut written = 0;
    if negative {
        out[0] = b'-';
        written = 1;
    }
    for index in (0..count).rev() {
        out[written] = digits[index];
        written += 1;
    }
    written
}
