//! Shared subprocess fixtures for worker integration tests.

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Minimal fake cgroup-launcher sidecar for worker tests that do not invoke a
/// shell.
///
/// It mirrors the real launcher's handshake ordering (task ab05): it waits for
/// the worker to publish its private credential and `worker.pid` into the shared
/// IPC directory, reads them, and only THEN binds the control socket and accepts
/// a connection. The worker still performs the real authenticated transport
/// handshake and readiness exchange; these tests stop before any broker child
/// request, so the fixture accepts only AUTH (asserting the presented credential
/// matches what the worker wrote) and READY, then rejects every later control.
pub struct ReadinessBroker {
    pub socket_path: PathBuf,
    pub credential_path: PathBuf,
    _server: std::thread::JoinHandle<()>,
}

impl ReadinessBroker {
    pub fn start(dir: &Path) -> Self {
        let socket_path = dir.join("broker.sock");
        let credential_path = dir.join("credential");
        let pid_path = dir.join("worker.pid");
        let thread_socket = socket_path.clone();
        let thread_credential = credential_path.clone();
        let server = std::thread::spawn(move || {
            // Wait for the worker to publish BOTH handshake files, exactly as the
            // real launcher's `read_worker_handshake` does, then read them.
            let credential = loop {
                if thread_credential.exists() && pid_path.exists() {
                    let bytes = std::fs::read(&thread_credential).expect("read worker credential");
                    let pid = std::fs::read_to_string(&pid_path).expect("read worker.pid");
                    assert!(
                        !bytes.is_empty() && pid.trim().parse::<u32>().is_ok_and(|p| p != 0),
                        "worker handshake must be a non-empty credential + non-zero pid"
                    );
                    break bytes;
                }
                std::thread::sleep(Duration::from_millis(10));
            };

            // Only now bind the socket, so the worker's bounded connect-retry
            // sees the same pre-bind race the production launcher creates.
            let listener = UnixListener::bind(&thread_socket).expect("bind test broker socket");
            let (mut stream, _) = listener.accept().expect("accept worker broker connection");

            let auth = read_frame(&mut stream).expect("read broker AUTH frame");
            assert_eq!(auth.first(), Some(&1), "worker must authenticate first");
            assert_eq!(
                &auth[1..],
                credential.as_slice(),
                "worker must present the credential it published"
            );
            write_frame(&mut stream, &[0]).expect("accept broker authentication");

            let ready = read_frame(&mut stream).expect("read broker READY frame");
            assert_eq!(ready.first(), Some(&2), "worker must submit readiness");
            write_frame(&mut stream, &[0]).expect("accept worker readiness");

            // No shell is expected in these integration paths. Block until the
            // worker drops its launch context (EOF); any control frame before
            // that is unexpected in a readiness-only exchange.
            if let Ok(frame) = read_frame(&mut stream) {
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
