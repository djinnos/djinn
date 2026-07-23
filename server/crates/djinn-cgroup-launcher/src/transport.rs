//! Bounded Unix socket transport for authenticated broker controls.
use crate::{
    CgroupFs, CloneIntoCgroup, CommandSpec, CpuStat, Error, Invocation,
    broker::{Broker, ConnectionId, ControlNonce, NonceSource, SocketPeer},
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
impl<F: CgroupFs, S: CloneIntoCgroup, N: NonceSource> UnixBrokerServer<F, S, N> {
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
        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
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
            reply(&mut s, self.dispatch(c, &r))?
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
                let (leaf, command) = command_in(x)?;
                Ok(self
                    .broker
                    .create(c, &i, n, &leaf, &command)?
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
    pub fn create(&mut self, i: &str, leaf: &str, command: &CommandSpec) -> Result<(), Error> {
        let mut x = self.control(i)?;
        x.extend(command_out(leaf, command)?);
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
fn command_out(leaf: &str, command: &CommandSpec) -> Result<Vec<u8>, Error> {
    command.validate()?;
    let mut out = enc(leaf)?;
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
fn command_in(input: &[u8]) -> Result<(String, CommandSpec), Error> {
    let (leaf, mut rest) = take_string(input)?;
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
    Ok((leaf, command))
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
fn reply(s: &mut UnixStream, r: Result<Vec<u8>, Error>) -> Result<(), Error> {
    match r {
        Ok(x) => {
            let mut y = vec![0];
            y.extend(x);
            write(s, &y)
        }
        Err(_) => write(s, &[1]),
    }
}
fn response(s: &mut UnixStream) -> Result<Vec<u8>, Error> {
    let x = read(s)?;
    match x.split_first() {
        Some((0, x)) => Ok(x.to_vec()),
        Some((1, _)) => Err(Error::InvalidControl),
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
        CgroupMode, ChildProcess, CloneIntoCgroup, Invocation, Launcher, LauncherConfig, Readiness,
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
    impl CloneIntoCgroup for ForkClone {
        fn clone_into_cgroup(
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
    impl CloneIntoCgroup for NoClone {
        fn clone_into_cgroup(
            &mut self,
            _: RawFd,
            _: &Invocation,
            _: &CommandSpec,
        ) -> Result<ChildProcess, Error> {
            self.0.clones.fetch_add(1, Ordering::SeqCst);
            Err(Error::CloneDenied)
        }
    }
    struct TestNonces(u8);
    impl NonceSource for TestNonces {
        fn nonce(&mut self) -> Result<crate::broker::ControlNonce, Error> {
            self.0 = self.0.wrapping_add(1);
            Ok(crate::broker::ControlNonce::from_bytes([self.0; 32]))
        }
    }
    fn broker<S: CloneIntoCgroup>(counts: Counts, clone: S) -> Broker<FakeFs, S, TestNonces> {
        let launcher = Launcher::new(
            FakeFs(counts),
            clone,
            LauncherConfig::new(None, unsafe { libc::geteuid() }).unwrap(),
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
            .ready(crate::child::prepare_worker_readiness(&mut ReadyDumpability).unwrap())
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
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("malformed", vec![0]),
            (
                "unsafe-program",
                create_wire("/bin/../sh", "/workspace", &[]),
            ),
            (
                "unsafe-cwd",
                create_wire("/bin/true", "/workspace/../escape", &[]),
            ),
            (
                "forbidden-env",
                create_wire("/bin/true", "/workspace", &[("LD_PRELOAD", "x")]),
            ),
            (
                "over-budget",
                create_wire(
                    "/bin/true",
                    "/workspace",
                    &[("DJINN_VALUE", &"x".repeat(CommandSpec::MAX_BYTES))],
                ),
            ),
            ("descriptor-shape", {
                let mut x = create_wire("/bin/true", "/workspace", &[]);
                x.extend([0, 1, 2]);
                x
            }),
        ];
        for (name, wire) in cases {
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
                assert!(matches!(
                    client.call(CREATE, &payload),
                    Err(Error::InvalidControl)
                ));
                drop(client);
                task.join().unwrap().unwrap();
            });
            assert_eq!(counts.leaves.load(Ordering::SeqCst), 0, "{name}");
            assert_eq!(counts.clones.load(Ordering::SeqCst), 0, "{name}");
        }
    }

    #[test]
    fn unix_wrong_binding_nonce_stdio_shape_and_unready_profile_reject_pre_exec() {
        let counts = Counts::new();
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        thread::scope(|scope| {
            let mut server = UnixBrokerServer::new(broker(counts.clone(), NoClone(counts.clone())));
            let task = scope.spawn(move || server.serve_connection(server_stream));
            let mut client = UnixBrokerClient::connect(client_stream, b"test-credential").unwrap();
            assert!(matches!(
                client.call(READY, &[0; 16]),
                Err(Error::InvalidControl)
            ));
            assert!(matches!(
                client.begin(Invocation {
                    id: "unready".into(),
                    fence: 1
                }),
                Err(Error::InvalidControl)
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
                Err(Error::InvalidControl)
            ));
            let mut stale = client.control("bound").unwrap();
            stale[2 + "bound".len()] ^= 1;
            stale.extend(create_wire("/bin/true", "/workspace", &[]));
            assert!(matches!(
                client.call(CREATE, &stale),
                Err(Error::InvalidControl)
            ));
            let mut descriptor = client.control("bound").unwrap();
            descriptor.push(9);
            assert!(matches!(
                client.call(STDOUT, &descriptor),
                Err(Error::InvalidControl)
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
                client.create("child", "leaf", &command()).unwrap();
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

    fn create_wire(program: &str, cwd: &str, environment: &[(&str, &str)]) -> Vec<u8> {
        let mut wire = enc("leaf").unwrap();
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
