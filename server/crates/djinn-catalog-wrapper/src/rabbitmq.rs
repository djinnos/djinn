//! Protocol-v1 RabbitMQ catalog wrapper: one vhost + one vhost-scoped user per
//! lease.
//!
//! Each lease is a dedicated vhost plus a dedicated user whose configure/write/
//! read permissions exist on that vhost alone, so a RabbitMQ user with no
//! permissions on any other vhost is structurally isolated. Provisioning and
//! teardown use fixed-argument local `rabbitmqctl` subprocesses (never a network
//! management endpoint, shell, Kubernetes exec, or operator AMQP credentials),
//! compatible with the catalog's management-plugin-free `rabbitmq:4-alpine`
//! runtime. Readiness proves the lease credentials open AMQP against the lease
//! vhost by declaring and deleting a probe queue.
//!
//! Like the Postgres (g3fq) and Redis (2o5t) halves, provisioning/rollback are
//! owned by a background task (never the request future or a mutex held across
//! subprocess I/O), the request identity is the CreateFresh idempotency key, and
//! the returned URL is self-describing: its userinfo and `/<vhost>` path name the
//! lease's own namespace. Only catalog-configured environment names are returned,
//! and every bounded error is the payload-free [`AdapterError`] — no credential,
//! generated vhost/user, raw stderr, or backend command output can enter it.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

use djinn_sandbox::service_provisioning::{CONTROL_PROTOCOL_REVISION, Request, Response};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpStream, UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify};
use url::Url;

use crate::{
    AdapterError, CreatedLease, CreationKey, LEASE_PREFIX, MAX_LINE_LEN, OPERATION_DEADLINE,
    error_response, generated, random_password, request_revision, signal_completion,
    valid_environment_name, valid_identifier,
};

const RABBIT_VHOST_PREFIX: &str = "djinn_vhost_";
const RABBIT_USER_PREFIX: &str = "djinn_rabbit_user_";
const RABBIT_PROBE_PREFIX: &str = "djinn_probe_";
const MAX_RABBIT_LEASES: usize = 1024;
/// Bytes retained from a local-control subprocess. Output beyond this is drained
/// (so the child never blocks on a full pipe) but discarded; captured bytes are
/// only ever inspected in-process to answer membership questions, never logged.
const MAX_CONTROL_OUTPUT: usize = 64 * 1024;
const DEFAULT_RABBIT_CONTROL: &str = "rabbitmqctl";

const AMQP_FRAME_METHOD: u8 = 1;
const AMQP_FRAME_END: u8 = 0xCE;
const AMQP_MAX_FRAME: usize = 128 * 1024;

#[derive(Clone)]
pub struct RabbitAdapter {
    endpoint: AmqpEndpoint,
    control_program: String,
    environment_names: Vec<String>,
    leases: Arc<Mutex<HashMap<String, RabbitLease>>>,
    creations: Arc<Mutex<HashMap<CreationKey, RabbitCreationState>>>,
    operation_deadline: Duration,
    #[cfg(test)]
    fail_rollback_attempts: Arc<AtomicUsize>,
}

/// AMQP data-plane endpoint the lease URL advertises and readiness probes. Only
/// host and port are taken from the admin URL; per-lease credentials and the
/// per-lease vhost replace any userinfo/path it carries.
#[derive(Clone)]
struct AmqpEndpoint {
    host: String,
    port: u16,
}

#[derive(Clone)]
struct RabbitLease {
    user: String,
    vhost: String,
    password: String,
}

/// Cleanup authority retained when provisioning may have created a vhost and/or
/// user. Passwords are never retained because cleanup does not need them.
#[derive(Clone)]
struct RabbitPartialTenant {
    user: String,
    vhost: String,
}

impl From<&RabbitLease> for RabbitPartialTenant {
    fn from(lease: &RabbitLease) -> Self {
        Self {
            user: lease.user.clone(),
            vhost: lease.vhost.clone(),
        }
    }
}

enum RabbitCreationState {
    Creating(Arc<Notify>),
    Created(CreatedLease),
    CleanupPending(RabbitPartialTenant),
}

impl AmqpEndpoint {
    fn parse(admin_url: &str) -> Result<Self, AdapterError> {
        let url = Url::parse(admin_url).map_err(|_| AdapterError::Rejected)?;
        if url.scheme() != "amqp" {
            return Err(AdapterError::Rejected);
        }
        Ok(Self {
            host: url.host_str().ok_or(AdapterError::Rejected)?.to_owned(),
            port: url.port().unwrap_or(5672),
        })
    }
}

impl RabbitAdapter {
    pub fn new(
        admin_url: &str,
        control_program: &str,
        mut environment_names: Vec<String>,
    ) -> Result<Self, AdapterError> {
        if control_program.is_empty()
            || environment_names.is_empty()
            || environment_names
                .iter()
                .any(|name| !valid_environment_name(name))
        {
            return Err(AdapterError::Rejected);
        }
        environment_names.sort();
        environment_names.dedup();
        Ok(Self {
            endpoint: AmqpEndpoint::parse(admin_url)?,
            control_program: control_program.to_owned(),
            environment_names,
            leases: Arc::new(Mutex::new(HashMap::new())),
            creations: Arc::new(Mutex::new(HashMap::new())),
            operation_deadline: OPERATION_DEADLINE,
            #[cfg(test)]
            fail_rollback_attempts: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub fn from_environment() -> Result<Self, AdapterError> {
        let admin_url = std::env::var("RABBITMQ_WRAPPER_AMQP_URL")
            .or_else(|_| std::env::var("AMQP_URL"))
            .map_err(|_| AdapterError::Rejected)?;
        let control_program =
            std::env::var("RABBITMQ_WRAPPER_CTL").unwrap_or_else(|_| DEFAULT_RABBIT_CONTROL.into());
        let names = std::env::var("CATALOG_RABBITMQ_ENV_NAMES")
            .unwrap_or_else(|_| "AMQP_URL".to_owned())
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect();
        Self::new(&admin_url, &control_program, names)
    }

    /// CreateFresh mirrors the g3fq/2o5t halves: the request identity is the
    /// idempotency key, provisioning and rollback are owned by a background task
    /// (never the request future or the lease-map mutex, and never held across
    /// subprocess I/O), and a retry that finds retained cleanup authority retries
    /// cleanup rather than orphaning a vhost/user.
    async fn create(
        &self,
        attempt_id: String,
        lease_nonce: String,
    ) -> Result<(String, BTreeMap<String, String>), AdapterError> {
        let key = CreationKey {
            attempt_id,
            lease_nonce,
        };
        let notify = {
            let mut creations = self.creations.lock().await;
            match creations.get(&key) {
                Some(RabbitCreationState::Created(created)) => {
                    return Ok((created.lease_id.clone(), created.environment.clone()));
                }
                Some(RabbitCreationState::Creating(notify)) => notify.clone(),
                Some(RabbitCreationState::CleanupPending(partial)) => {
                    let partial = partial.clone();
                    let notify = Arc::new(Notify::new());
                    creations.insert(key.clone(), RabbitCreationState::Creating(notify.clone()));
                    let adapter = self.clone();
                    let task_key = key.clone();
                    let task_notify = notify.clone();
                    // A retry first retries bounded cleanup. Do not discard the
                    // partial tenant and create another one while it remains.
                    tokio::spawn(async move {
                        adapter.finish_cleanup(task_key, partial, task_notify).await;
                    });
                    notify
                }
                None => {
                    let notify = Arc::new(Notify::new());
                    creations.insert(key.clone(), RabbitCreationState::Creating(notify.clone()));
                    let adapter = self.clone();
                    let task_key = key.clone();
                    let task_notify = notify.clone();
                    // This task, not the request future, owns provisioning and
                    // rollback. A socket deadline cannot cancel cleanup.
                    tokio::spawn(async move {
                        adapter.complete_creation(task_key, task_notify).await;
                    });
                    notify
                }
            }
        };
        self.await_creation(&key, notify).await
    }

    async fn await_creation(
        &self,
        key: &CreationKey,
        mut notify: Arc<Notify>,
    ) -> Result<(String, BTreeMap<String, String>), AdapterError> {
        let deadline = tokio::time::Instant::now() + self.operation_deadline;
        loop {
            let wait_notify = notify.clone();
            let notified = wait_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let current_notify = {
                let creations = self.creations.lock().await;
                match creations.get(key) {
                    Some(RabbitCreationState::Created(created)) => {
                        return Ok((created.lease_id.clone(), created.environment.clone()));
                    }
                    Some(RabbitCreationState::Creating(current_notify)) => current_notify.clone(),
                    Some(RabbitCreationState::CleanupPending(_)) | None => {
                        return Err(AdapterError::Rejected);
                    }
                }
            };

            if !Arc::ptr_eq(&notify, &current_notify) {
                notify = current_notify;
                continue;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return Err(AdapterError::Rejected);
            }
        }
    }

    async fn complete_creation(&self, key: CreationKey, notify: Arc<Notify>) {
        // Admission control: bound the number of concurrently live tenants.
        if self.leases.lock().await.len() >= MAX_RABBIT_LEASES {
            self.creations.lock().await.remove(&key);
            signal_completion(&notify);
            return;
        }
        let Ok((lease_id, lease)) = self.new_lease() else {
            self.creations.lock().await.remove(&key);
            signal_completion(&notify);
            return;
        };
        if self.provision(&lease).await.is_err() {
            self.finish_cleanup(key, RabbitPartialTenant::from(&lease), notify)
                .await;
            return;
        }
        let url = self.lease_url(&lease);
        let environment = self
            .environment_names
            .iter()
            .cloned()
            .map(|name| (name, url.clone()))
            .collect();
        self.leases.lock().await.insert(lease_id.clone(), lease);
        self.creations.lock().await.insert(
            key,
            RabbitCreationState::Created(CreatedLease {
                lease_id,
                environment,
            }),
        );
        signal_completion(&notify);
    }

    async fn finish_cleanup(
        &self,
        key: CreationKey,
        partial: RabbitPartialTenant,
        notify: Arc<Notify>,
    ) {
        let cleanup_succeeded = self.rollback(&partial.vhost, &partial.user).await.is_ok();
        let mut creations = self.creations.lock().await;
        if cleanup_succeeded {
            creations.remove(&key);
        } else {
            // Preserve the generated vhost/user until cleanup succeeds. This is
            // the sole authority capable of removing a partial tenant.
            creations.insert(key, RabbitCreationState::CleanupPending(partial));
        }
        drop(creations);
        signal_completion(&notify);
    }

    fn new_lease(&self) -> Result<(String, RabbitLease), AdapterError> {
        let lease_id = generated(LEASE_PREFIX, 20)?;
        let user = generated(RABBIT_USER_PREFIX, 20)?;
        let vhost = generated(RABBIT_VHOST_PREFIX, 20)?;
        let password = random_password()?;
        Ok((
            lease_id,
            RabbitLease {
                user,
                vhost,
                password,
            },
        ))
    }

    fn lease_url(&self, lease: &RabbitLease) -> String {
        let host = if self.endpoint.host.contains(':') {
            format!("[{}]", self.endpoint.host)
        } else {
            self.endpoint.host.clone()
        };
        // The vhost, user, and URL-safe password are drawn from the safe
        // identifier/password alphabets, so no percent-encoding is required.
        format!(
            "amqp://{}:{}@{}:{}/{}",
            lease.user, lease.password, host, self.endpoint.port, lease.vhost,
        )
    }

    /// Create the vhost, then the user, then grant configure/write/read on that
    /// vhost alone. Ordering matches Delete's teardown (vhost then user).
    async fn provision(&self, lease: &RabbitLease) -> Result<(), AdapterError> {
        let provision = async {
            self.control_ok(&["add_vhost".to_owned(), lease.vhost.clone()])
                .await?;
            self.control_ok(&[
                "add_user".to_owned(),
                lease.user.clone(),
                lease.password.clone(),
            ])
            .await?;
            self.control_ok(&[
                "set_permissions".to_owned(),
                "-p".to_owned(),
                lease.vhost.clone(),
                lease.user.clone(),
                ".*".to_owned(),
                ".*".to_owned(),
                ".*".to_owned(),
            ])
            .await?;
            Ok::<(), AdapterError>(())
        };
        tokio::time::timeout(self.operation_deadline, provision)
            .await
            .ok()
            .and_then(Result::ok)
            .ok_or(AdapterError::Rejected)
    }

    async fn rollback(&self, vhost: &str, user: &str) -> Result<(), AdapterError> {
        #[cfg(test)]
        if self
            .fail_rollback_attempts
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err(AdapterError::Rejected);
        }
        // Cleanup is independently bounded; on timeout its retained partial state
        // makes the next identical request retry safely.
        tokio::time::timeout(OPERATION_DEADLINE, self.cleanup(vhost, user))
            .await
            .map_err(|_| AdapterError::Rejected)?
    }

    /// Drop only this lease's vhost then its user. Existence is checked first so
    /// cleanup is idempotent (a not-yet-created or already-removed entity is
    /// skipped) while a genuine backend failure still surfaces as an error and
    /// retains cleanup authority. Deleting a vhost/user removes only the objects
    /// and permissions bound to it, so other leases are untouched.
    async fn cleanup(&self, vhost: &str, user: &str) -> Result<(), AdapterError> {
        let vhosts = self.control_capture(&["list_vhosts".to_owned()]).await?;
        if control_output_lists(&vhosts, vhost) {
            self.control_ok(&["delete_vhost".to_owned(), vhost.to_owned()])
                .await?;
        }
        let users = self.control_capture(&["list_users".to_owned()]).await?;
        if control_output_lists(&users, user) {
            self.control_ok(&["delete_user".to_owned(), user.to_owned()])
                .await?;
        }
        Ok(())
    }

    /// Run a local-control command and require success, discarding its output.
    async fn control_ok(&self, args: &[String]) -> Result<(), AdapterError> {
        let output = self.run_control(args).await?;
        output.success.then_some(()).ok_or(AdapterError::Rejected)
    }

    /// Run a local-control command, require success, and return captured stdout
    /// for in-process membership checks (never logged, never returned to callers).
    async fn control_capture(&self, args: &[String]) -> Result<Vec<u8>, AdapterError> {
        let output = self.run_control(args).await?;
        if output.success {
            Ok(output.stdout)
        } else {
            Err(AdapterError::Rejected)
        }
    }

    /// Execute a fixed-argument `rabbitmqctl` subprocess with no shell, stdin
    /// closed, and bounded output capture, all under the operation deadline. A
    /// timed-out child is killed on drop. Neither stdout nor stderr is logged.
    async fn run_control(&self, args: &[String]) -> Result<ControlOutput, AdapterError> {
        let program = self.control_program.clone();
        let args = args.to_vec();
        let run = async move {
            let mut child = spawn_control(&program, &args).await?;
            let mut stdout = child.stdout.take().ok_or(AdapterError::Rejected)?;
            let mut stderr = child.stderr.take().ok_or(AdapterError::Rejected)?;
            let mut captured = Vec::new();
            let mut discarded = Vec::new();
            // Drain both pipes concurrently so a chatty child can always exit.
            tokio::join!(
                read_capped(&mut stdout, &mut captured),
                read_capped(&mut stderr, &mut discarded),
            );
            let status = child.wait().await.map_err(|_| AdapterError::Rejected)?;
            Ok::<ControlOutput, AdapterError>(ControlOutput {
                success: status.success(),
                stdout: captured,
            })
        };
        tokio::time::timeout(self.operation_deadline, run)
            .await
            .map_err(|_| AdapterError::Rejected)?
    }

    #[cfg(test)]
    fn fail_next_rollbacks(&self, attempts: usize) {
        self.fail_rollback_attempts
            .store(attempts, Ordering::SeqCst);
    }

    #[cfg(test)]
    fn with_operation_deadline(mut self, deadline: Duration) -> Self {
        self.operation_deadline = deadline;
        self
    }

    async fn ready(&self, lease_id: &str) -> Result<(), AdapterError> {
        let lease = self
            .leases
            .lock()
            .await
            .get(lease_id)
            .cloned()
            .ok_or(AdapterError::Rejected)?;
        let probe = generated(RABBIT_PROBE_PREFIX, 12)?;
        tokio::time::timeout(
            self.operation_deadline,
            amqp_probe(
                &self.endpoint,
                &lease.user,
                &lease.password,
                &lease.vhost,
                &probe,
            ),
        )
        .await
        .map_err(|_| AdapterError::Rejected)?
    }

    async fn delete(&self, lease_id: &str) -> Result<(), AdapterError> {
        // Retried Delete after a successful first call is intentionally a no-op.
        // Keep a lease whose cleanup failed so a caller can retry rather than
        // losing the sole authority to clean up its tenant.
        let Some(lease) = self.leases.lock().await.get(lease_id).cloned() else {
            return Ok(());
        };
        tokio::time::timeout(OPERATION_DEADLINE, self.cleanup(&lease.vhost, &lease.user))
            .await
            .map_err(|_| AdapterError::Rejected)??;
        self.leases.lock().await.remove(lease_id);
        self.creations.lock().await.retain(|_, state| {
            !matches!(state, RabbitCreationState::Created(created) if created.lease_id == lease_id)
        });
        Ok(())
    }
}

/// Spawn a fixed-argument control subprocess with stdin closed and both pipes
/// captured. A just-created executable can transiently exec-fail with `ETXTBSY`
/// when another thread in this process forked while the file was briefly open
/// for writing; a bounded retry absorbs that race. In production the control
/// program is a stable pre-installed binary, so this retry path is effectively
/// never taken. All other spawn failures fail closed. Overall duration remains
/// bounded by the enclosing operation deadline.
async fn spawn_control(
    program: &str,
    args: &[String],
) -> Result<tokio::process::Child, AdapterError> {
    const TEXT_FILE_BUSY: i32 = 26;
    const MAX_ATTEMPTS: usize = 20;
    for attempt in 0..MAX_ATTEMPTS {
        let mut command = tokio::process::Command::new(program);
        command
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        match command.spawn() {
            Ok(child) => return Ok(child),
            Err(error)
                if error.raw_os_error() == Some(TEXT_FILE_BUSY) && attempt + 1 < MAX_ATTEMPTS =>
            {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(_) => return Err(AdapterError::Rejected),
        }
    }
    Err(AdapterError::Rejected)
}

struct ControlOutput {
    success: bool,
    stdout: Vec<u8>,
}

/// Read to EOF, storing at most `MAX_CONTROL_OUTPUT` bytes but always draining
/// the rest so the child process can never block on a full pipe.
async fn read_capped<R>(reader: &mut R, sink: &mut Vec<u8>)
where
    R: AsyncReadExt + Unpin,
{
    let mut chunk = [0u8; 512];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                if sink.len() < MAX_CONTROL_OUTPUT {
                    let room = MAX_CONTROL_OUTPUT - sink.len();
                    sink.extend_from_slice(&chunk[..count.min(room)]);
                }
            }
        }
    }
}

/// True when `token` appears as a whitespace-delimited entry of control output.
/// The lease's vhost/user names are unique random identifiers, so an exact token
/// match is robust across `rabbitmqctl` listing formats and never collides with
/// a header word such as `Listing`.
fn control_output_lists(output: &[u8], token: &str) -> bool {
    String::from_utf8_lossy(output)
        .split_whitespace()
        .any(|entry| entry == token)
}

// ----- Minimal hand-rolled AMQP 0-9-1 readiness client -----
//
// Like the Redis half hand-rolls RESP, the readiness probe hand-rolls just the
// AMQP 0-9-1 connection/channel handshake plus Queue.Declare/Delete over a
// `TcpStream`. This keeps the crate free of a heavyweight async AMQP dependency
// (and avoids the workspace-hack / merge-queue churn adding one would incur)
// while still proving the lease credentials authenticate against the lease vhost.

fn amqp_method_frame(channel: u16, class: u16, method: u16, args: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(4 + args.len());
    payload.extend_from_slice(&class.to_be_bytes());
    payload.extend_from_slice(&method.to_be_bytes());
    payload.extend_from_slice(args);
    let mut frame = Vec::with_capacity(8 + payload.len());
    frame.push(AMQP_FRAME_METHOD);
    frame.extend_from_slice(&channel.to_be_bytes());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    frame.push(AMQP_FRAME_END);
    frame
}

fn amqp_short_string(out: &mut Vec<u8>, value: &str) {
    out.push(value.len() as u8);
    out.extend_from_slice(value.as_bytes());
}

fn amqp_long_string(out: &mut Vec<u8>, value: &[u8]) {
    out.extend_from_slice(&(value.len() as u32).to_be_bytes());
    out.extend_from_slice(value);
}

fn amqp_expect(method: &AmqpMethod, class: u16, id: u16) -> Result<(), AdapterError> {
    (method.class == class && method.method == id)
        .then_some(())
        .ok_or(AdapterError::Rejected)
}

struct AmqpMethod {
    class: u16,
    method: u16,
    args: Vec<u8>,
}

struct Amqp {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl Amqp {
    async fn connect(endpoint: &AmqpEndpoint) -> Result<Self, AdapterError> {
        let stream = TcpStream::connect((endpoint.host.as_str(), endpoint.port))
            .await
            .map_err(|_| AdapterError::Rejected)?;
        let (read, writer) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(read),
            writer,
        })
    }

    async fn write_method(
        &mut self,
        channel: u16,
        class: u16,
        method: u16,
        args: &[u8],
    ) -> Result<(), AdapterError> {
        self.writer
            .write_all(&amqp_method_frame(channel, class, method, args))
            .await
            .map_err(|_| AdapterError::Rejected)
    }

    async fn read_method(&mut self) -> Result<AmqpMethod, AdapterError> {
        let mut header = [0u8; 7];
        self.reader
            .read_exact(&mut header)
            .await
            .map_err(|_| AdapterError::Rejected)?;
        let size = u32::from_be_bytes([header[3], header[4], header[5], header[6]]) as usize;
        if size > AMQP_MAX_FRAME {
            return Err(AdapterError::Rejected);
        }
        let mut payload = vec![0u8; size];
        self.reader
            .read_exact(&mut payload)
            .await
            .map_err(|_| AdapterError::Rejected)?;
        let mut end = [0u8; 1];
        self.reader
            .read_exact(&mut end)
            .await
            .map_err(|_| AdapterError::Rejected)?;
        // Only method frames are expected during the handshake; a server
        // Connection.Close (auth/vhost denial) is a method too and is caught by
        // the class/method expectation at each step.
        if header[0] != AMQP_FRAME_METHOD || end[0] != AMQP_FRAME_END || payload.len() < 4 {
            return Err(AdapterError::Rejected);
        }
        Ok(AmqpMethod {
            class: u16::from_be_bytes([payload[0], payload[1]]),
            method: u16::from_be_bytes([payload[2], payload[3]]),
            args: payload[4..].to_vec(),
        })
    }
}

async fn amqp_probe(
    endpoint: &AmqpEndpoint,
    user: &str,
    password: &str,
    vhost: &str,
    probe_queue: &str,
) -> Result<(), AdapterError> {
    let mut connection = Amqp::connect(endpoint).await?;
    connection
        .writer
        .write_all(b"AMQP\x00\x00\x09\x01")
        .await
        .map_err(|_| AdapterError::Rejected)?;

    amqp_expect(&connection.read_method().await?, 10, 10)?; // Connection.Start

    let mut start_ok = Vec::new();
    amqp_long_string(&mut start_ok, &[]); // client-properties: empty field table
    amqp_short_string(&mut start_ok, "PLAIN");
    let mut response = Vec::with_capacity(user.len() + password.len() + 2);
    response.push(0);
    response.extend_from_slice(user.as_bytes());
    response.push(0);
    response.extend_from_slice(password.as_bytes());
    amqp_long_string(&mut start_ok, &response);
    amqp_short_string(&mut start_ok, "en_US");
    connection.write_method(0, 10, 11, &start_ok).await?; // Connection.Start-Ok

    let tune = connection.read_method().await?;
    amqp_expect(&tune, 10, 30)?; // Connection.Tune
    let (channel_max, frame_max) = if tune.args.len() >= 6 {
        (
            u16::from_be_bytes([tune.args[0], tune.args[1]]),
            u32::from_be_bytes([tune.args[2], tune.args[3], tune.args[4], tune.args[5]]),
        )
    } else {
        (0, 0)
    };
    let mut tune_ok = Vec::new();
    tune_ok.extend_from_slice(&channel_max.to_be_bytes());
    tune_ok.extend_from_slice(&frame_max.to_be_bytes());
    tune_ok.extend_from_slice(&0u16.to_be_bytes()); // heartbeat disabled
    connection.write_method(0, 10, 31, &tune_ok).await?; // Connection.Tune-Ok

    let mut open = Vec::new();
    amqp_short_string(&mut open, vhost);
    amqp_short_string(&mut open, ""); // reserved-1
    open.push(0); // reserved-2
    connection.write_method(0, 10, 40, &open).await?; // Connection.Open
    amqp_expect(&connection.read_method().await?, 10, 41)?; // Connection.Open-Ok

    let mut channel_open = Vec::new();
    amqp_short_string(&mut channel_open, ""); // reserved-1
    connection.write_method(1, 20, 10, &channel_open).await?; // Channel.Open
    amqp_expect(&connection.read_method().await?, 20, 11)?; // Channel.Open-Ok

    let mut declare = Vec::new();
    declare.extend_from_slice(&0u16.to_be_bytes()); // reserved-1
    amqp_short_string(&mut declare, probe_queue);
    declare.push(0b0000_1100); // exclusive + auto-delete
    amqp_long_string(&mut declare, &[]); // arguments: empty field table
    connection.write_method(1, 50, 10, &declare).await?; // Queue.Declare
    amqp_expect(&connection.read_method().await?, 50, 11)?; // Queue.Declare-Ok

    let mut queue_delete = Vec::new();
    queue_delete.extend_from_slice(&0u16.to_be_bytes()); // reserved-1
    amqp_short_string(&mut queue_delete, probe_queue);
    queue_delete.push(0); // no flags
    connection.write_method(1, 50, 40, &queue_delete).await?; // Queue.Delete
    amqp_expect(&connection.read_method().await?, 50, 41)?; // Queue.Delete-Ok

    let mut close = Vec::new();
    close.extend_from_slice(&200u16.to_be_bytes()); // reply-code
    amqp_short_string(&mut close, ""); // reply-text
    close.extend_from_slice(&0u16.to_be_bytes()); // class-id
    close.extend_from_slice(&0u16.to_be_bytes()); // method-id
    connection.write_method(0, 10, 50, &close).await?; // Connection.Close
    let _ = connection.read_method().await; // Connection.Close-Ok (best-effort)
    Ok(())
}

pub struct RabbitWrapperServer {
    adapter: RabbitAdapter,
}

impl RabbitWrapperServer {
    pub fn new(adapter: RabbitAdapter) -> Self {
        Self { adapter }
    }

    pub async fn serve(self, socket: impl AsRef<Path>) -> Result<(), std::io::Error> {
        let socket = socket.as_ref();
        if socket.exists() {
            std::fs::remove_file(socket)?;
        }
        let listener = UnixListener::bind(socket)?;
        loop {
            let (stream, _) = listener.accept().await?;
            let adapter = self.adapter.clone();
            tokio::spawn(async move {
                rabbit_socket(stream, adapter).await;
            });
        }
    }
}

async fn rabbit_socket(stream: UnixStream, adapter: RabbitAdapter) {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    let response = match tokio::time::timeout(
        OPERATION_DEADLINE,
        BufReader::new(read).read_line(&mut line),
    )
    .await
    {
        Ok(Ok(n)) if n > 0 && n <= MAX_LINE_LEN => match serde_json::from_str(&line) {
            Ok(request) => rabbit_dispatch(request, adapter).await,
            Err(_) => error_response("invalid_request"),
        },
        _ => error_response("invalid_request"),
    };
    if let Ok(body) = serde_json::to_vec(&response) {
        let _ = write.write_all(&body).await;
        let _ = write.write_all(b"\n").await;
    }
}

async fn rabbit_dispatch(request: Request, adapter: RabbitAdapter) -> Response {
    if request_revision(&request) != CONTROL_PROTOCOL_REVISION {
        return error_response("revision_mismatch");
    }
    let operation = async {
        match request {
            Request::CreateFresh {
                attempt_id,
                lease_nonce,
                ..
            } if valid_identifier(&attempt_id) && valid_identifier(&lease_nonce) => {
                let (lease_id, environment) = adapter.create(attempt_id, lease_nonce).await?;
                Ok(Response::Created {
                    revision: CONTROL_PROTOCOL_REVISION,
                    lease_id,
                    environment,
                })
            }
            Request::Ready { lease_id, .. } if valid_identifier(&lease_id) => {
                adapter.ready(&lease_id).await?;
                Ok(Response::Ready {
                    revision: CONTROL_PROTOCOL_REVISION,
                })
            }
            Request::Delete { lease_id, .. } if valid_identifier(&lease_id) => {
                adapter.delete(&lease_id).await?;
                Ok(Response::Deleted {
                    revision: CONTROL_PROTOCOL_REVISION,
                })
            }
            _ => Err(AdapterError::Rejected),
        }
    };
    match tokio::time::timeout(OPERATION_DEADLINE, operation).await {
        Ok(Ok(response)) => response,
        Ok(Err(_)) => error_response("rejected"),
        Err(_) => error_response("timeout"),
    }
}

#[cfg(test)]
mod tests;
