//! ij6g: fake protocol-v1 wrapper integration test.
//!
//! A fixture wrapper server binds a real Unix control socket at the exact path
//! the worker adapter is told to use and speaks protocol v1. This proves the
//! production `UnixCatalogServiceProvisioner` client connects to the socket a
//! wrapper server actually creates, drives the full create→ready→delete
//! lifecycle in order, tears leases down in reverse, and fails closed on a
//! missing socket or a mismatched protocol revision.

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;

use djinn_sandbox::service_provisioning::{
    CatalogServiceProvisioner, ServiceProvisioningCode, UnixCatalogServiceProvisioner,
    create_ready_leases, delete_leases,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// Records the operation of every request that reaches the socket.
type Journal = Arc<Mutex<Vec<String>>>;

/// A minimal protocol-v1 wrapper: creates the socket and answers create/ready/
/// delete. `revision` lets a test force a protocol mismatch. Loops until aborted.
fn spawn_fake_wrapper(
    socket: &Path,
    revision: u32,
    env_name: &'static str,
    journal: Journal,
) -> tokio::task::JoinHandle<()> {
    let listener = UnixListener::bind(socket).expect("bind fake wrapper socket");
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let journal = journal.clone();
            tokio::spawn(async move {
                handle(stream, revision, env_name, journal).await;
            });
        }
    })
}

async fn handle(stream: UnixStream, revision: u32, env_name: &'static str, journal: Journal) {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    if BufReader::new(read).read_line(&mut line).await.is_err() {
        return;
    }
    let request: serde_json::Value = match serde_json::from_str(&line) {
        Ok(value) => value,
        Err(_) => return,
    };
    let operation = request["operation"].as_str().unwrap_or_default().to_owned();
    journal.lock().unwrap().push(operation.clone());
    let response = match operation.as_str() {
        "create_fresh" => serde_json::json!({
            "status": "created",
            "revision": revision,
            "lease_id": "lease_abc",
            "environment": { env_name: "proto://127.0.0.1:5432/lease" },
        }),
        "ready" => serde_json::json!({"status": "ready", "revision": revision}),
        "delete" => serde_json::json!({"status": "deleted", "revision": revision}),
        _ => serde_json::json!({"status": "error", "revision": revision, "code": "rejected"}),
    };
    let mut body = serde_json::to_vec(&response).unwrap();
    body.push(b'\n');
    let _ = write.write_all(&body).await;
}

#[tokio::test]
async fn adapter_drives_full_lifecycle_against_a_real_socket() {
    let dir = tempfile::tempdir().unwrap();
    // The adapter connects to the exact path we hand the provisioner — the same
    // shape djinn-k8s renders (`<control-dir>/<preset>.sock`).
    let socket = dir.path().join("preset-postgres-18.sock");
    let journal: Journal = Arc::new(Mutex::new(Vec::new()));
    let server = spawn_fake_wrapper(&socket, 1, "TEST_URL", journal.clone());

    let provisioner: Arc<dyn CatalogServiceProvisioner> =
        Arc::new(UnixCatalogServiceProvisioner::new(
            "preset-postgres-18".into(),
            socket.clone(),
            vec!["TEST_URL".into()],
        ));

    let leases = create_ready_leases(std::slice::from_ref(&provisioner), "attempt-1")
        .await
        .expect("create + ready succeed against the fake wrapper");
    assert_eq!(leases.len(), 1);
    assert_eq!(
        leases[0].1.environment["TEST_URL"],
        "proto://127.0.0.1:5432/lease"
    );

    // Create is followed by readiness — the canonical order.
    assert_eq!(*journal.lock().unwrap(), vec!["create_fresh", "ready"]);

    delete_leases(&leases).await.expect("reverse teardown");
    assert_eq!(
        *journal.lock().unwrap(),
        vec!["create_fresh", "ready", "delete"]
    );

    server.abort();
}

#[tokio::test]
async fn adapter_fails_closed_on_a_missing_socket() {
    let dir = tempfile::tempdir().unwrap();
    let provisioner = UnixCatalogServiceProvisioner::new(
        "preset-postgres-18".into(),
        dir.path().join("never-bound.sock"),
        vec!["TEST_URL".into()],
    );
    let error = provisioner.create("attempt").await.unwrap_err();
    assert_eq!(error.code, ServiceProvisioningCode::Unavailable);
}

#[tokio::test]
async fn adapter_fails_closed_on_protocol_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("preset-redis-7.sock");
    let journal: Journal = Arc::new(Mutex::new(Vec::new()));
    // The fake wrapper answers with revision 2 — an unsupported protocol.
    let server = spawn_fake_wrapper(&socket, 2, "REDIS_URL", journal);
    let provisioner = UnixCatalogServiceProvisioner::new(
        "preset-redis-7".into(),
        socket,
        vec!["REDIS_URL".into()],
    );
    let error = provisioner.create("attempt").await.unwrap_err();
    assert_eq!(error.code, ServiceProvisioningCode::ProtocolMismatch);
    server.abort();
}
