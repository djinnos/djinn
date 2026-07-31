//! Bounded Unix socket transport for authenticated broker controls.
use crate::{
    CgroupFs, CommandSpec, ControlRejection, CpuStat, Error, Invocation, LeaseAuthority,
    SpawnIntoCgroup,
    broker::{Broker, ConnectionId, ControlNonce, NonceSource, SocketPeer, WORKER_GID, WORKER_UID},
    child::WorkerReadinessAssertion,
};
use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{FileTypeExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
};
const MAX: usize = 65536;
const AUTH: u8 = 1;
const READY: u8 = 2;
const BEGIN: u8 = 3;
const CREATE: u8 = 4;
const SAMPLE: u8 = 5;
const LIFT: u8 = 6;
const KILL: u8 = 7;
const WAIT: u8 = 8;
const CLEAN: u8 = 9;
const STDOUT: u8 = 10;
const STDERR: u8 = 11;
pub struct UnixBrokerServer<F, S, N = crate::broker::OsNonceSource> {
    broker: Broker<F, S, N>,
    listener: Option<UnixListener>,
    socket_path: Option<PathBuf>,
}
impl<F: CgroupFs, S: SpawnIntoCgroup, N: NonceSource> UnixBrokerServer<F, S, N> {
    pub fn new(broker: Broker<F, S, N>) -> Self {
        Self {
            broker,
            listener: None,
            socket_path: None,
        }
    }

    /// Bind a private, filesystem-backed control socket. A live or non-socket
    /// path is never replaced; a stale socket is removed before binding.
    pub fn bind(path: impl AsRef<Path>, broker: Broker<F, S, N>) -> Result<Self, Error> {
        let path = path.as_ref().to_path_buf();
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            if !metadata.file_type().is_socket() {
                return Err(Error::UnsafeSocketPath);
            }
            if UnixStream::connect(&path).is_ok() {
                return Err(Error::UnsafeSocketPath);
            }
            fs::remove_file(&path)?;
        }
        let listener = UnixListener::bind(&path).map_err(|error| Error::SocketSetupFailed {
            operation: "bind(broker control socket)",
            path: path.display().to_string(),
            errno: error.raw_os_error().unwrap_or_default(),
        })?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(|error| {
            Error::SocketSetupFailed {
                operation: "chmod(broker socket to 0600)",
                path: path.display().to_string(),
                errno: error.raw_os_error().unwrap_or_default(),
            }
        })?;
        restrict_socket_to_worker(&path)?;
        Ok(Self {
            broker,
            listener: Some(listener),
            socket_path: Some(path),
        })
    }

    pub fn socket_path(&self) -> Option<&Path> {
        self.socket_path.as_deref()
    }

    /// Serve exactly one accepted connection, keeping acceptance bounded for
    /// the embedding privileged process.
    pub fn serve_once(&mut self) -> Result<(), Error> {
        let listener = self.listener.as_ref().ok_or(Error::UnsafeSocketPath)?;
        let (stream, _) = listener.accept()?;
        self.serve_connection(stream)
    }

    pub fn serve_connection(&mut self, mut s: UnixStream) -> Result<(), Error> {
        let a = read(&mut s)?;
        if a.first() != Some(&AUTH) {
            return Err(Error::InvalidTransportFrame);
        }
        let c = self
            .broker
            .authenticate(&SocketPeer(s.as_raw_fd()), &a[1..])?;
        reply(&mut s, Ok(vec![]))?;
        loop {
            let r = match read(&mut s) {
                Ok(v) => v,
                Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e) => return Err(e),
            };
            let outcome = self.dispatch(c, &r);
            // A protocol disagreement ends the CONNECTION, not just the frame.
            // The categorized refusal is still written first — the worker must
            // be able to log why it was dropped — but the serve loop then
            // returns, so no BEGIN or CREATE can follow on this socket even if
            // the peer ignores the refusal and keeps writing frames.
            let terminal = match &outcome {
                Err(Error::ProtocolMismatch { launcher, worker }) => Some((*launcher, *worker)),
                _ => None,
            };
            reply(&mut s, outcome)?;
            if let Some((launcher, worker)) = terminal {
                return Err(Error::ProtocolMismatch { launcher, worker });
            }
        }
    }
    fn dispatch(&mut self, c: ConnectionId, r: &[u8]) -> Result<Vec<u8>, Error> {
        let (&t, b) = r.split_first().ok_or(Error::InvalidTransportFrame)?;
        match t {
            READY => {
                self.broker
                    .accept_worker_readiness(c, WorkerReadinessAssertion::from_wire(b)?)?;
                Ok(vec![])
            }
            BEGIN => {
                let (i, f) = begin(b)?;
                Ok(self
                    .broker
                    .begin_invocation(c, Invocation { id: i, fence: f })?
                    .as_bytes()
                    .to_vec())
            }
            CREATE => {
                let (i, n, x) = control(b)?;
                let (leaf, authority, command) = command_in(x)?;
                Ok(self
                    .broker
                    .create(c, &i, n, &leaf, authority, &command)?
                    .as_bytes()
                    .to_vec())
            }
            STDOUT | STDERR => {
                let (i, n, x) = control(b)?;
                if !x.is_empty() {
                    return Err(Error::InvalidTransportFrame);
                }
                let (bytes, eof, status, nonce) = self.broker.output(c, &i, n, t == STDERR)?;
                let mut out = vec![
                    u8::from(eof),
                    match status {
                        crate::broker::ChildStatus::Running => 0,
                        crate::broker::ChildStatus::Exited(_) => 1,
                        crate::broker::ChildStatus::Signaled(_) => 2,
                    },
                    match status {
                        crate::broker::ChildStatus::Running => 0,
                        crate::broker::ChildStatus::Exited(v)
                        | crate::broker::ChildStatus::Signaled(v) => v,
                    },
                ];
                out.extend(enc_bytes(&bytes)?);
                out.extend(nonce.as_bytes());
                Ok(out)
            }
            SAMPLE => {
                let (i, n, x) = control(b)?;
                if !x.is_empty() {
                    return Err(Error::InvalidTransportFrame);
                }
                let (q, n) = self.broker.sample(c, &i, n)?;
                let mut o = cpu_out(q);
                o.extend(n.as_bytes());
                Ok(o)
            }
            LIFT => {
                let (i, n, x) = control(b)?;
                Ok(self.broker.lift(c, &i, n, u64v(x)?)?.as_bytes().to_vec())
            }
            KILL | WAIT => {
                let (i, n, x) = control(b)?;
                if !x.is_empty() {
                    return Err(Error::InvalidTransportFrame);
                }
                let n = if t == KILL {
                    self.broker.kill(c, &i, n)?
                } else {
                    self.broker.wait_empty(c, &i, n)?
                };
                Ok(n.as_bytes().to_vec())
            }
            CLEAN => {
                let (i, n, x) = control(b)?;
                if !x.is_empty() {
                    return Err(Error::InvalidTransportFrame);
                }
                self.broker.cleanup(c, &i, n)?;
                Ok(vec![])
            }
            _ => Err(Error::InvalidTransportFrame),
        }
    }
}
impl<F, S, N> Drop for UnixBrokerServer<F, S, N> {
    fn drop(&mut self) {
        if let Some(path) = self.socket_path.take() {
            let _ = fs::remove_file(path);
        }
    }
}

/// Hand the 0600 control socket to the worker identity when the broker runs
/// privileged.
///
/// `connect(2)` on a filesystem-backed Unix socket requires WRITE permission on
/// the socket inode. In the rendered Pod the launcher is uid 0 and the worker is
/// [`WORKER_UID`], so a root-owned 0600 socket is unreachable by the very peer
/// the broker exists to serve — while a laxer mode would open it to the
/// launcher-spawned child (uid 1001). Owner-only, worker-owned is the single
/// mode that admits exactly the worker: the child is refused by the kernel at
/// `connect`, before a byte of the protocol is read. Proven by
/// `tests/kernel_boundary_under_rendered_context.rs`.
///
/// Unprivileged embeddings (tests, local runs) already own the socket they just
/// created, so there is nothing to hand over and this is a no-op.
/// Hand the bound control socket to the worker.
///
/// Mode `0600` plus `WORKER_UID` ownership is what excludes the
/// launcher-spawned child (uid 1001) from the broker at the filesystem layer,
/// ahead of credential authentication.
///
/// `chown(2)` needs `CAP_CHOWN` even at euid 0 — the kernel checks the
/// capability, not the uid — so the launcher retains it past bootstrap
/// (`bootstrap::RETAINED_CAPABILITIES`). When that capability was missing this
/// returned a bare `EPERM` with no operation or path attached, and the resulting
/// "filesystem operation failed: Operation not permitted" was the entire
/// diagnostic available for a failed production rollout. It is named now.
fn restrict_socket_to_worker(path: &Path) -> Result<(), Error> {
    if unsafe { libc::geteuid() } != 0 {
        return Ok(());
    }
    let raw = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| Error::UnsafeSocketPath)?;
    if unsafe { libc::chown(raw.as_ptr(), WORKER_UID, WORKER_GID) } != 0 {
        return Err(Error::SocketSetupFailed {
            operation: "chown(broker socket to the worker uid/gid)",
            path: path.display().to_string(),
            errno: std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or_default(),
        });
    }
    Ok(())
}

pub struct UnixBrokerClient {
    stream: UnixStream,
    nonces: HashMap<String, ControlNonce>,
}
impl UnixBrokerClient {
    pub fn connect_path(path: impl AsRef<Path>, cred: &[u8]) -> Result<Self, Error> {
        Self::connect(UnixStream::connect(path)?, cred)
    }

    pub fn connect(mut s: UnixStream, cred: &[u8]) -> Result<Self, Error> {
        let mut x = vec![AUTH];
        x.extend(cred);
        write(&mut s, &x)?;
        response(&mut s)?;
        Ok(Self {
            stream: s,
            nonces: HashMap::new(),
        })
    }
    pub fn ready(&mut self, assertion: WorkerReadinessAssertion) -> Result<(), Error> {
        self.call(READY, &assertion.wire_bytes()).map(|_| ())
    }
    pub fn begin(&mut self, i: Invocation) -> Result<(), Error> {
        let mut x = enc(&i.id)?;
        x.extend(i.fence.to_be_bytes());
        let n = nonce(&self.call(BEGIN, &x)?)?;
        self.nonces.insert(i.id, n);
        Ok(())
    }
    pub fn create(
        &mut self,
        i: &str,
        leaf: &str,
        authority: LeaseAuthority,
        command: &CommandSpec,
    ) -> Result<(), Error> {
        let mut x = self.control(i)?;
        x.extend(command_out(leaf, authority, command)?);
        let r = self.call(CREATE, &x)?;
        self.rotate(i, &r)
    }
    pub fn stdout(
        &mut self,
        i: &str,
    ) -> Result<(Vec<u8>, bool, crate::broker::ChildStatus), Error> {
        self.output(STDOUT, i)
    }
    pub fn stderr(
        &mut self,
        i: &str,
    ) -> Result<(Vec<u8>, bool, crate::broker::ChildStatus), Error> {
        self.output(STDERR, i)
    }
    fn output(
        &mut self,
        kind: u8,
        i: &str,
    ) -> Result<(Vec<u8>, bool, crate::broker::ChildStatus), Error> {
        let x = self.control(i)?;
        let response = self.call(kind, &x)?;
        if response.len() < 35 {
            return Err(Error::InvalidTransportFrame);
        }
        let eof = response[0] != 0;
        let status = match response[1] {
            0 => crate::broker::ChildStatus::Running,
            1 => crate::broker::ChildStatus::Exited(response[2]),
            2 => crate::broker::ChildStatus::Signaled(response[2]),
            _ => return Err(Error::InvalidTransportFrame),
        };
        let bytes = bytes_in(&response[3..response.len() - 32])?;
        self.rotate(i, &response[response.len() - 32..])?;
        Ok((bytes, eof, status))
    }
    pub fn sample(&mut self, i: &str) -> Result<CpuStat, Error> {
        let x = self.control(i)?;
        let r = self.call(SAMPLE, &x)?;
        if r.len() != 80 {
            return Err(Error::InvalidTransportFrame);
        }
        let q = cpu_in(&r[..48])?;
        self.rotate(i, &r[48..])?;
        Ok(q)
    }
    pub fn lift(&mut self, i: &str, f: u64) -> Result<(), Error> {
        let mut x = self.control(i)?;
        x.extend(f.to_be_bytes());
        let r = self.call(LIFT, &x)?;
        self.rotate(i, &r)
    }
    pub fn kill(&mut self, i: &str) -> Result<(), Error> {
        let x = self.control(i)?;
        let r = self.call(KILL, &x)?;
        self.rotate(i, &r)
    }
    pub fn wait_empty(&mut self, i: &str) -> Result<(), Error> {
        let x = self.control(i)?;
        let r = self.call(WAIT, &x)?;
        self.rotate(i, &r)
    }
    pub fn cleanup(&mut self, i: &str) -> Result<(), Error> {
        let x = self.control(i)?;
        self.call(CLEAN, &x)?;
        self.nonces.remove(i);
        Ok(())
    }
    fn call(&mut self, t: u8, b: &[u8]) -> Result<Vec<u8>, Error> {
        let mut x = vec![t];
        x.extend(b);
        write(&mut self.stream, &x)?;
        response(&mut self.stream)
    }
    fn control(&self, i: &str) -> Result<Vec<u8>, Error> {
        let mut x = enc(i)?;
        x.extend(
            self.nonces
                .get(i)
                .ok_or(Error::InvalidInvocationBinding)?
                .as_bytes(),
        );
        Ok(x)
    }
    fn rotate(&mut self, i: &str, r: &[u8]) -> Result<(), Error> {
        self.nonces.insert(i.into(), nonce(r)?);
        Ok(())
    }
}
fn enc_bytes(bytes: &[u8]) -> Result<Vec<u8>, Error> {
    let n = u16::try_from(bytes.len()).map_err(|_| Error::InvalidTransportFrame)?;
    let mut out = n.to_be_bytes().to_vec();
    out.extend(bytes);
    Ok(out)
}
fn bytes_in(input: &[u8]) -> Result<Vec<u8>, Error> {
    if input.len() < 2 {
        return Err(Error::InvalidTransportFrame);
    }
    let n = u16::from_be_bytes([input[0], input[1]]) as usize;
    if input.len() != n + 2 {
        return Err(Error::InvalidTransportFrame);
    }
    Ok(input[2..].to_vec())
}
fn command_out(
    leaf: &str,
    authority: LeaseAuthority,
    command: &CommandSpec,
) -> Result<Vec<u8>, Error> {
    command.validate()?;
    let mut out = enc(leaf)?;
    // The authority travels WITH the create, not as a separate control: the
    // birth quota is chosen inside the same broker call that makes the leaf, so
    // there is no window in which a leaf exists without a decided ceiling.
    out.push(authority.wire());
    for item in [&command.program, &command.cwd] {
        out.extend(enc(item)?);
    }
    out.push(u8::try_from(command.argv.len()).map_err(|_| Error::InvalidTransportFrame)?);
    for arg in &command.argv {
        out.extend(enc(arg)?);
    }
    out.push(u8::try_from(command.environment.len()).map_err(|_| Error::InvalidTransportFrame)?);
    for (key, value) in &command.environment {
        out.extend(enc(key)?);
        out.extend(enc(value)?);
    }
    Ok(out)
}
fn command_in(input: &[u8]) -> Result<(String, LeaseAuthority, CommandSpec), Error> {
    let (leaf, rest) = take_string(input)?;
    let (&authority, mut rest) = rest.split_first().ok_or(Error::InvalidTransportFrame)?;
    let authority = LeaseAuthority::from_wire(authority)?;
    let (program, next) = take_string(rest)?;
    rest = next;
    let (cwd, next) = take_string(rest)?;
    rest = next;
    let (&argc, mut rest) = rest.split_first().ok_or(Error::InvalidTransportFrame)?;
    let mut argv = Vec::with_capacity(argc as usize);
    for _ in 0..argc {
        let (value, next) = take_string(rest)?;
        argv.push(value);
        rest = next;
    }
    let (&envc, mut rest) = rest.split_first().ok_or(Error::InvalidTransportFrame)?;
    let mut environment = Vec::with_capacity(envc as usize);
    for _ in 0..envc {
        let (key, next) = take_string(rest)?;
        let (value, next) = take_string(next)?;
        environment.push((key, value));
        rest = next;
    }
    if !rest.is_empty() {
        return Err(Error::InvalidTransportFrame);
    }
    let command = CommandSpec {
        program,
        argv,
        cwd,
        environment,
    };
    command.validate()?;
    Ok((leaf, authority, command))
}
fn take_string(input: &[u8]) -> Result<(String, &[u8]), Error> {
    if input.len() < 2 {
        return Err(Error::InvalidTransportFrame);
    }
    let n = u16::from_be_bytes([input[0], input[1]]) as usize;
    if input.len() < n + 2 {
        return Err(Error::InvalidTransportFrame);
    }
    let value = std::str::from_utf8(&input[2..n + 2])
        .map_err(|_| Error::InvalidTransportFrame)?
        .to_owned();
    Ok((value, &input[n + 2..]))
}

fn read(s: &mut UnixStream) -> Result<Vec<u8>, Error> {
    let mut n = [0; 4];
    s.read_exact(&mut n)?;
    let n = u32::from_be_bytes(n) as usize;
    if n == 0 || n > MAX {
        return Err(Error::InvalidTransportFrame);
    }
    let mut x = vec![0; n];
    s.read_exact(&mut x)?;
    Ok(x)
}
fn write(s: &mut UnixStream, x: &[u8]) -> Result<(), Error> {
    if x.is_empty() || x.len() > MAX {
        return Err(Error::InvalidTransportFrame);
    }
    s.write_all(&(x.len() as u32).to_be_bytes())?;
    s.write_all(x)?;
    Ok(())
}
/// Answer one control. A refusal carries its bounded [`ControlRejection`]
/// category and nothing else — never the error's message, path, errno or the
/// rejected bytes.
fn reply(s: &mut UnixStream, r: Result<Vec<u8>, Error>) -> Result<(), Error> {
    match r {
        Ok(x) => {
            let mut y = vec![0];
            y.extend(x);
            write(s, &y)
        }
        Err(error) => write(s, &[1, ControlRejection::of(&error).code()]),
    }
}
fn response(s: &mut UnixStream) -> Result<Vec<u8>, Error> {
    let x = read(s)?;
    match x.split_first() {
        Some((0, x)) => Ok(x.to_vec()),
        // A refusal from a peer that predates the reason byte still decodes, as
        // `Unspecified`: a rolling update must not turn a legible refusal into
        // an illegible frame error.
        Some((1, reason)) => Err(Error::ControlRejected(ControlRejection::from_code(
            reason.first().copied().unwrap_or_default(),
        ))),
        _ => Err(Error::InvalidTransportFrame),
    }
}
fn enc(s: &str) -> Result<Vec<u8>, Error> {
    let n = u16::try_from(s.len()).map_err(|_| Error::InvalidTransportFrame)?;
    let mut x = n.to_be_bytes().to_vec();
    x.extend(s.as_bytes());
    Ok(x)
}
fn begin(x: &[u8]) -> Result<(String, u64), Error> {
    if x.len() < 10 {
        return Err(Error::InvalidTransportFrame);
    }
    let n = u16::from_be_bytes([x[0], x[1]]) as usize;
    if x.len() != n + 10 {
        return Err(Error::InvalidTransportFrame);
    }
    Ok((
        std::str::from_utf8(&x[2..n + 2])
            .map_err(|_| Error::InvalidTransportFrame)?
            .into(),
        u64v(&x[n + 2..])?,
    ))
}
fn control(x: &[u8]) -> Result<(String, ControlNonce, &[u8]), Error> {
    if x.len() < 34 {
        return Err(Error::InvalidTransportFrame);
    }
    let n = u16::from_be_bytes([x[0], x[1]]) as usize;
    if x.len() < n + 34 {
        return Err(Error::InvalidTransportFrame);
    }
    Ok((
        std::str::from_utf8(&x[2..n + 2])
            .map_err(|_| Error::InvalidTransportFrame)?
            .into(),
        nonce(&x[n + 2..n + 34])?,
        &x[n + 34..],
    ))
}
fn nonce(x: &[u8]) -> Result<ControlNonce, Error> {
    if x.len() != 32 {
        return Err(Error::InvalidTransportFrame);
    }
    let mut n = [0; 32];
    n.copy_from_slice(x);
    Ok(ControlNonce::from_bytes(n))
}
fn u64v(x: &[u8]) -> Result<u64, Error> {
    Ok(u64::from_be_bytes(
        x.try_into().map_err(|_| Error::InvalidTransportFrame)?,
    ))
}
fn cpu_out(c: CpuStat) -> Vec<u8> {
    [
        c.usage_usec,
        c.user_usec,
        c.system_usec,
        c.nr_periods,
        c.nr_throttled,
        c.throttled_usec,
    ]
    .into_iter()
    .flat_map(u64::to_be_bytes)
    .collect()
}
fn cpu_in(x: &[u8]) -> Result<CpuStat, Error> {
    if x.len() != 48 {
        return Err(Error::InvalidTransportFrame);
    }
    let f = |i| u64v(&x[i..i + 8]);
    Ok(CpuStat {
        usage_usec: f(0)?,
        user_usec: f(8)?,
        system_usec: f(16)?,
        nr_periods: f(24)?,
        nr_throttled: f(32)?,
        throttled_usec: f(40)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CgroupMode, ChildProcess, Invocation, Launcher, LauncherConfig, Readiness, SpawnIntoCgroup,
        broker::{BrokerConfig, NonceSource},
    };
    use std::{
        collections::BTreeSet,
        os::fd::RawFd,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
    };

    #[derive(Clone)]
    struct Counts {
        leaves: Arc<AtomicUsize>,
        clones: Arc<AtomicUsize>,
        pipe_capacity: Arc<AtomicUsize>,
        output_bytes: Arc<AtomicUsize>,
    }
    impl Counts {
        fn new() -> Self {
            Self {
                leaves: Arc::new(AtomicUsize::new(0)),
                clones: Arc::new(AtomicUsize::new(0)),
                pipe_capacity: Arc::new(AtomicUsize::new(0)),
                output_bytes: Arc::new(AtomicUsize::new(0)),
            }
        }
    }
    struct FakeFs(Counts);
    impl CgroupFs for FakeFs {
        fn readiness(&self) -> Result<Readiness, Error> {
            Ok(Readiness {
                mode: CgroupMode::V2,
                root_writable: true,
                owner_uid: unsafe { libc::geteuid() },
                delegated_controllers: BTreeSet::from(["cpu".into()]),
            })
        }
        fn create_direct_child(&mut self, _: &str) -> Result<RawFd, Error> {
            self.0.leaves.fetch_add(1, Ordering::SeqCst);
            Ok(42)
        }
        fn write_leaf(&mut self, _: RawFd, _: &str, _: &str) -> Result<(), Error> {
            Ok(())
        }
        fn read_leaf(&mut self, _: RawFd, file: &str) -> Result<String, Error> {
            Ok(if file == "cgroup.events" {
                "populated 0".into()
            } else {
                "usage_usec 0".into()
            })
        }
        fn remove_leaf(&mut self, _: RawFd, _: &str) -> Result<(), Error> {
            Ok(())
        }
    }
    #[derive(Clone, Copy)]
    enum Outcome {
        Exit(u8),
        Signal,
    }
    struct ForkClone {
        counts: Counts,
        outcome: Outcome,
    }
    impl SpawnIntoCgroup for ForkClone {
        fn spawn_into_cgroup(
            &mut self,
            _: RawFd,
            _: &Invocation,
            _: &CommandSpec,
        ) -> Result<ChildProcess, Error> {
            self.counts.clones.fetch_add(1, Ordering::SeqCst);
            let mut out = [0; 2];
            let mut err = [0; 2];
            assert_eq!(unsafe { libc::pipe(out.as_mut_ptr()) }, 0);
            assert_eq!(unsafe { libc::pipe(err.as_mut_ptr()) }, 0);
            // Do not assume a host's default pipe size: the test writes one
            // byte more than the actual kernel-reported capacity.
            let pipe_capacity = unsafe { libc::fcntl(out[1], libc::F_GETPIPE_SZ) };
            assert!(pipe_capacity > 0);
            let output_bytes = pipe_capacity as usize + 1;
            self.counts
                .pipe_capacity
                .store(pipe_capacity as usize, Ordering::SeqCst);
            self.counts
                .output_bytes
                .store(output_bytes, Ordering::SeqCst);
            let mut progress = [0; 2];
            assert_eq!(unsafe { libc::pipe(progress.as_mut_ptr()) }, 0);
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0);
            if pid == 0 {
                unsafe {
                    libc::close(out[0]);
                    libc::close(err[0]);
                    libc::close(progress[0]);
                    // Fill the pipe completely, prove that a further write is
                    // backpressured before the broker can serve CREATE, then
                    // make that write blocking until bounded frames drain it.
                    let bytes = vec![b'o'; output_bytes];
                    let flags = libc::fcntl(out[1], libc::F_GETFL);
                    assert!(flags >= 0);
                    assert_eq!(
                        libc::fcntl(out[1], libc::F_SETFL, flags | libc::O_NONBLOCK),
                        0
                    );
                    assert_eq!(
                        libc::write(out[1], bytes.as_ptr().cast(), pipe_capacity as usize),
                        pipe_capacity as isize
                    );
                    assert_eq!(
                        libc::write(out[1], bytes[pipe_capacity as usize..].as_ptr().cast(), 1),
                        -1
                    );
                    assert_eq!(
                        std::io::Error::last_os_error().raw_os_error(),
                        Some(libc::EAGAIN)
                    );
                    assert_eq!(libc::write(progress[1], b"B".as_ptr().cast(), 1), 1);
                    libc::close(progress[1]);
                    assert_eq!(libc::fcntl(out[1], libc::F_SETFL, flags), 0);
                    assert_eq!(
                        libc::write(out[1], bytes[pipe_capacity as usize..].as_ptr().cast(), 1),
                        1
                    );
                    assert_eq!(
                        libc::write(err[1], b"separate-stderr".as_ptr().cast(), 15),
                        15
                    );
                    libc::close(out[1]);
                    libc::close(err[1]);
                    match self.outcome {
                        Outcome::Exit(code) => libc::_exit(code.into()),
                        Outcome::Signal => {
                            libc::raise(libc::SIGTERM);
                            libc::_exit(127)
                        }
                    }
                }
            }
            unsafe {
                libc::close(out[1]);
                libc::close(err[1]);
                libc::close(progress[1]);
                let mut backpressured = 0;
                assert_eq!(
                    libc::read(progress[0], (&mut backpressured as *mut u8).cast(), 1),
                    1
                );
                assert_eq!(backpressured, b'B');
                libc::close(progress[0]);
                for fd in [out[0], err[0]] {
                    let flags = libc::fcntl(fd, libc::F_GETFL);
                    assert!(flags >= 0);
                    assert_eq!(libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK), 0);
                }
            }
            Ok(ChildProcess {
                pid,
                stdout: out[0],
                stderr: err[0],
            })
        }
    }
    struct NoClone(Counts);
    impl SpawnIntoCgroup for NoClone {
        fn spawn_into_cgroup(
            &mut self,
            _: RawFd,
            _: &Invocation,
            _: &CommandSpec,
        ) -> Result<ChildProcess, Error> {
            self.0.clones.fetch_add(1, Ordering::SeqCst);
            Err(Error::SpawnDenied)
        }
    }
    struct TestNonces(u8);
    impl NonceSource for TestNonces {
        fn nonce(&mut self) -> Result<crate::broker::ControlNonce, Error> {
            self.0 = self.0.wrapping_add(1);
            Ok(crate::broker::ControlNonce::from_bytes([self.0; 32]))
        }
    }
    fn broker<S: SpawnIntoCgroup>(counts: Counts, clone: S) -> Broker<FakeFs, S, TestNonces> {
        broker_running(counts, clone, crate::LauncherAuthorityProtocol::LeafV1)
    }
    fn broker_running<S: SpawnIntoCgroup>(
        counts: Counts,
        clone: S,
        protocol: crate::LauncherAuthorityProtocol,
    ) -> Broker<FakeFs, S, TestNonces> {
        let launcher = Launcher::new(
            FakeFs(counts),
            clone,
            LauncherConfig::new(None, None, unsafe { libc::geteuid() })
                .unwrap()
                .with_authority_protocol(protocol),
        )
        .unwrap();
        Broker::new(
            launcher,
            BrokerConfig {
                worker_pid: unsafe { libc::getpid() as u32 },
                worker_uid: unsafe { libc::geteuid() },
                worker_gid: unsafe { libc::getegid() },
                pod_credential: b"test-credential".to_vec(),
            },
            TestNonces(0),
        )
        .unwrap()
    }
    fn command() -> CommandSpec {
        CommandSpec {
            program: "/bin/true".into(),
            argv: vec![],
            cwd: "/workspace".into(),
            environment: vec![],
        }
    }
    struct ReadyDumpability;
    impl crate::child::WorkerDumpability for ReadyDumpability {
        fn set_non_dumpable(&mut self) -> Result<(), Error> {
            Ok(())
        }
        fn get_dumpable(&mut self) -> Result<i32, Error> {
            Ok(0)
        }
    }
    fn ready(client: &mut UnixBrokerClient) {
        client
            .ready(
                crate::child::prepare_worker_readiness(
                    &mut ReadyDumpability,
                    crate::LauncherAuthorityProtocol::LeafV1,
                )
                .unwrap(),
            )
            .unwrap();
    }

    #[test]
    fn bounded_frame_reader_rejects_zero_oversized_and_truncated_frames() {
        for header in [[0, 0, 0, 0], [0, 1, 0, 1], [0, 0, 0, 4]] {
            let (mut writer, mut reader) = UnixStream::pair().unwrap();
            writer.write_all(&header).unwrap();
            if header == [0, 0, 0, 4] {
                writer.write_all(&[AUTH]).unwrap();
            }
            drop(writer);
            assert!(read(&mut reader).is_err());
        }
    }

    #[test]
    fn malformed_control_payloads_and_responses_fail_closed() {
        assert!(begin(&[0, 1, b'x']).is_err());
        assert!(control(&[0, 1, b'x']).is_err());
        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        write(&mut writer, &[2]).unwrap();
        assert!(matches!(
            response(&mut reader),
            Err(Error::InvalidTransportFrame)
        ));
    }

    #[test]
    fn unix_rejections_never_create_a_leaf_or_call_the_clone_seam() {
        // Each case pins WHY it was refused, not merely that it was. Until goxi
        // blocker 14 gave refusals a category these all asserted the same
        // indistinct error — and `create_wire` was omitting the authority byte,
        // so every "policy" case here was actually being refused as a malformed
        // frame before `CommandSpec::validate` ever saw the command. The
        // allow-list assertions were vacuous and nothing could tell.
        let cases: Vec<(&str, Vec<u8>, ControlRejection)> = vec![
            ("malformed", vec![0], ControlRejection::Malformed),
            (
                "unsafe-program",
                create_wire("/bin/../sh", "/workspace", &[]),
                ControlRejection::Command,
            ),
            (
                "unsafe-cwd",
                create_wire("/bin/true", "/workspace/../escape", &[]),
                ControlRejection::Command,
            ),
            (
                "forbidden-env",
                create_wire("/bin/true", "/workspace", &[("LD_PRELOAD", "x")]),
                ControlRejection::Command,
            ),
            (
                "over-budget",
                create_wire(
                    "/bin/true",
                    "/workspace",
                    &[("DJINN_VALUE", &"x".repeat(CommandSpec::MAX_BYTES))],
                ),
                ControlRejection::Command,
            ),
            (
                "descriptor-shape",
                {
                    let mut x = create_wire("/bin/true", "/workspace", &[]);
                    x.extend([0, 1, 2]);
                    x
                },
                ControlRejection::Malformed,
            ),
        ];
        for (name, wire, expected) in cases {
            let counts = Counts::new();
            let (client_stream, server_stream) = UnixStream::pair().unwrap();
            thread::scope(|scope| {
                let mut server =
                    UnixBrokerServer::new(broker(counts.clone(), NoClone(counts.clone())));
                let task = scope.spawn(move || server.serve_connection(server_stream));
                let mut client =
                    UnixBrokerClient::connect(client_stream, b"test-credential").unwrap();
                ready(&mut client);
                client
                    .begin(Invocation {
                        id: name.into(),
                        fence: 7,
                    })
                    .unwrap();
                let mut payload = client.control(name).unwrap();
                payload.extend(wire);
                let refused = client
                    .call(CREATE, &payload)
                    .expect_err("a rejected CREATE must not succeed");
                assert!(
                    matches!(refused, Error::ControlRejected(actual) if actual == expected),
                    "{name} must be refused as {expected:?}, got {refused:?}"
                );
                drop(client);
                task.join().unwrap().unwrap();
            });
            assert_eq!(counts.leaves.load(Ordering::SeqCst), 0, "{name}");
            assert_eq!(counts.clones.load(Ordering::SeqCst), 0, "{name}");
        }
    }

    /// AC2, over the real socket: a mismatched READY is refused with its own
    /// category AND the serve loop returns, so the connection is closed before
    /// a BEGIN or CREATE frame can be dispatched on it. No leaf, no clone.
    #[test]
    fn unix_protocol_mismatch_refuses_ready_and_closes_the_connection() {
        let counts = Counts::new();
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        thread::scope(|scope| {
            // The launcher runs resize-v2; `ready()` asserts leaf-v1.
            let mut server = UnixBrokerServer::new(broker_running(
                counts.clone(),
                NoClone(counts.clone()),
                crate::LauncherAuthorityProtocol::ResizeV2,
            ));
            let task = scope.spawn(move || server.serve_connection(server_stream));
            let mut client = UnixBrokerClient::connect(client_stream, b"test-credential").unwrap();

            let refused = client
                .ready(
                    crate::child::prepare_worker_readiness(
                        &mut ReadyDumpability,
                        crate::LauncherAuthorityProtocol::LeafV1,
                    )
                    .unwrap(),
                )
                .expect_err("a disagreeing READY must be refused");
            assert!(
                matches!(refused, Error::ControlRejected(ControlRejection::Protocol)),
                "the worker must learn WHY it was dropped, got {refused:?}"
            );

            // The socket is gone: the next control cannot be served at all.
            assert!(
                client
                    .begin(Invocation {
                        id: "after-mismatch".into(),
                        fence: 1,
                    })
                    .is_err(),
                "no BEGIN may be dispatched after a refused READY"
            );
            drop(client);
            let served = task.join().unwrap();
            assert!(
                matches!(served, Err(Error::ProtocolMismatch { .. })),
                "the serve loop must END on a protocol mismatch, got {served:?}"
            );
        });
        assert_eq!(counts.leaves.load(Ordering::SeqCst), 0);
        assert_eq!(counts.clones.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn unix_wrong_binding_nonce_stdio_shape_and_unready_profile_reject_pre_exec() {
        let counts = Counts::new();
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        thread::scope(|scope| {
            let mut server = UnixBrokerServer::new(broker(counts.clone(), NoClone(counts.clone())));
            let task = scope.spawn(move || server.serve_connection(server_stream));
            let mut client = UnixBrokerClient::connect(client_stream, b"test-credential").unwrap();
            // The categories are pinned, not just "some refusal": before goxi
            // blocker 14 every one of these arrived as the single indistinct
            // , and a fence mismatch wearing that name is what
            // sent the investigation in the wrong direction.
            assert!(matches!(
                client.call(READY, &[0; 16]),
                Err(Error::ControlRejected(ControlRejection::Worker))
            ));
            assert!(matches!(
                client.begin(Invocation {
                    id: "unready".into(),
                    fence: 1
                }),
                Err(Error::ControlRejected(ControlRejection::Worker))
            ));
            ready(&mut client);
            client
                .begin(Invocation {
                    id: "bound".into(),
                    fence: 1,
                })
                .unwrap();
            let nonce = *client.nonces.get("bound").unwrap();
            let mut wrong_id = enc("other").unwrap();
            wrong_id.extend(nonce.as_bytes());
            wrong_id.extend(create_wire("/bin/true", "/workspace", &[]));
            assert!(matches!(
                client.call(CREATE, &wrong_id),
                Err(Error::ControlRejected(ControlRejection::Binding))
            ));
            let mut stale = client.control("bound").unwrap();
            stale[2 + "bound".len()] ^= 1;
            stale.extend(create_wire("/bin/true", "/workspace", &[]));
            assert!(matches!(
                client.call(CREATE, &stale),
                Err(Error::ControlRejected(ControlRejection::Nonce))
            ));
            let mut descriptor = client.control("bound").unwrap();
            descriptor.push(9);
            assert!(matches!(
                client.call(STDOUT, &descriptor),
                Err(Error::ControlRejected(_))
            ));
            drop(client);
            task.join().unwrap().unwrap();
        });
        assert_eq!(counts.leaves.load(Ordering::SeqCst), 0);
        assert_eq!(counts.clones.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn unix_output_is_independent_bounded_and_reports_exit_or_signal() {
        for outcome in [Outcome::Exit(23), Outcome::Signal] {
            let counts = Counts::new();
            let (client_stream, server_stream) = UnixStream::pair().unwrap();
            thread::scope(|scope| {
                let mut server = UnixBrokerServer::new(broker(
                    counts.clone(),
                    ForkClone {
                        counts: counts.clone(),
                        outcome,
                    },
                ));
                let task = scope.spawn(move || server.serve_connection(server_stream));
                let mut client =
                    UnixBrokerClient::connect(client_stream, b"test-credential").unwrap();
                ready(&mut client);
                client
                    .begin(Invocation {
                        id: "child".into(),
                        fence: 1,
                    })
                    .unwrap();
                client
                    .create("child", "leaf", LeaseAuthority::Armed, &command())
                    .unwrap();
                let mut stdout = Vec::new();
                let mut stdout_eof = false;
                while !stdout_eof {
                    let (frame, eof, status) = client.stdout("child").unwrap();
                    assert!(frame.len() <= 4096);
                    stdout.extend(frame);
                    stdout_eof = eof;
                    assert!(matches!(
                        status,
                        crate::broker::ChildStatus::Running
                            | crate::broker::ChildStatus::Exited(_)
                            | crate::broker::ChildStatus::Signaled(_)
                    ));
                }
                let mut stderr = Vec::new();
                let mut stderr_eof = false;
                while !stderr_eof {
                    let (frame, eof, status) = client.stderr("child").unwrap();
                    assert!(frame.len() <= 4096);
                    stderr.extend(frame);
                    stderr_eof = eof;
                    assert!(matches!(
                        status,
                        crate::broker::ChildStatus::Running
                            | crate::broker::ChildStatus::Exited(_)
                            | crate::broker::ChildStatus::Signaled(_)
                    ));
                }
                // EOF only says that a stream has closed. Keep polling after
                // both explicit EOFs until waitpid reports a terminal status;
                // closing descriptors before _exit/raise is not terminal.
                let terminal = loop {
                    let (frame, eof, status) = client.stdout("child").unwrap();
                    assert!(frame.is_empty());
                    assert!(eof, "stdout EOF must remain observable while polling");
                    if status != crate::broker::ChildStatus::Running {
                        break status;
                    }
                    thread::yield_now();
                };
                assert!(stdout_eof);
                assert!(stderr_eof);
                let pipe_capacity = counts.pipe_capacity.load(Ordering::SeqCst);
                let output_bytes = counts.output_bytes.load(Ordering::SeqCst);
                assert!(output_bytes > pipe_capacity);
                assert_eq!(stdout, vec![b'o'; output_bytes]);
                assert_eq!(stderr, b"separate-stderr");
                match outcome {
                    Outcome::Exit(code) => {
                        assert_eq!(terminal, crate::broker::ChildStatus::Exited(code))
                    }
                    Outcome::Signal => assert_eq!(
                        terminal,
                        crate::broker::ChildStatus::Signaled(libc::SIGTERM as u8)
                    ),
                }
                drop(client);
                task.join().unwrap().unwrap();
            });
            assert_eq!(counts.leaves.load(Ordering::SeqCst), 1);
            assert_eq!(counts.clones.load(Ordering::SeqCst), 1);
        }
    }

    /// Unprivileged embeddings already own their socket, so handing it to the
    /// worker identity is a no-op that must never weaken the 0600 mode. The
    /// privileged half (root hands a 0600 socket to uid 1000, which is what
    /// admits the worker and refuses the uid-1001 child at `connect`) is proven
    /// in `tests/kernel_boundary_under_rendered_context.rs`.
    #[test]
    fn worker_socket_ownership_is_a_no_op_when_unprivileged_and_never_relaxes_mode() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = std::env::var_os("CARGO_TARGET_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("djinn-launcher-socket-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("broker.sock");
        let listener = UnixListener::bind(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        restrict_socket_to_worker(&path).unwrap();
        assert_eq!(
            fs::symlink_metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "the control socket must stay owner-only"
        );
        drop(listener);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A well-formed `CREATE` payload, in the exact field order `command_in`
    /// parses: leaf, **authority byte**, program, cwd, argc, argv, envc, env.
    ///
    /// The authority byte used to be missing here, which made every case above
    /// fail at `take_string` on a shifted frame rather than at the check it
    /// claimed to exercise.
    fn create_wire(program: &str, cwd: &str, environment: &[(&str, &str)]) -> Vec<u8> {
        let mut wire = enc("leaf").unwrap();
        wire.push(LeaseAuthority::Armed.wire());
        wire.extend(enc(program).unwrap());
        wire.extend(enc(cwd).unwrap());
        wire.push(0);
        wire.push(environment.len() as u8);
        for (key, value) in environment {
            wire.extend(enc(key).unwrap());
            wire.extend(enc(value).unwrap());
        }
        wire
    }
}
