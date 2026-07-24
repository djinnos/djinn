//! Redis catalog-service adapter, minimal RESP client, and Unix control-socket
//! server.
//!
//! Each lease is isolated by a per-lease key/channel prefix that is, by
//! convention, the ACL username followed by a colon (`<user>:`); the ACL grants
//! `~<user>:*` and `&<user>:*` and the returned URL carries the same prefix as a
//! `key_prefix` query parameter (see [`redis_key_prefix`]).

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
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

const REDIS_USER_PREFIX: &str = "djinn_redis_user_";
const MAX_REDIS_LEASES: usize = 1024;

/// The per-lease key/channel namespace prefix is defined to be the ACL username
/// followed by a colon. A single generated identifier therefore names both the
/// credential and the namespace: the ACL grants `~<user>:*` and `&<user>:*`, and
/// the returned URL carries the same prefix verbatim as a `key_prefix` query
/// parameter so the lease holder can construct in-namespace keys and channels
/// from the returned value alone. This mirrors g3fq, where the returned Postgres
/// URL is likewise self-describing (its database is the lease's own namespace).
///
/// Only catalog-configured environment names are ever returned; the prefix is
/// communicated inside the connection URL rather than as an invented variable.
fn redis_key_prefix(user: &str) -> String {
    format!("{user}:")
}

/// The ACL/SCAN glob for a lease: every key and channel the lease may touch.
fn redis_scope_pattern(user: &str) -> String {
    format!("{user}:*")
}

#[derive(Clone)]
pub struct RedisAdapter {
    endpoint: Endpoint,
    environment_names: Vec<String>,
    leases: Arc<Mutex<HashMap<String, RedisLease>>>,
    creations: Arc<Mutex<HashMap<CreationKey, RedisCreationState>>>,
    operation_deadline: Duration,
    #[cfg(test)]
    pause_after_user: Arc<Mutex<Option<Duration>>>,
    #[cfg(test)]
    fail_rollback_attempts: Arc<AtomicUsize>,
}

#[derive(Clone)]
struct Endpoint {
    host: String,
    port: u16,
    user: Option<String>,
    password: Option<String>,
}

#[derive(Clone)]
struct RedisLease {
    user: String,
    password: String,
}

/// Cleanup authority retained when provisioning may have created an ACL user.
/// The namespace prefix is derived from the username, so no separate field is
/// needed. Passwords are never retained here because cleanup does not need them.
#[derive(Clone)]
struct RedisPartialTenant {
    user: String,
}

impl From<&RedisLease> for RedisPartialTenant {
    fn from(lease: &RedisLease) -> Self {
        Self {
            user: lease.user.clone(),
        }
    }
}

enum RedisCreationState {
    Creating(Arc<Notify>),
    Created(CreatedLease),
    CleanupPending(RedisPartialTenant),
}

impl Endpoint {
    fn parse(admin_url: &str) -> Result<Self, AdapterError> {
        let url = Url::parse(admin_url).map_err(|_| AdapterError::Rejected)?;
        if url.scheme() != "redis" || !matches!(url.path(), "" | "/") {
            return Err(AdapterError::Rejected);
        }
        Ok(Self {
            host: url.host_str().ok_or(AdapterError::Rejected)?.to_owned(),
            port: url.port().unwrap_or(6379),
            user: (!url.username().is_empty()).then(|| url.username().to_owned()),
            password: url.password().map(str::to_owned),
        })
    }
}

impl RedisAdapter {
    pub fn new(admin_url: &str, mut environment_names: Vec<String>) -> Result<Self, AdapterError> {
        if environment_names.is_empty()
            || environment_names
                .iter()
                .any(|name| !valid_environment_name(name))
        {
            return Err(AdapterError::Rejected);
        }
        environment_names.sort();
        environment_names.dedup();
        Ok(Self {
            endpoint: Endpoint::parse(admin_url)?,
            environment_names,
            leases: Arc::new(Mutex::new(HashMap::new())),
            creations: Arc::new(Mutex::new(HashMap::new())),
            operation_deadline: OPERATION_DEADLINE,
            #[cfg(test)]
            pause_after_user: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            fail_rollback_attempts: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub fn from_environment() -> Result<Self, AdapterError> {
        let admin_url = std::env::var("REDIS_WRAPPER_ADMIN_URL")
            .or_else(|_| std::env::var("REDIS_URL"))
            .map_err(|_| AdapterError::Rejected)?;
        let names = std::env::var("CATALOG_REDIS_ENV_NAMES")
            .unwrap_or_else(|_| "REDIS_URL".to_owned())
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect();
        Self::new(&admin_url, names)
    }

    /// CreateFresh mirrors g3fq's Postgres half: the request identity is the
    /// idempotency key, provisioning and rollback are owned by a background task
    /// (never the request future or the lease-map mutex), and a retry that finds
    /// retained cleanup authority retries cleanup rather than orphaning a tenant.
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
                Some(RedisCreationState::Created(created)) => {
                    return Ok((created.lease_id.clone(), created.environment.clone()));
                }
                Some(RedisCreationState::Creating(notify)) => notify.clone(),
                Some(RedisCreationState::CleanupPending(partial)) => {
                    let partial = partial.clone();
                    let notify = Arc::new(Notify::new());
                    creations.insert(key.clone(), RedisCreationState::Creating(notify.clone()));
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
                    creations.insert(key.clone(), RedisCreationState::Creating(notify.clone()));
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
                    Some(RedisCreationState::Created(created)) => {
                        return Ok((created.lease_id.clone(), created.environment.clone()));
                    }
                    Some(RedisCreationState::Creating(current_notify)) => current_notify.clone(),
                    Some(RedisCreationState::CleanupPending(_)) | None => {
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
        if self.leases.lock().await.len() >= MAX_REDIS_LEASES {
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
            self.finish_cleanup(key, RedisPartialTenant::from(&lease), notify)
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
            RedisCreationState::Created(CreatedLease {
                lease_id,
                environment,
            }),
        );
        signal_completion(&notify);
    }

    async fn finish_cleanup(
        &self,
        key: CreationKey,
        partial: RedisPartialTenant,
        notify: Arc<Notify>,
    ) {
        let cleanup_succeeded = self.rollback(&partial.user).await.is_ok();
        let mut creations = self.creations.lock().await;
        if cleanup_succeeded {
            creations.remove(&key);
        } else {
            // Preserve the generated username until cleanup succeeds. This is the
            // sole authority capable of removing a partial tenant.
            creations.insert(key, RedisCreationState::CleanupPending(partial));
        }
        drop(creations);
        signal_completion(&notify);
    }

    fn new_lease(&self) -> Result<(String, RedisLease), AdapterError> {
        let lease_id = generated(LEASE_PREFIX, 20)?;
        let user = generated(REDIS_USER_PREFIX, 20)?;
        let password = random_password()?;
        Ok((lease_id, RedisLease { user, password }))
    }

    fn lease_url(&self, lease: &RedisLease) -> String {
        let host = if self.endpoint.host.contains(':') {
            format!("[{}]", self.endpoint.host)
        } else {
            self.endpoint.host.clone()
        };
        format!(
            "redis://{}:{}@{}:{}?key_prefix={}",
            lease.user,
            lease.password,
            host,
            self.endpoint.port,
            redis_key_prefix(&lease.user),
        )
    }

    async fn provision(&self, lease: &RedisLease) -> Result<(), AdapterError> {
        let provision = async {
            let mut admin = self.admin().await?;
            let pattern = redis_scope_pattern(&lease.user);
            let mut command = vec![
                "ACL".to_owned(),
                "SETUSER".to_owned(),
                lease.user.clone(),
                "reset".to_owned(),
                "on".to_owned(),
                format!(">{}", lease.password),
                format!("~{pattern}"),
                format!("&{pattern}"),
                "-@all".to_owned(),
            ];
            command.extend(
                REDIS_ALLOWED_COMMANDS
                    .iter()
                    .map(|allowed| (*allowed).to_owned()),
            );
            admin.cmd(&command).await.map(|_| ())?;
            #[cfg(test)]
            if let Some(pause) = *self.pause_after_user.lock().await {
                tokio::time::sleep(pause).await;
            }
            Ok::<(), AdapterError>(())
        };
        tokio::time::timeout(self.operation_deadline, provision)
            .await
            .ok()
            .and_then(Result::ok)
            .ok_or(AdapterError::Rejected)
    }

    async fn rollback(&self, user: &str) -> Result<(), AdapterError> {
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
        tokio::time::timeout(OPERATION_DEADLINE, async {
            let mut admin = self.admin().await?;
            purge_namespace(&mut admin, user).await?;
            delete_user(&mut admin, user).await
        })
        .await
        .map_err(|_| AdapterError::Rejected)?
    }

    #[cfg(test)]
    async fn pause_after_creating_user(&self, pause: Duration) {
        *self.pause_after_user.lock().await = Some(pause);
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
        tokio::time::timeout(self.operation_deadline, async {
            Conn::open(
                &self.endpoint,
                Some(&lease.user),
                Some(&lease.password),
                self.operation_deadline,
            )
            .await?
            .cmd(&["PING".to_owned()])
            .await
            .map(|_| ())
        })
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
        tokio::time::timeout(OPERATION_DEADLINE, async {
            let mut admin = self.admin().await?;
            purge_namespace(&mut admin, &lease.user).await?;
            delete_user(&mut admin, &lease.user).await
        })
        .await
        .map_err(|_| AdapterError::Rejected)??;
        self.leases.lock().await.remove(lease_id);
        self.creations.lock().await.retain(|_, state| {
            !matches!(state, RedisCreationState::Created(created) if created.lease_id == lease_id)
        });
        Ok(())
    }

    async fn admin(&self) -> Result<Conn, AdapterError> {
        Conn::open(
            &self.endpoint,
            self.endpoint.user.as_deref(),
            self.endpoint.password.as_deref(),
            self.operation_deadline,
        )
        .await
    }
}

/// SCAN-and-UNLINK only the lease's own prefix. Never FLUSHDB/FLUSHALL, which
/// would destroy every other lease sharing the backend.
async fn purge_namespace(admin: &mut Conn, user: &str) -> Result<(), AdapterError> {
    let mut cursor = "0".to_owned();
    loop {
        let reply = admin
            .cmd(&[
                "SCAN".to_owned(),
                cursor,
                "MATCH".to_owned(),
                redis_scope_pattern(user),
                "COUNT".to_owned(),
                "100".to_owned(),
            ])
            .await?;
        let Resp::Array(entries) = reply else {
            return Err(AdapterError::Rejected);
        };
        let [next, Resp::Array(keys)] = entries.as_slice() else {
            return Err(AdapterError::Rejected);
        };
        cursor = next.text().ok_or(AdapterError::Rejected)?.to_owned();
        let mut unlink = vec!["UNLINK".to_owned()];
        for key in keys {
            unlink.push(key.text().ok_or(AdapterError::Rejected)?.to_owned());
        }
        if unlink.len() > 1 {
            admin.cmd(&unlink).await?;
        }
        if cursor == "0" {
            break;
        }
    }
    Ok(())
}

/// Delete only this lease's ACL user. Never touches any other user.
async fn delete_user(admin: &mut Conn, user: &str) -> Result<(), AdapterError> {
    admin
        .cmd(&["ACL".to_owned(), "DELUSER".to_owned(), user.to_owned()])
        .await
        .map(|_| ())
}

const REDIS_ALLOWED_COMMANDS: &[&str] = &[
    "+ping",
    "+get",
    "+set",
    "+del",
    "+unlink",
    "+exists",
    "+expire",
    "+ttl",
    "+mget",
    "+mset",
    "+hget",
    "+hset",
    "+hdel",
    "+lpush",
    "+rpush",
    "+lpop",
    "+rpop",
    "+sadd",
    "+srem",
    "+zadd",
    "+zrem",
    "+zrange",
    "+scan",
    "+sscan",
    "+hscan",
    "+zscan",
    "+publish",
    "+subscribe",
    "+psubscribe",
    "+unsubscribe",
    "+punsubscribe",
]; // Explicitly no ACL, CONFIG, SCRIPT, MODULE, FUNCTION, FLUSHDB, FLUSHALL, or admin command.

/// A minimal RESP client over a single Redis connection. Every socket operation
/// is bounded by `deadline` so neither a stalled connect nor a stalled read can
/// outlive the enclosing operation deadline.
struct Conn {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    deadline: Duration,
}

/// The RESP reply subset the wrapper consumes. Bulk and simple strings collapse
/// to `Text`; integers carry no value because no wrapper command needs one.
enum Resp {
    Text(String),
    Integer,
    Array(Vec<Resp>),
}

impl Resp {
    fn text(&self) -> Option<&str> {
        match self {
            Self::Text(value) => Some(value),
            _ => None,
        }
    }
}

impl Conn {
    async fn open(
        endpoint: &Endpoint,
        user: Option<&str>,
        password: Option<&str>,
        deadline: Duration,
    ) -> Result<Self, AdapterError> {
        let stream = tokio::time::timeout(
            deadline,
            TcpStream::connect((endpoint.host.as_str(), endpoint.port)),
        )
        .await
        .map_err(|_| AdapterError::Rejected)?
        .map_err(|_| AdapterError::Rejected)?;
        let (read, write) = stream.into_split();
        let mut connection = Self {
            reader: BufReader::new(read),
            writer: write,
            deadline,
        };
        if let Some(password) = password {
            let auth = match user {
                Some(user) => vec!["AUTH".to_owned(), user.to_owned(), password.to_owned()],
                None => vec!["AUTH".to_owned(), password.to_owned()],
            };
            connection.cmd(&auth).await?;
        }
        Ok(connection)
    }

    async fn cmd(&mut self, arguments: &[String]) -> Result<Resp, AdapterError> {
        tokio::time::timeout(self.deadline, self.exchange(arguments))
            .await
            .map_err(|_| AdapterError::Rejected)?
    }

    async fn exchange(&mut self, arguments: &[String]) -> Result<Resp, AdapterError> {
        let mut buffer = format!("*{}\r\n", arguments.len()).into_bytes();
        for argument in arguments {
            buffer.extend_from_slice(format!("${}\r\n", argument.len()).as_bytes());
            buffer.extend_from_slice(argument.as_bytes());
            buffer.extend_from_slice(b"\r\n");
        }
        self.writer
            .write_all(&buffer)
            .await
            .map_err(|_| AdapterError::Rejected)?;
        read_resp(&mut self.reader).await
    }
}

fn read_resp<'a, R>(
    reader: &'a mut R,
) -> Pin<Box<dyn Future<Output = Result<Resp, AdapterError>> + Send + 'a>>
where
    R: AsyncBufReadExt + AsyncReadExt + Unpin + Send,
{
    Box::pin(async move {
        let mut tag = [0u8; 1];
        reader
            .read_exact(&mut tag)
            .await
            .map_err(|_| AdapterError::Rejected)?;
        let mut header = String::new();
        reader
            .read_line(&mut header)
            .await
            .map_err(|_| AdapterError::Rejected)?;
        let header = header.strip_suffix("\r\n").ok_or(AdapterError::Rejected)?;
        match tag[0] {
            b'+' => Ok(Resp::Text(header.to_owned())),
            b':' => Ok(Resp::Integer),
            b'-' => Err(AdapterError::Rejected),
            b'$' => {
                let length: usize = header.parse().map_err(|_| AdapterError::Rejected)?;
                let mut body = vec![0u8; length + 2];
                reader
                    .read_exact(&mut body)
                    .await
                    .map_err(|_| AdapterError::Rejected)?;
                Ok(Resp::Text(
                    std::str::from_utf8(&body[..length])
                        .map_err(|_| AdapterError::Rejected)?
                        .to_owned(),
                ))
            }
            b'*' => {
                let count: usize = header.parse().map_err(|_| AdapterError::Rejected)?;
                let mut items = Vec::with_capacity(count);
                for _ in 0..count {
                    items.push(read_resp(reader).await?);
                }
                Ok(Resp::Array(items))
            }
            _ => Err(AdapterError::Rejected),
        }
    })
}

pub struct RedisWrapperServer {
    adapter: RedisAdapter,
}
impl RedisWrapperServer {
    pub fn new(adapter: RedisAdapter) -> Self {
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
                redis_socket(stream, adapter).await;
            });
        }
    }
}
async fn redis_socket(stream: UnixStream, adapter: RedisAdapter) {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    let response = match tokio::time::timeout(
        OPERATION_DEADLINE,
        BufReader::new(read).read_line(&mut line),
    )
    .await
    {
        Ok(Ok(n)) if n > 0 && n <= MAX_LINE_LEN => match serde_json::from_str(&line) {
            Ok(request) => redis_dispatch(request, adapter).await,
            Err(_) => error_response("invalid_request"),
        },
        _ => error_response("invalid_request"),
    };
    if let Ok(body) = serde_json::to_vec(&response) {
        let _ = write.write_all(&body).await;
        let _ = write.write_all(b"\n").await;
    }
}
async fn redis_dispatch(request: Request, adapter: RedisAdapter) -> Response {
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
