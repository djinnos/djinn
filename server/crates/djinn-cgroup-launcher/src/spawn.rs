//! The child-spawn seam: `fork` + a pipe handshake + a `cgroup.procs` write.
//!
//! # Why not `clone3(CLONE_INTO_CGROUP)` (task 7deu, defect 3)
//!
//! The launcher used to be able to start a command exactly one way:
//! `clone3(CLONE_INTO_CGROUP)`. That made a single syscall's availability a
//! hard dependency of the entire feature, and it cost real production time:
//!
//! * The container runtime's `RuntimeDefault` seccomp profile answers `clone3`
//!   with `ENOSYS` in the restricted launcher profile. That made command launch
//!   depend on a syscall the privilege-free sidecar cannot use, and the failure
//!   surfaced as an opaque per-command spawn error.
//! * `libc::CLONE_INTO_CGROUP` is declared `c_int` on glibc with the value
//!   `0x200000000`, which does not fit; the `libc` crate's blanket
//!   `allow(overflowing_literals)` truncated it to **0**. The shipped call
//!   therefore passed no flag at all: an ordinary `fork` whose child stayed in
//!   the launcher's own cgroup, so the delegated leaf and the `cpu.max` written
//!   on it governed nothing. The test suite could not see it because the clone
//!   seam was faked.
//!
//! Neither problem exists here. `fork(2)` and `write(2)` are not intercepted by
//! any seccomp profile in use, there is no flag constant to truncate, and the
//! placement is verified by reading it back (see [`verify_placement`]) rather
//! than assumed. This is also what `runc` itself does.
//!
//! # The no-unthrottled-interval property is preserved
//!
//! goxi correctly insists that a child must never execute work outside its
//! quota. The ordering here is enforced by construction:
//!
//! ```text
//!   parent: fork ─┬─────────────────────────────────────────────────────────
//!                 │                                    child: read(sync) ⏸
//!   parent: write child pid → leaf/cgroup.procs        (blocked, no work)
//!   parent: read back leaf/cgroup.procs, assert pid    (blocked, no work)
//!   parent: write 1 byte → sync ───────────────────────▶ child: resumes
//!                                                       child: prepare_child
//!                                                       child: execve
//! ```
//!
//! Between `fork` and the go byte the child issues exactly one syscall — a
//! blocking `read` on an empty pipe — and burns no CPU. If the parent cannot
//! place it, the sync pipe is closed instead of written: the child observes EOF
//! and `_exit`s **without ever reaching `execve`**.

use std::ffi::CString;
use std::io;
use std::os::fd::RawFd;

use crate::child::{self, ChildMounts};
use crate::{ChildProcess, CommandSpec, Error, Invocation, SpawnIntoCgroup};

/// Byte the parent writes once the child is confirmed inside the leaf.
const GO: u8 = b'G';

/// Exit code for a child that was told to stand down before `execve`.
const EXIT_NOT_PLACED: i32 = 126;
/// Exit code for a child that failed its own pre-exec preparation.
const EXIT_PREPARATION_FAILED: i32 = 127;

/// Safe default seam: refuses every spawn.
///
/// Used where a launcher must exist but must never start a process — the
/// readiness-rejection paths, and tests that assert a spawn is not reached.
pub struct DenySpawn;

impl SpawnIntoCgroup for DenySpawn {
    fn spawn_into_cgroup(
        &mut self,
        _: RawFd,
        _: &Invocation,
        _: &CommandSpec,
    ) -> Result<ChildProcess, Error> {
        Err(Error::SpawnDenied)
    }
}

/// Everything `execve` needs, allocated in the **parent** before `fork`.
///
/// After `fork` in a process that may hold locks from other threads, only
/// async-signal-safe operations are permitted — which excludes every allocation.
/// The previous implementation built its `CString`s in the child; this one does
/// not, so the child path is allocation-free.
struct PreparedExec {
    program: CString,
    cwd: CString,
    dev_null: CString,
    /// Backing storage for `argv`; must outlive the pointer array.
    _argv_storage: Vec<CString>,
    /// Backing storage for `envp`; must outlive the pointer array.
    _envp_storage: Vec<CString>,
    argv: Vec<*mut libc::c_char>,
    envp: Vec<*mut libc::c_char>,
}

impl PreparedExec {
    fn new(command: &CommandSpec) -> Result<Self, Error> {
        let program = CString::new(command.program.as_str()).map_err(|_| Error::InvalidCommand)?;
        let cwd = CString::new(command.cwd.as_str()).map_err(|_| Error::InvalidCommand)?;
        let dev_null = CString::new("/dev/null").map_err(|_| Error::InvalidCommand)?;

        let argv_storage: Vec<CString> = command
            .argv
            .iter()
            .map(|value| CString::new(value.as_str()).map_err(|_| Error::InvalidCommand))
            .collect::<Result<_, _>>()?;
        let envp_storage: Vec<CString> = command
            .environment
            .iter()
            .map(|(key, value)| {
                CString::new(format!("{key}={value}")).map_err(|_| Error::InvalidCommand)
            })
            .collect::<Result<_, _>>()?;

        let mut argv = Vec::with_capacity(argv_storage.len() + 2);
        argv.push(program.as_ptr().cast_mut());
        argv.extend(argv_storage.iter().map(|value| value.as_ptr().cast_mut()));
        argv.push(std::ptr::null_mut());

        let mut envp: Vec<*mut libc::c_char> = envp_storage
            .iter()
            .map(|value| value.as_ptr().cast_mut())
            .collect();
        envp.push(std::ptr::null_mut());

        Ok(Self {
            program,
            cwd,
            dev_null,
            _argv_storage: argv_storage,
            _envp_storage: envp_storage,
            argv,
            envp,
        })
    }
}

/// A pipe pair that closes both ends unless explicitly disarmed.
struct Pipe {
    read: RawFd,
    write: RawFd,
}

impl Pipe {
    fn new() -> Result<Self, Error> {
        let mut fds = [0; 2];
        // `O_CLOEXEC` so a descriptor can never survive into the exec'd image;
        // the child still inherits them across `fork`, which is what we want.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        Ok(Self {
            read: fds[0],
            write: fds[1],
        })
    }

    fn take_read(&mut self) -> RawFd {
        std::mem::replace(&mut self.read, -1)
    }

    fn close_read(&mut self) {
        close_if_open(&mut self.read);
    }

    fn close_write(&mut self) {
        close_if_open(&mut self.write);
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        self.close_read();
        self.close_write();
    }
}

fn close_if_open(fd: &mut RawFd) {
    if *fd >= 0 {
        unsafe { libc::close(*fd) };
        *fd = -1;
    }
}

/// Production spawn seam: the child is placed in `cgroup` and the placement is
/// verified before it is allowed to `execve`.
pub struct NativeCgroupSpawn;

impl SpawnIntoCgroup for NativeCgroupSpawn {
    fn spawn_into_cgroup(
        &mut self,
        cgroup: RawFd,
        _: &Invocation,
        command: &CommandSpec,
    ) -> Result<ChildProcess, Error> {
        command.validate()?;
        let prepared = PreparedExec::new(command)?;

        let mut out = Pipe::new()?;
        let mut err = Pipe::new()?;
        let mut sync = Pipe::new()?;

        // Everything above this line allocates; everything the child touches
        // below it does not.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(Error::SpawnDenied);
        }
        if pid == 0 {
            // SAFETY: post-`fork` child. Only async-signal-safe libc calls and
            // pre-allocated pointers are used, and every path terminates in
            // `_exit` or `execve`.
            unsafe { child_after_fork(&out, &err, &sync, &prepared) };
        }

        // ── Parent ──────────────────────────────────────────────────────────
        out.close_write();
        err.close_write();
        let sync_read = sync.take_read();
        unsafe { libc::close(sync_read) };

        // Place the child, then PROVE it is placed. A write that silently did
        // nothing is exactly the failure mode that shipped last time.
        if let Err(error) = place_and_verify(cgroup, pid) {
            // Closing the sync pipe without writing GO makes the child observe
            // EOF and exit before `execve`: no command runs outside its quota.
            sync.close_write();
            reap(pid);
            return Err(error);
        }

        let go = [GO];
        let written = unsafe { libc::write(sync.write, go.as_ptr().cast(), 1) };
        if written != 1 {
            let error = Error::Io(io::Error::last_os_error());
            sync.close_write();
            reap(pid);
            return Err(error);
        }
        sync.close_write();

        for fd in [out.read, err.read] {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                // The child has already been released, so it is running and
                // owned by nobody unless it is killed here. Returning without
                // this would leak a process inside a leaf the caller is about
                // to try to remove — and `remove` requires `populated 0`, so
                // the leak would strand the reservation too.
                let error = Error::Io(io::Error::last_os_error());
                reap(pid);
                return Err(error);
            }
        }

        Ok(ChildProcess {
            pid,
            stdout: out.take_read(),
            stderr: err.take_read(),
        })
    }
}

/// The post-`fork` child. Never returns.
///
/// # Safety
///
/// Must only be called in a freshly forked child. Every call below is
/// async-signal-safe and every pointer was allocated by the parent.
unsafe fn child_after_fork(out: &Pipe, err: &Pipe, sync: &Pipe, prepared: &PreparedExec) -> ! {
    unsafe {
        libc::close(out.read);
        libc::close(err.read);
        libc::close(sync.write);

        // Stand at the gate. This is the ONLY work the child performs before it
        // is inside the invocation cgroup, and it consumes no CPU.
        let mut byte = [0_u8; 1];
        let read = libc::read(sync.read, byte.as_mut_ptr().cast(), 1);
        if read != 1 || byte[0] != GO {
            // EOF (parent could not place us) or a corrupt handshake.
            libc::_exit(EXIT_NOT_PLACED);
        }
        libc::close(sync.read);

        let stdin = libc::open(prepared.dev_null.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
        if stdin < 0 || libc::dup2(stdin, 0) < 0 {
            libc::_exit(EXIT_PREPARATION_FAILED);
        }
        libc::close(stdin);
        if libc::dup2(out.write, 1) < 0 || libc::dup2(err.write, 2) < 0 {
            libc::_exit(EXIT_PREPARATION_FAILED);
        }
        libc::close(out.write);
        libc::close(err.write);

        // One kernel operation, no procfs iterator: enumerating `/proc/self/fd`
        // owns a descriptor that the enumeration itself reports and then closes,
        // leaving a stale fd for `prepare_child` to fail on.
        if libc::syscall(libc::SYS_close_range, 3_u32, u32::MAX, 0_u32) != 0 {
            libc::_exit(EXIT_PREPARATION_FAILED);
        }

        let mut syscalls = child::NativeChildSyscalls;
        if child::prepare_child(&mut syscalls, &[], &ChildMounts::isolated()).is_err() {
            libc::_exit(EXIT_PREPARATION_FAILED);
        }
        if libc::chdir(prepared.cwd.as_ptr()) != 0 {
            libc::_exit(EXIT_PREPARATION_FAILED);
        }
        libc::execve(
            prepared.program.as_ptr(),
            prepared.argv.as_ptr().cast(),
            prepared.envp.as_ptr().cast(),
        );
        libc::_exit(EXIT_PREPARATION_FAILED)
    }
}

/// Write `pid` into `cgroup`'s `cgroup.procs`, then read it back and confirm.
///
/// The read-back is the point. A `cpu.max` written on a leaf the child is not
/// actually in governs nothing, and that failure is invisible from the leaf's
/// configuration — it only shows up as a build that is mysteriously not
/// throttled. Verifying membership makes the silent variant impossible.
fn place_and_verify(cgroup: RawFd, pid: i32) -> Result<(), Error> {
    write_cgroup_procs(cgroup, pid)?;
    verify_placement(cgroup, pid)
}

fn write_cgroup_procs(cgroup: RawFd, pid: i32) -> Result<(), Error> {
    let name = CString::new("cgroup.procs").map_err(|_| Error::UnsafeLeafName)?;
    let fd = unsafe {
        libc::openat(
            cgroup,
            name.as_ptr(),
            libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(Error::CgroupPlacementFailed {
            pid,
            errno: errno(),
        });
    }
    let text = pid.to_string();
    let bytes = text.as_bytes();
    let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    let error = errno();
    unsafe { libc::close(fd) };
    if written != bytes.len() as isize {
        return Err(Error::CgroupPlacementFailed { pid, errno: error });
    }
    Ok(())
}

/// Read `cgroup.procs` back and require `pid` to be a member.
fn verify_placement(cgroup: RawFd, pid: i32) -> Result<(), Error> {
    let name = CString::new("cgroup.procs").map_err(|_| Error::UnsafeLeafName)?;
    let fd = unsafe {
        libc::openat(
            cgroup,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(Error::CgroupPlacementFailed {
            pid,
            errno: errno(),
        });
    }
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let count = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
        if count < 0 {
            let error = errno();
            unsafe { libc::close(fd) };
            return Err(Error::CgroupPlacementFailed { pid, errno: error });
        }
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..count as usize]);
    }
    unsafe { libc::close(fd) };

    let member = String::from_utf8_lossy(&bytes)
        .split_ascii_whitespace()
        .any(|entry| entry.parse::<i32>() == Ok(pid));
    if member {
        Ok(())
    } else {
        Err(Error::ChildNotInInvocationCgroup { pid })
    }
}

/// Reap a child that will never exec. Best effort: a child blocked on the sync
/// pipe exits as soon as it sees EOF, so this returns promptly.
fn reap(pid: i32) {
    unsafe {
        libc::kill(pid, libc::SIGKILL);
        let mut status = 0;
        libc::waitpid(pid, &raw mut status, 0);
    }
}

fn errno() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn command(program: &str) -> CommandSpec {
        CommandSpec {
            program: program.to_owned(),
            argv: vec![],
            cwd: "/workspace".to_owned(),
            environment: vec![],
        }
    }

    fn invocation() -> Invocation {
        Invocation {
            id: "spawn-test".to_owned(),
            fence: 1,
        }
    }

    #[test]
    fn the_deny_seam_never_starts_a_process() {
        assert!(matches!(
            DenySpawn.spawn_into_cgroup(3, &invocation(), &command("/bin/true")),
            Err(Error::SpawnDenied)
        ));
    }

    /// A cgroup descriptor that cannot accept a `cgroup.procs` write must fail
    /// the spawn **and** leave no process behind, because the child stands at
    /// the gate until the parent has proven placement.
    ///
    /// This is the negative control for the whole seam: if placement were
    /// assumed rather than verified, this would return a running child.
    #[test]
    fn an_unplaceable_child_never_execs_and_leaves_no_process() {
        // A real directory that is emphatically not a cgroup: `openat` of
        // `cgroup.procs` inside it fails with ENOENT.
        let base = std::env::var_os("CARGO_TARGET_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("djinn-spawn-unplaceable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("create scratch dir");
        let raw = CString::new(base.to_string_lossy().as_bytes()).expect("path");
        let dir = unsafe {
            libc::open(
                raw.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        assert!(dir >= 0, "open scratch dir");

        // `/bin/true` would exit 0 instantly if it ever ran; it must not run.
        let result = NativeCgroupSpawn.spawn_into_cgroup(dir, &invocation(), &command("/bin/true"));
        unsafe { libc::close(dir) };
        let _ = std::fs::remove_dir_all(&base);

        match result {
            Err(Error::CgroupPlacementFailed { errno, .. }) => {
                assert_eq!(
                    errno,
                    libc::ENOENT,
                    "a non-cgroup directory has no cgroup.procs"
                );
            }
            Err(other) => panic!("expected a named placement failure, got: {other}"),
            Ok(child) => {
                reap(child.pid);
                panic!(
                    "a child was started into a cgroup it could not be placed in; the \
                     no-unthrottled-interval property is broken"
                );
            }
        }
    }

    #[test]
    fn a_malformed_command_is_rejected_before_any_fork() {
        let mut malformed = command("/bin/true");
        malformed.program = "/bin/../sh".to_owned();
        assert!(matches!(
            NativeCgroupSpawn.spawn_into_cgroup(-1, &invocation(), &malformed),
            Err(Error::InvalidCommand)
        ));
    }

    /// Every `execve` argument is built before `fork`, so the child path
    /// performs no allocation. Asserted on the prepared block itself because a
    /// post-fork allocation is a deadlock, not a test failure.
    #[test]
    fn exec_arguments_are_fully_prepared_in_the_parent() {
        let spec = CommandSpec {
            program: "/bin/echo".to_owned(),
            argv: vec!["one".to_owned(), "two".to_owned()],
            cwd: "/workspace".to_owned(),
            environment: vec![("PATH".to_owned(), "/usr/bin".to_owned())],
        };
        let prepared = PreparedExec::new(&spec).expect("prepare");
        assert_eq!(prepared.program.to_str().unwrap(), "/bin/echo");
        assert_eq!(prepared.cwd.to_str().unwrap(), "/workspace");
        // argv0 + two arguments + NULL.
        assert_eq!(prepared.argv.len(), 4);
        assert!(prepared.argv.last().unwrap().is_null());
        // one entry + NULL.
        assert_eq!(prepared.envp.len(), 2);
        assert!(prepared.envp.last().unwrap().is_null());
    }
}
