//! Bounded Unix socket transport for authenticated broker controls.
use crate::{
    CgroupFs, CloneIntoCgroup, CpuStat, Error, Invocation,
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
                Ok(self.broker.create(c, &i, n, &strv(x)?)?.as_bytes().to_vec())
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
    pub fn create(&mut self, i: &str, l: &str) -> Result<(), Error> {
        let mut x = self.control(i)?;
        x.extend(enc(l)?);
        let r = self.call(CREATE, &x)?;
        self.rotate(i, &r)
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
fn strv(x: &[u8]) -> Result<String, Error> {
    if x.len() < 2 {
        return Err(Error::InvalidTransportFrame);
    }
    let n = u16::from_be_bytes([x[0], x[1]]) as usize;
    if x.len() != n + 2 {
        return Err(Error::InvalidTransportFrame);
    }
    std::str::from_utf8(&x[2..])
        .map(str::to_owned)
        .map_err(|_| Error::InvalidTransportFrame)
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
        assert!(strv(&[0, 2, b'x']).is_err());
        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        write(&mut writer, &[2]).unwrap();
        assert!(matches!(
            response(&mut reader),
            Err(Error::InvalidTransportFrame)
        ));
    }
}
