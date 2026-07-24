//! RabbitMQ execution coverage for the [`super`] adapter.
//!
//! The control plane is a real `rabbitmqctl` subprocess (a generated shell
//! script), so every assertion exercises the wrapper's real fixed-argument
//! spawn + bounded-capture path; the AMQP data plane is an in-process 0-9-1
//! fake that validates the very vhost/user/password state the fake
//! `rabbitmqctl` wrote. No network management endpoint, shell interpolation,
//! or operator credential is involved. A real broker is not spawned.

use super::*;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use tokio::net::TcpListener;

// ----- Fake `rabbitmqctl`: a real subprocess over a shared state dir -----

const FAKE_CONTROL_SCRIPT: &str = r#"#!/bin/sh
DIR="__STATE__"
printf '%s\n' "$*" >> "$DIR/calls.log"
op="$1"
if [ -f "$DIR/slow.$op" ]; then sleep "$(cat "$DIR/slow.$op")"; fi
if [ -f "$DIR/fail.$op" ]; then cat "$DIR/fail.$op"; cat "$DIR/fail.$op" >&2; exit 70; fi
case "$op" in
  add_vhost) mkdir -p "$DIR/vhosts"; : > "$DIR/vhosts/$2" ;;
  add_user) mkdir -p "$DIR/users"; printf '%s' "$3" > "$DIR/users/$2" ;;
  set_permissions) mkdir -p "$DIR/perms"; printf '%s\n' "$3" >> "$DIR/perms/$4" ;;
  delete_vhost) rm -f "$DIR/vhosts/$2" ;;
  delete_user) rm -f "$DIR/users/$2" "$DIR/perms/$2" ;;
  list_vhosts)
echo "Listing vhosts ..."
if [ -d "$DIR/vhosts" ]; then ls "$DIR/vhosts"; fi ;;
  list_users)
echo "Listing users ..."
if [ -d "$DIR/users" ]; then for u in "$DIR"/users/*; do [ -e "$u" ] && printf '%s\t[]\n' "$(basename "$u")"; done; fi ;;
  *) echo "unknown command: $op" >&2; exit 64 ;;
esac
exit 0
"#;

struct FakeControl {
    _dir: tempfile::TempDir,
    state: PathBuf,
    script: PathBuf,
}

impl FakeControl {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let script = dir.path().join("rabbitmqctl");
        std::fs::write(
            &script,
            FAKE_CONTROL_SCRIPT.replace("__STATE__", state.to_str().unwrap()),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        Self {
            _dir: dir,
            state,
            script,
        }
    }

    fn program(&self) -> String {
        self.script.to_str().unwrap().to_owned()
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.state.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn vhosts(&self) -> Vec<String> {
        list_dir(&self.state.join("vhosts"))
    }

    fn users(&self) -> Vec<String> {
        list_dir(&self.state.join("users"))
    }

    fn fail_op(&self, op: &str, content: &str) {
        std::fs::write(self.state.join(format!("fail.{op}")), content).unwrap();
    }

    fn slow_op(&self, op: &str, seconds: &str) {
        std::fs::write(self.state.join(format!("slow.{op}")), seconds).unwrap();
    }
}

fn list_dir(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect()
}

// ----- In-process fake AMQP 0-9-1 broker over the same state dir -----

struct FakeAmqp {
    port: u16,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for FakeAmqp {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl FakeAmqp {
    async fn start(state: PathBuf) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let state = state.clone();
                tokio::spawn(async move {
                    fake_amqp_connection(stream, state).await;
                });
            }
        });
        Self { port, handle }
    }

    fn admin_url(&self) -> String {
        format!("amqp://127.0.0.1:{}", self.port)
    }
}

async fn read_fake_method(reader: &mut BufReader<OwnedReadHalf>) -> Option<(u16, u16, Vec<u8>)> {
    let mut header = [0u8; 7];
    reader.read_exact(&mut header).await.ok()?;
    let size = u32::from_be_bytes([header[3], header[4], header[5], header[6]]) as usize;
    let mut payload = vec![0u8; size];
    reader.read_exact(&mut payload).await.ok()?;
    let mut end = [0u8; 1];
    reader.read_exact(&mut end).await.ok()?;
    if end[0] != AMQP_FRAME_END || payload.len() < 4 {
        return None;
    }
    Some((
        u16::from_be_bytes([payload[0], payload[1]]),
        u16::from_be_bytes([payload[2], payload[3]]),
        payload[4..].to_vec(),
    ))
}

fn parse_plain(args: &[u8]) -> Option<(String, String)> {
    let mut offset = 0usize;
    let properties = u32::from_be_bytes(args.get(offset..offset + 4)?.try_into().ok()?) as usize;
    offset += 4 + properties;
    let mechanism = *args.get(offset)? as usize;
    offset += 1 + mechanism;
    let response_len = u32::from_be_bytes(args.get(offset..offset + 4)?.try_into().ok()?) as usize;
    offset += 4;
    let response = args.get(offset..offset + response_len)?;
    let mut parts = response.split(|&byte| byte == 0);
    let _leading = parts.next()?;
    let user = String::from_utf8(parts.next()?.to_vec()).ok()?;
    let password = String::from_utf8(parts.next()?.to_vec()).ok()?;
    Some((user, password))
}

fn parse_short(args: &[u8], offset: usize) -> Option<String> {
    let length = *args.get(offset)? as usize;
    let bytes = args.get(offset + 1..offset + 1 + length)?;
    String::from_utf8(bytes.to_vec()).ok()
}

async fn fake_amqp_connection(stream: TcpStream, state: PathBuf) {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);

    let mut protocol = [0u8; 8];
    if reader.read_exact(&mut protocol).await.is_err() {
        return;
    }

    let mut start = Vec::new();
    start.push(0); // version-major
    start.push(9); // version-minor
    amqp_long_string(&mut start, &[]); // server-properties: empty field table
    amqp_long_string(&mut start, b"PLAIN"); // mechanisms
    amqp_long_string(&mut start, b"en_US"); // locales
    if write
        .write_all(&amqp_method_frame(0, 10, 10, &start))
        .await
        .is_err()
    {
        return;
    }

    let Some((10, 11, args)) = read_fake_method(&mut reader).await else {
        return;
    };
    let Some((user, password)) = parse_plain(&args) else {
        return;
    };
    // Authenticate against exactly the user/password the fake rabbitmqctl wrote.
    match std::fs::read_to_string(state.join("users").join(&user)) {
        Ok(stored) if stored == password => {}
        _ => return, // auth failure: drop the connection
    }

    let mut tune = Vec::new();
    tune.extend_from_slice(&0u16.to_be_bytes()); // channel-max
    tune.extend_from_slice(&131_072u32.to_be_bytes()); // frame-max
    tune.extend_from_slice(&0u16.to_be_bytes()); // heartbeat
    if write
        .write_all(&amqp_method_frame(0, 10, 30, &tune))
        .await
        .is_err()
    {
        return;
    }

    let Some((10, 31, _)) = read_fake_method(&mut reader).await else {
        return;
    };
    let Some((10, 40, args)) = read_fake_method(&mut reader).await else {
        return;
    };
    let Some(vhost) = parse_short(&args, 0) else {
        return;
    };
    // The vhost must exist and this user must hold permission on it.
    if !state.join("vhosts").join(&vhost).exists() {
        return;
    }
    let perms = std::fs::read_to_string(state.join("perms").join(&user)).unwrap_or_default();
    if !perms.lines().any(|line| line == vhost) {
        return;
    }

    let mut open_ok = Vec::new();
    amqp_short_string(&mut open_ok, ""); // reserved-1
    if write
        .write_all(&amqp_method_frame(0, 10, 41, &open_ok))
        .await
        .is_err()
    {
        return;
    }

    let Some((20, 10, _)) = read_fake_method(&mut reader).await else {
        return;
    };
    let mut channel_ok = Vec::new();
    amqp_long_string(&mut channel_ok, &[]); // reserved-1
    if write
        .write_all(&amqp_method_frame(1, 20, 11, &channel_ok))
        .await
        .is_err()
    {
        return;
    }

    let Some((50, 10, args)) = read_fake_method(&mut reader).await else {
        return;
    };
    let Some(queue) = parse_short(&args, 2) else {
        return;
    };
    let mut declare_ok = Vec::new();
    amqp_short_string(&mut declare_ok, &queue);
    declare_ok.extend_from_slice(&0u32.to_be_bytes()); // message-count
    declare_ok.extend_from_slice(&0u32.to_be_bytes()); // consumer-count
    if write
        .write_all(&amqp_method_frame(1, 50, 11, &declare_ok))
        .await
        .is_err()
    {
        return;
    }

    let Some((50, 40, _)) = read_fake_method(&mut reader).await else {
        return;
    };
    let mut delete_ok = Vec::new();
    delete_ok.extend_from_slice(&0u32.to_be_bytes()); // message-count
    if write
        .write_all(&amqp_method_frame(1, 50, 41, &delete_ok))
        .await
        .is_err()
    {
        return;
    }

    let Some((10, 50, _)) = read_fake_method(&mut reader).await else {
        return;
    };
    let _ = write.write_all(&amqp_method_frame(0, 10, 51, &[])).await; // Close-Ok
}

struct Harness {
    control: FakeControl,
    amqp: FakeAmqp,
    adapter: RabbitAdapter,
}

async fn harness() -> Harness {
    let control = FakeControl::new();
    let amqp = FakeAmqp::start(control.state.clone()).await;
    let adapter = RabbitAdapter::new(
        &amqp.admin_url(),
        &control.program(),
        vec!["AMQP_URL".into()],
    )
    .unwrap();
    Harness {
        control,
        amqp,
        adapter,
    }
}

fn lease_parts(url: &str) -> (String, String, String) {
    let parsed = Url::parse(url).unwrap();
    (
        parsed.username().to_owned(),
        parsed.password().unwrap().to_owned(),
        parsed.path().trim_start_matches('/').to_owned(),
    )
}

async fn wait_for_empty_creations(adapter: &RabbitAdapter) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if adapter.creations.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("background cleanup did not finish");
}

async fn wait_for_cleanup_pending(adapter: &RabbitAdapter, attempt: &str, nonce: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(
                adapter.creations.lock().await.get(&CreationKey {
                    attempt_id: attempt.to_owned(),
                    lease_nonce: nonce.to_owned(),
                }),
                Some(RabbitCreationState::CleanupPending(_))
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("partial cleanup authority was discarded");
}

// ----- Configuration validation -----

#[test]
fn rejects_invalid_configuration() {
    assert!(
        RabbitAdapter::new(
            "amqp://127.0.0.1:5672",
            "rabbitmqctl",
            vec!["AMQP_URL".into()]
        )
        .is_ok()
    );
    // Wrong scheme, empty control program, and unsafe env name each fail closed.
    assert!(
        RabbitAdapter::new("redis://127.0.0.1", "rabbitmqctl", vec!["AMQP_URL".into()]).is_err()
    );
    assert!(RabbitAdapter::new("amqp://127.0.0.1", "", vec!["AMQP_URL".into()]).is_err());
    assert!(
        RabbitAdapter::new("amqp://127.0.0.1", "rabbitmqctl", vec!["AMQP-URL".into()]).is_err()
    );
}

#[tokio::test]
async fn protocol_mismatch_fails_closed_without_backend_access() {
    let control = FakeControl::new();
    let adapter = RabbitAdapter::new(
        "amqp://127.0.0.1:5672",
        &control.program(),
        vec!["AMQP_URL".into()],
    )
    .unwrap();
    let response = rabbit_dispatch(
        Request::Delete {
            revision: 2,
            lease_id: "lease_1".into(),
        },
        adapter,
    )
    .await;
    assert_eq!(response, error_response("revision_mismatch"));
    // A protocol mismatch never touches the local control plane.
    assert!(control.calls().is_empty());
}

// ----- AC1: fixed-argument vhost + user + vhost-scoped permissions -----

#[tokio::test]
async fn create_provisions_vhost_user_and_scoped_permissions() {
    let Harness {
        control, adapter, ..
    } = harness().await;
    let (lease_id, env) = adapter
        .create("attempt_a".into(), "nonce_a".into())
        .await
        .unwrap();
    // Only the catalog-configured environment name is returned.
    assert_eq!(env.keys().cloned().collect::<Vec<_>>(), vec!["AMQP_URL"]);
    let (user, password, vhost) = lease_parts(&env["AMQP_URL"]);

    // Fixed argument sequence: vhost, then user, then vhost-scoped grant.
    let calls = control.calls();
    assert_eq!(calls[0], format!("add_vhost {vhost}"));
    assert_eq!(calls[1], format!("add_user {user} {password}"));
    assert_eq!(
        calls[2],
        format!("set_permissions -p {vhost} {user} .* .* .*")
    );
    assert!(control.vhosts().contains(&vhost));
    assert!(control.users().contains(&user));

    adapter.delete(&lease_id).await.unwrap();
}

// ----- AC2: readiness authenticates over AMQP; isolation holds -----

#[tokio::test]
async fn ready_authenticates_over_amqp_and_cross_lease_access_is_denied() {
    // Keep the fake control script and AMQP server alive for the whole test.
    let Harness {
        control: _control,
        amqp: _amqp,
        adapter,
    } = harness().await;
    let (a_id, a_env) = adapter
        .create("attempt_a".into(), "nonce_a".into())
        .await
        .unwrap();
    let (b_id, b_env) = adapter
        .create("attempt_b".into(), "nonce_b".into())
        .await
        .unwrap();

    // Each lease's own credentials open its own vhost.
    adapter.ready(&a_id).await.unwrap();
    adapter.ready(&b_id).await.unwrap();
    // Unknown lease never touches the backend.
    assert_eq!(
        adapter.ready("lease_missing").await,
        Err(AdapterError::Rejected)
    );

    // Lease A's credentials cannot open lease B's vhost.
    let (a_user, a_password, _) = lease_parts(&a_env["AMQP_URL"]);
    let (_, _, b_vhost) = lease_parts(&b_env["AMQP_URL"]);
    assert_eq!(
        amqp_probe(&adapter.endpoint, &a_user, &a_password, &b_vhost, "probe").await,
        Err(AdapterError::Rejected)
    );

    adapter.delete(&a_id).await.unwrap();
    adapter.delete(&b_id).await.unwrap();
}

// ----- AC2/AC5: Delete drops only the leased vhost then user, in order -----

#[tokio::test]
async fn delete_removes_only_its_own_vhost_then_user() {
    let Harness {
        control,
        adapter,
        amqp: _amqp,
    } = harness().await;
    let (a_id, a_env) = adapter
        .create("attempt_a".into(), "nonce_a".into())
        .await
        .unwrap();
    let (b_id, b_env) = adapter
        .create("attempt_b".into(), "nonce_b".into())
        .await
        .unwrap();
    let (a_user, _, a_vhost) = lease_parts(&a_env["AMQP_URL"]);
    let (_, _, b_vhost) = lease_parts(&b_env["AMQP_URL"]);

    let before = control.calls().len();
    adapter.delete(&a_id).await.unwrap();
    let delete_calls: Vec<String> = control.calls().into_iter().skip(before).collect();

    // vhost is dropped before the user, and only lease A's identifiers appear.
    assert_eq!(delete_calls[0], "list_vhosts");
    assert_eq!(delete_calls[1], format!("delete_vhost {a_vhost}"));
    assert_eq!(delete_calls[2], "list_users");
    assert_eq!(delete_calls[3], format!("delete_user {a_user}"));

    assert!(!control.vhosts().contains(&a_vhost));
    assert!(!control.users().contains(&a_user));
    // Lease B is untouched: its vhost survives and it still readies over AMQP.
    assert!(control.vhosts().contains(&b_vhost));
    adapter.ready(&b_id).await.unwrap();

    adapter.delete(&b_id).await.unwrap();
}

// ----- AC3: idempotent create; repeated and unknown delete are no-ops -----

#[tokio::test]
async fn create_is_idempotent_and_delete_is_repeatable() {
    let Harness {
        control: _control,
        amqp: _amqp,
        adapter,
    } = harness().await;
    let (first, first_env) = adapter
        .create("attempt_retry".into(), "nonce_retry".into())
        .await
        .unwrap();
    let (retry, retry_env) = adapter
        .create("attempt_retry".into(), "nonce_retry".into())
        .await
        .unwrap();
    assert_eq!(first, retry);
    assert_eq!(first_env, retry_env);
    assert_eq!(adapter.leases.lock().await.len(), 1);

    adapter.delete(&first).await.unwrap();
    // Repeated delete of the same lease and an unknown lease are no-op successes.
    adapter.delete(&first).await.unwrap();
    adapter.delete("lease_unknown").await.unwrap();
}

// ----- AC1/AC4: partial create failure rolls back the created vhost -----

#[tokio::test]
async fn partial_create_failure_rolls_back_the_vhost() {
    let Harness {
        control, adapter, ..
    } = harness().await;
    // add_vhost succeeds; add_user fails, so the vhost must be rolled back.
    control.fail_op("add_user", "boom");
    assert_eq!(
        adapter.create("attempt_x".into(), "nonce_x".into()).await,
        Err(AdapterError::Rejected)
    );
    wait_for_empty_creations(&adapter).await;
    assert!(adapter.leases.lock().await.is_empty());
    assert!(control.vhosts().is_empty());
    assert!(control.users().is_empty());
}

// ----- AC3/finding: a failed rollback retains authority until a retry -----

#[tokio::test]
async fn failed_rollback_retains_authority_until_a_retry_cleans_it() {
    let Harness {
        control, adapter, ..
    } = harness().await;
    control.fail_op("add_user", "boom");
    adapter.fail_next_rollbacks(1);
    assert_eq!(
        adapter.create("attempt_c".into(), "nonce_c".into()).await,
        Err(AdapterError::Rejected)
    );
    wait_for_cleanup_pending(&adapter, "attempt_c", "nonce_c").await;
    // The created vhost is still present under retained cleanup authority.
    assert!(!control.vhosts().is_empty());
    // The identical retry retries cleanup rather than orphaning the vhost.
    assert_eq!(
        adapter.create("attempt_c".into(), "nonce_c".into()).await,
        Err(AdapterError::Rejected)
    );
    wait_for_empty_creations(&adapter).await;
    assert!(control.vhosts().is_empty());
}

// ----- AC4: control failure is bounded and leaks no backend output -----

#[tokio::test]
async fn control_failure_is_bounded_without_leaking_backend_output() {
    let Harness {
        control, adapter, ..
    } = harness().await;
    // The failing command emits secret-shaped bytes to stdout and stderr.
    control.fail_op(
        "add_vhost",
        "SUPER_SECRET_CREDENTIAL leaked vhost=secretvhost user=secretuser",
    );
    // The adapter surfaces only the payload-free error type.
    assert_eq!(
        adapter.create("attempt_r".into(), "nonce_r".into()).await,
        Err(AdapterError::Rejected)
    );
    wait_for_empty_creations(&adapter).await;

    // Through the protocol, the response is the bounded revision-1 error and
    // carries none of the backend's stdout/stderr.
    let response = rabbit_dispatch(
        Request::CreateFresh {
            revision: CONTROL_PROTOCOL_REVISION,
            attempt_id: "attempt_r2".into(),
            lease_nonce: "nonce_r2".into(),
        },
        adapter.clone(),
    )
    .await;
    assert_eq!(response, error_response("rejected"));
    let serialized = serde_json::to_string(&response).unwrap();
    assert!(!serialized.contains("SUPER_SECRET_CREDENTIAL"));
    assert!(!serialized.contains("secretvhost"));
    assert!(!serialized.contains("secretuser"));
    wait_for_empty_creations(&adapter).await;
}

// ----- AC4: a control call exceeding the deadline is killed and rolled back -----

#[tokio::test]
async fn control_deadline_expiry_is_bounded_and_rolls_back() {
    let control = FakeControl::new();
    let amqp = FakeAmqp::start(control.state.clone()).await;
    let adapter = RabbitAdapter::new(
        &amqp.admin_url(),
        &control.program(),
        vec!["AMQP_URL".into()],
    )
    .unwrap()
    .with_operation_deadline(Duration::from_secs(1));
    // add_vhost sleeps far past the deadline (before creating anything), so the
    // subprocess is killed on drop and nothing is left to clean up.
    control.slow_op("add_vhost", "30");
    assert_eq!(
        adapter.create("attempt_t".into(), "nonce_t".into()).await,
        Err(AdapterError::Rejected)
    );
    wait_for_empty_creations(&adapter).await;
    assert!(adapter.leases.lock().await.is_empty());
    assert!(control.vhosts().is_empty());
    assert!(control.users().is_empty());
}

// ----- Suppress unused-field warnings for harness fields kept for lifetime -----

#[tokio::test]
async fn harness_fields_are_retained_for_backend_lifetime() {
    let harness = harness().await;
    assert!(harness.amqp.port > 0);
    assert!(!harness.control.program().is_empty());
    assert!(harness.adapter.leases.lock().await.is_empty());
}
