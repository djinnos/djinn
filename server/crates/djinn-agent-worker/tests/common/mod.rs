//! Shared subprocess fixtures for worker integration tests.

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

/// Minimal broker control endpoint for worker tests that do not invoke a shell.
///
/// The production worker still performs the real authenticated transport
/// handshake and readiness exchange. These tests stop before any broker child
/// request, so the fixture intentionally accepts only AUTH and READY frames and
/// rejects every later control.
pub struct ReadinessBroker {
    pub socket_path: PathBuf,
    pub credential_path: PathBuf,
    _server: std::thread::JoinHandle<()>,
}

impl ReadinessBroker {
    pub fn start(dir: &Path) -> Self {
        let socket_path = dir.join("cgroup-broker.sock");
        let credential_path = dir.join("cgroup-broker-credential");
        std::fs::write(&credential_path, b"worker-test-credential")
            .expect("write broker credential");
        let listener = UnixListener::bind(&socket_path).expect("bind test broker socket");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept worker broker connection");
            let auth = read_frame(&mut stream).expect("read broker AUTH frame");
            assert_eq!(auth.first(), Some(&1), "worker must authenticate first");
            write_frame(&mut stream, &[0]).expect("accept broker authentication");

            let ready = read_frame(&mut stream).expect("read broker READY frame");
            assert_eq!(ready.first(), Some(&2), "worker must submit readiness");
            write_frame(&mut stream, &[0]).expect("accept worker readiness");

            // No shell is expected in these integration paths. Keep the
            // connection alive until the worker drops its launch context.
            while let Ok(frame) = read_frame(&mut stream) {
                panic!("unexpected broker control type {:?}", frame.first());
            }
        });
        Self {
            socket_path,
            credential_path,
            _server: server,
        }
    }
}

fn read_frame(stream: &mut impl Read) -> std::io::Result<Vec<u8>> {
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    let mut frame = vec![0; length];
    stream.read_exact(&mut frame)?;
    Ok(frame)
}

fn write_frame(stream: &mut impl Write, frame: &[u8]) -> std::io::Result<()> {
    stream.write_all(&(frame.len() as u32).to_be_bytes())?;
    stream.write_all(frame)
}
