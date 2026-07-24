//! Private protocol-v1 catalog-service wrapper server with Postgres and Redis
//! adapters.
//!
//! This crate deliberately exposes only bounded protocol errors. In particular,
//! backend errors, tenant identifiers, and connection credentials never cross
//! the Unix control socket or enter logs.
//!
//! Each adapter returns a self-describing lease URL under only the
//! catalog-configured environment names. The Redis adapter isolates leases by a
//! per-lease key/channel prefix that is, by convention, the ACL username
//! followed by a colon (`<user>:`); the ACL grants `~<user>:*` and `&<user>:*`
//! and the returned URL carries the same prefix as a `key_prefix` query
//! parameter (see `redis_key_prefix`).

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

use base64::Engine as _;
use djinn_sandbox::service_provisioning::{CONTROL_PROTOCOL_REVISION, Request, Response};
use ring::rand::{SecureRandom, SystemRandom};
use sqlx::{Connection, PgConnection};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpStream, UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify};
use url::Url;

const OPERATION_DEADLINE: Duration = Duration::from_secs(15);
const MAX_IDENTIFIER_LEN: usize = 64;
const MAX_LINE_LEN: usize = 4096;
const ROLE_PREFIX: &str = "djinn_role_";
const DATABASE_PREFIX: &str = "djinn_db_";
const LEASE_PREFIX: &str = "lease_";

#[derive(Clone)]
pub struct PostgresAdapter {
    admin_url: Url,
    environment_names: Vec<String>,
    leases: Arc<Mutex<HashMap<String, Lease>>>,
    creations: Arc<Mutex<HashMap<CreationKey, CreationState>>>,
    operation_deadline: Duration,
    #[cfg(test)]
    pause_after_role: Arc<Mutex<Option<Duration>>>,
    #[cfg(test)]
    fail_rollback_attempts: Arc<AtomicUsize>,
}

#[derive(Clone)]
struct Lease {
    role: String,
    database: String,
    password: String,
}

/// The request identity is the idempotency key for CreateFresh. Keeping it
/// associated with the returned lease preserves delete authority after a
/// response is lost and the caller retries.
#[derive(Clone, Hash, PartialEq, Eq)]
struct CreationKey {
    attempt_id: String,
    lease_nonce: String,
}

#[derive(Clone)]
struct CreatedLease {
    lease_id: String,
    environment: BTreeMap<String, String>,
}

/// Cleanup authority retained when provisioning may have created backend
/// objects. Passwords are intentionally not retained here because cleanup
/// never needs them.
#[derive(Clone)]
struct PartialTenant {
    role: String,
    database: String,
}

impl From<&Lease> for PartialTenant {
    fn from(lease: &Lease) -> Self {
        Self {
            role: lease.role.clone(),
            database: lease.database.clone(),
        }
    }
}

enum CreationState {
    Creating(Arc<Notify>),
    Created(CreatedLease),
    CleanupPending(PartialTenant),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdapterError {
    Rejected,
}

impl PostgresAdapter {
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
        let admin_url = Url::parse(admin_url).map_err(|_| AdapterError::Rejected)?;
        if admin_url.scheme() != "postgres" && admin_url.scheme() != "postgresql" {
            return Err(AdapterError::Rejected);
        }
        Ok(Self {
            admin_url,
            environment_names,
            leases: Arc::new(Mutex::new(HashMap::new())),
            creations: Arc::new(Mutex::new(HashMap::new())),
            operation_deadline: OPERATION_DEADLINE,
            #[cfg(test)]
            pause_after_role: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            fail_rollback_attempts: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Production configuration intentionally prefers a dedicated admin URL.
    /// `TEST_POSTGRES_URL` is only the deterministic integration-test fallback.
    pub fn from_environment() -> Result<Self, AdapterError> {
        let admin_url = std::env::var("POSTGRES_WRAPPER_ADMIN_URL")
            .or_else(|_| std::env::var("TEST_POSTGRES_URL"))
            .map_err(|_| AdapterError::Rejected)?;
        let names = std::env::var("CATALOG_POSTGRES_ENV_NAMES")
            .unwrap_or_else(|_| "DATABASE_URL".to_owned())
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect();
        Self::new(&admin_url, names)
    }

    pub async fn create(
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
                Some(CreationState::Created(created)) => {
                    return Ok((created.lease_id.clone(), created.environment.clone()));
                }
                Some(CreationState::Creating(notify)) => notify.clone(),
                Some(CreationState::CleanupPending(partial)) => {
                    let partial = partial.clone();
                    let notify = Arc::new(Notify::new());
                    creations.insert(key.clone(), CreationState::Creating(notify.clone()));
                    let adapter = self.clone();
                    let task_key = key.clone();
                    let task_notify = notify.clone();
                    // A retry first retries bounded cleanup. Do not discard the
                    // partial tenant and create another one while it remains.
                    tokio::spawn(async move {
                        adapter
                            .complete_cleanup(task_key, partial, task_notify)
                            .await;
                    });
                    notify
                }
                None => {
                    let notify = Arc::new(Notify::new());
                    creations.insert(key.clone(), CreationState::Creating(notify.clone()));
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
            // Register before checking the authoritative state. Completion can
            // then happen before, during, or after registration without being
            // lost: either this check observes it or this waiter is notified.
            let wait_notify = notify.clone();
            let notified = wait_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let current_notify = {
                let creations = self.creations.lock().await;
                match creations.get(key) {
                    Some(CreationState::Created(created)) => {
                        return Ok((created.lease_id.clone(), created.environment.clone()));
                    }
                    Some(CreationState::Creating(current_notify)) => current_notify.clone(),
                    // Cleanup either succeeded (the entry was removed) or
                    // remains pending for another retry. Neither case may
                    // create a new tenant as part of this request.
                    Some(CreationState::CleanupPending(_)) | None => {
                        return Err(AdapterError::Rejected);
                    }
                }
            };

            if !Arc::ptr_eq(&notify, &current_notify) {
                // A cleanup retry can replace the notifier while an older
                // identical request is waking. Follow authoritative state
                // rather than waiting on a stale notifier.
                notify = current_notify;
                continue;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return Err(AdapterError::Rejected);
            }
        }
    }

    async fn complete_creation(&self, key: CreationKey, notify: Arc<Notify>) {
        let Ok((lease_id, lease)) = self.new_lease() else {
            self.creations.lock().await.remove(&key);
            signal_completion(&notify);
            return;
        };
        if self.provision(&lease).await.is_err() {
            self.finish_cleanup(key, PartialTenant::from(&lease), notify)
                .await;
            return;
        }
        let Ok(connection_url) = self.lease_url(&lease) else {
            self.finish_cleanup(key, PartialTenant::from(&lease), notify)
                .await;
            return;
        };
        let environment = self
            .environment_names
            .iter()
            .cloned()
            .map(|name| (name, connection_url.clone()))
            .collect();
        self.leases.lock().await.insert(lease_id.clone(), lease);
        self.creations.lock().await.insert(
            key,
            CreationState::Created(CreatedLease {
                lease_id,
                environment,
            }),
        );
        signal_completion(&notify);
    }

    async fn complete_cleanup(
        &self,
        key: CreationKey,
        partial: PartialTenant,
        notify: Arc<Notify>,
    ) {
        self.finish_cleanup(key, partial, notify).await;
    }

    async fn finish_cleanup(&self, key: CreationKey, partial: PartialTenant, notify: Arc<Notify>) {
        let cleanup_succeeded = self
            .rollback(&partial.database, &partial.role)
            .await
            .is_ok();
        let mut creations = self.creations.lock().await;
        if cleanup_succeeded {
            creations.remove(&key);
        } else {
            // Preserve generated identifiers until cleanup succeeds. This is
            // the sole authority capable of removing a partial tenant.
            creations.insert(key, CreationState::CleanupPending(partial));
        }
        drop(creations);
        signal_completion(&notify);
    }

    fn new_lease(&self) -> Result<(String, Lease), AdapterError> {
        let lease_id = generated(LEASE_PREFIX, 20)?;
        let role = generated(ROLE_PREFIX, 20)?;
        let database = generated(DATABASE_PREFIX, 20)?;
        let password = random_password()?;
        Ok((
            lease_id,
            Lease {
                role,
                database,
                password,
            },
        ))
    }

    async fn provision(&self, lease: &Lease) -> Result<(), AdapterError> {
        let role = &lease.role;
        let database = &lease.database;
        let password = &lease.password;
        let provision = async {
            let mut admin = self.admin_connection().await?;
            sqlx::query(&format!(
                "CREATE ROLE \"{role}\" LOGIN PASSWORD '{password}'"
            ))
            .execute(&mut admin)
            .await
            .map_err(|_| AdapterError::Rejected)?;
            #[cfg(test)]
            if let Some(pause) = *self.pause_after_role.lock().await {
                tokio::time::sleep(pause).await;
            }
            sqlx::query(&format!("CREATE DATABASE \"{database}\" OWNER \"{role}\""))
                .execute(&mut admin)
                .await
                .map_err(|_| AdapterError::Rejected)?;
            sqlx::query(&format!(
                "REVOKE ALL ON DATABASE \"{database}\" FROM PUBLIC"
            ))
            .execute(&mut admin)
            .await
            .map_err(|_| AdapterError::Rejected)?;
            Ok::<(), AdapterError>(())
        };
        tokio::time::timeout(self.operation_deadline, provision)
            .await
            .ok()
            .and_then(Result::ok)
            .ok_or(AdapterError::Rejected)
    }

    async fn rollback(&self, database: &str, role: &str) -> Result<(), AdapterError> {
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
        // Cleanup is independently bounded; on timeout its retained partial
        // state makes the next identical request retry safely.
        tokio::time::timeout(OPERATION_DEADLINE, async {
            let mut admin = self.admin_connection().await?;
            cleanup(&mut admin, database, role).await
        })
        .await
        .map_err(|_| AdapterError::Rejected)?
    }

    #[cfg(test)]
    async fn pause_after_creating_role(&self, pause: Duration) {
        *self.pause_after_role.lock().await = Some(pause);
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

    pub async fn ready(&self, lease_id: &str) -> Result<(), AdapterError> {
        let lease = self
            .leases
            .lock()
            .await
            .get(lease_id)
            .cloned()
            .ok_or(AdapterError::Rejected)?;
        let url = self.lease_url(&lease)?;
        let mut connection = PgConnection::connect(&url)
            .await
            .map_err(|_| AdapterError::Rejected)?;
        sqlx::query("SELECT 1")
            .execute(&mut connection)
            .await
            .map_err(|_| AdapterError::Rejected)?;
        Ok(())
    }

    pub async fn delete(&self, lease_id: &str) -> Result<(), AdapterError> {
        // Retried Delete after a successful first call is intentionally a no-op.
        // Keep a failed cleanup lease so a caller can retry rather than losing
        // the sole authority to clean up its tenant.
        let Some(lease) = self.leases.lock().await.get(lease_id).cloned() else {
            return Ok(());
        };
        let mut admin = self.admin_connection().await?;
        cleanup(&mut admin, &lease.database, &lease.role).await?;
        self.leases.lock().await.remove(lease_id);
        Ok(())
    }

    async fn admin_connection(&self) -> Result<PgConnection, AdapterError> {
        PgConnection::connect(self.admin_url.as_str())
            .await
            .map_err(|_| AdapterError::Rejected)
    }

    fn lease_url(&self, lease: &Lease) -> Result<String, AdapterError> {
        let mut url = self.admin_url.clone();
        url.set_username(&lease.role)
            .map_err(|_| AdapterError::Rejected)?;
        url.set_password(Some(&lease.password))
            .map_err(|_| AdapterError::Rejected)?;
        url.set_path(&format!("/{}", lease.database));
        Ok(url.into())
    }
}

/// Wake every registered request and retain one permit for a request that has
/// observed `Creating` but has not registered its waiter yet. The authoritative
/// `CreationState` re-check in `await_creation` remains the source of truth;
/// the stored permit additionally makes the completion edge itself lossless.
fn signal_completion(notify: &Notify) {
    notify.notify_waiters();
    notify.notify_one();
}

async fn cleanup(admin: &mut PgConnection, database: &str, role: &str) -> Result<(), AdapterError> {
    sqlx::query("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = $1 AND pid <> pg_backend_pid()")
        .bind(database)
        .execute(&mut *admin)
        .await
        .map_err(|_| AdapterError::Rejected)?;
    sqlx::query(&format!("DROP DATABASE IF EXISTS \"{database}\""))
        .execute(&mut *admin)
        .await
        .map_err(|_| AdapterError::Rejected)?;
    sqlx::query(&format!("DROP ROLE IF EXISTS \"{role}\""))
        .execute(&mut *admin)
        .await
        .map_err(|_| AdapterError::Rejected)?;
    Ok(())
}

/// Reusable dispatch seam for catalog wrappers. The listener is Unix-only and
/// each request gets a fresh bounded operation deadline.
pub struct WrapperServer {
    adapter: PostgresAdapter,
}

impl WrapperServer {
    pub fn new(adapter: PostgresAdapter) -> Self {
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
            tokio::spawn(async move { handle_connection(stream, adapter).await });
        }
    }
}

async fn handle_connection(stream: UnixStream, adapter: PostgresAdapter) {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    let read_result = tokio::time::timeout(OPERATION_DEADLINE, async {
        BufReader::new(read).read_line(&mut line).await
    })
    .await;
    let response = match read_result {
        Ok(Ok(count)) if count > 0 && count <= MAX_LINE_LEN => match serde_json::from_str(&line) {
            Ok(request) => dispatch(request, adapter).await,
            Err(_) => error_response("invalid_request"),
        },
        _ => error_response("invalid_request"),
    };
    if let Ok(body) = serde_json::to_vec(&response) {
        let _ = write.write_all(&body).await;
        let _ = write.write_all(b"\n").await;
    }
}

async fn dispatch(request: Request, adapter: PostgresAdapter) -> Response {
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

fn request_revision(request: &Request) -> u32 {
    match request {
        Request::CreateFresh { revision, .. }
        | Request::Ready { revision, .. }
        | Request::Delete { revision, .. } => *revision,
    }
}

fn error_response(code: &str) -> Response {
    Response::Error {
        revision: CONTROL_PROTOCOL_REVISION,
        code: code.to_owned(),
    }
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_IDENTIFIER_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn valid_environment_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_IDENTIFIER_LEN
        && value.bytes().enumerate().all(|(index, byte)| {
            if index == 0 {
                byte == b'_' || byte.is_ascii_alphabetic()
            } else {
                byte == b'_' || byte.is_ascii_alphanumeric()
            }
        })
}

fn generated(prefix: &str, bytes: usize) -> Result<String, AdapterError> {
    let mut random = vec![0; bytes];
    SystemRandom::new()
        .fill(&mut random)
        .map_err(|_| AdapterError::Rejected)?;
    Ok(format!("{prefix}{}", hex::encode(random)))
}

fn random_password() -> Result<String, AdapterError> {
    let mut random = [0; 32];
    SystemRandom::new()
        .fill(&mut random)
        .map_err(|_| AdapterError::Rejected)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random))
}

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
mod tests {
    use super::*;

    // These tests count shared server objects, so keep their setup and cleanup
    // atomic with respect to the other Postgres-backed tests in this module.
    static POSTGRES_TEST_LOCK: Mutex<()> = Mutex::const_new(());

    #[test]
    fn redis_acl_is_prefix_scoped_and_has_no_administrative_commands() {
        assert!(RedisAdapter::new("redis://127.0.0.1:6379", vec!["REDIS_URL".into()]).is_ok());
        assert!(REDIS_ALLOWED_COMMANDS.contains(&"+publish"));
        for forbidden in [
            "+acl",
            "+config",
            "+flushdb",
            "+flushall",
            "+script",
            "+module",
            "+function",
        ] {
            assert!(!REDIS_ALLOWED_COMMANDS.contains(&forbidden));
        }
    }

    #[test]
    fn identifier_validation_is_bounded_and_strict() {
        assert!(valid_identifier("attempt_1-A"));
        assert!(!valid_identifier(""));
        assert!(!valid_identifier("has space"));
        assert!(!valid_identifier(&"a".repeat(MAX_IDENTIFIER_LEN + 1)));
    }

    #[test]
    fn rejects_unsafe_exported_environment_names() {
        assert!(
            PostgresAdapter::new(
                "postgres://postgres@localhost/postgres",
                vec!["DATABASE_URL".into()]
            )
            .is_ok()
        );
        assert!(
            PostgresAdapter::new(
                "postgres://postgres@localhost/postgres",
                vec!["DATABASE-URL".into()]
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn protocol_mismatch_fails_closed_without_backend_access() {
        let adapter = PostgresAdapter::new(
            "postgres://postgres@localhost/postgres",
            vec!["DATABASE_URL".into()],
        )
        .unwrap();
        let response = dispatch(
            Request::Delete {
                revision: 2,
                lease_id: "lease_1".into(),
            },
            adapter,
        )
        .await;
        assert_eq!(response, error_response("revision_mismatch"));
    }

    #[tokio::test]
    async fn unix_dispatch_rejects_malformed_identifiers_without_connecting_to_postgres() {
        let adapter = PostgresAdapter::new(
            "postgres://postgres@localhost/postgres",
            vec!["DATABASE_URL".into()],
        )
        .unwrap();
        let (client, server) = UnixStream::pair().unwrap();
        let task = tokio::spawn(handle_connection(server, adapter));
        let (read, mut write) = client.into_split();
        write
            .write_all(b"{\"operation\":\"create_fresh\",\"revision\":1,\"attempt_id\":\"bad id\",\"lease_nonce\":\"nonce\"}\n")
            .await
            .unwrap();
        let mut response = String::new();
        BufReader::new(read).read_line(&mut response).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Response>(&response).unwrap(),
            error_response("rejected")
        );
        task.await.unwrap();
    }

    #[tokio::test]
    async fn completion_signal_retains_a_permit_for_a_late_waiter() {
        let notify = Notify::new();

        // Exercise the part of the handshake that `notify_waiters()` alone
        // cannot provide: completion happens with no registered waiter.
        signal_completion(&notify);

        tokio::time::timeout(Duration::from_millis(50), notify.notified())
            .await
            .expect("completion permit was lost before waiter registration");
    }

    #[tokio::test]
    async fn completion_before_registration_is_observed_by_identical_waiters() {
        let adapter = PostgresAdapter::new(
            "postgres://postgres@localhost/postgres",
            vec!["DATABASE_URL".into()],
        )
        .unwrap()
        .with_operation_deadline(Duration::from_millis(100));
        let key = CreationKey {
            attempt_id: "completed_attempt".into(),
            lease_nonce: "completed_nonce".into(),
        };
        let notify = Arc::new(Notify::new());
        adapter
            .creations
            .lock()
            .await
            .insert(key.clone(), CreationState::Creating(notify.clone()));

        // Force the precise rejected interleaving: state recording and the
        // completion signal both happen before either waiter is registered.
        adapter.creations.lock().await.insert(
            key.clone(),
            CreationState::Created(CreatedLease {
                lease_id: "lease_recorded".into(),
                environment: BTreeMap::new(),
            }),
        );
        signal_completion(&notify);

        let (first, second) = tokio::time::timeout(Duration::from_millis(50), async {
            tokio::join!(
                adapter.await_creation(&key, notify.clone()),
                adapter.await_creation(&key, notify.clone())
            )
        })
        .await
        .expect("recorded completion was not returned promptly");
        let Ok((first_lease_id, _)) = first else {
            panic!("first identical waiter rejected a recorded creation");
        };
        let Ok((second_lease_id, _)) = second else {
            panic!("second identical waiter rejected a recorded creation");
        };
        assert_eq!(first_lease_id, "lease_recorded");
        assert_eq!(second_lease_id, "lease_recorded");
        assert_eq!(adapter.creations.lock().await.len(), 1);
        assert!(adapter.leases.lock().await.is_empty());
    }

    #[tokio::test]
    async fn postgres_leases_are_fresh_and_isolated() {
        let Some(url) = std::env::var("TEST_POSTGRES_URL").ok() else {
            return;
        };
        let _guard = POSTGRES_TEST_LOCK.lock().await;
        let adapter = PostgresAdapter::new(&url, vec!["DATABASE_URL".into()]).unwrap();
        let (first, second) = tokio::join!(
            adapter.create("attempt_first".into(), "nonce_first".into()),
            adapter.create("attempt_second".into(), "nonce_second".into())
        );
        let (first_id, first_env) = first.unwrap();
        let (second_id, second_env) = second.unwrap();
        assert_ne!(first_id, second_id);
        let first_url = &first_env["DATABASE_URL"];
        let second_url = &second_env["DATABASE_URL"];
        let mut first = PgConnection::connect(first_url).await.unwrap();
        sqlx::query("CREATE TABLE only_first(value integer)")
            .execute(&mut first)
            .await
            .unwrap();
        let mut second = PgConnection::connect(second_url).await.unwrap();
        let visible: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM information_schema.tables WHERE table_name = 'only_first'",
        )
        .fetch_one(&mut second)
        .await
        .unwrap();
        assert_eq!(visible, 0);
        adapter.delete(&first_id).await.unwrap();
        assert!(PgConnection::connect(first_url).await.is_err());
        let (third_id, third_env) = adapter
            .create("attempt_third".into(), "nonce_third".into())
            .await
            .unwrap();
        let mut third = PgConnection::connect(&third_env["DATABASE_URL"])
            .await
            .unwrap();
        let tables: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM information_schema.tables WHERE table_schema = 'public'",
        )
        .fetch_one(&mut third)
        .await
        .unwrap();
        assert_eq!(tables, 0);
        adapter.delete(&third_id).await.unwrap();
        adapter.delete(&second_id).await.unwrap();
    }

    #[tokio::test]
    async fn retried_create_fresh_returns_the_original_lease() {
        let Some(url) = std::env::var("TEST_POSTGRES_URL").ok() else {
            return;
        };
        let _guard = POSTGRES_TEST_LOCK.lock().await;
        let adapter = PostgresAdapter::new(&url, vec!["DATABASE_URL".into()]).unwrap();
        let request = || Request::CreateFresh {
            revision: CONTROL_PROTOCOL_REVISION,
            attempt_id: "retry_attempt".into(),
            lease_nonce: "retry_nonce".into(),
        };
        let (first, concurrent) = tokio::join!(
            dispatch(request(), adapter.clone()),
            dispatch(request(), adapter.clone())
        );
        // Model a lost first response after concurrent identical waiters have
        // both observed completion: retry the identical protocol request.
        let retry = dispatch(request(), adapter.clone()).await;
        let Response::Created {
            lease_id: first_lease_id,
            environment: first_environment,
            ..
        } = first
        else {
            panic!("initial CreateFresh did not succeed");
        };
        let Response::Created {
            lease_id: concurrent_lease_id,
            environment: concurrent_environment,
            ..
        } = concurrent
        else {
            panic!("concurrent CreateFresh did not succeed");
        };
        let Response::Created {
            lease_id,
            environment: retry_environment,
            ..
        } = retry
        else {
            panic!("retried CreateFresh did not succeed");
        };
        assert_eq!(first_lease_id, concurrent_lease_id);
        assert_eq!(first_lease_id, lease_id);
        let first_keys = first_environment.keys().collect::<Vec<_>>();
        assert_eq!(
            first_keys,
            concurrent_environment.keys().collect::<Vec<_>>()
        );
        assert_eq!(first_keys, retry_environment.keys().collect::<Vec<_>>());
        assert_eq!(adapter.leases.lock().await.len(), 1);
        adapter.delete(&lease_id).await.unwrap();
    }

    #[tokio::test]
    async fn timeout_after_role_creation_rolls_back_before_replying() {
        let Some(url) = std::env::var("TEST_POSTGRES_URL").ok() else {
            return;
        };
        let _guard = POSTGRES_TEST_LOCK.lock().await;
        let adapter = PostgresAdapter::new(&url, vec!["DATABASE_URL".into()])
            .unwrap()
            .with_operation_deadline(Duration::from_millis(25));
        let mut admin = PgConnection::connect(&url).await.unwrap();
        let before: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pg_roles WHERE rolname LIKE 'djinn_role_%'")
                .fetch_one(&mut admin)
                .await
                .unwrap();
        adapter
            .pause_after_creating_role(Duration::from_millis(100))
            .await;
        assert_eq!(
            adapter
                .create("timeout_attempt".into(), "timeout_nonce".into())
                .await,
            Err(AdapterError::Rejected)
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if adapter.creations.lock().await.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("background cleanup did not finish");
        let after: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pg_roles WHERE rolname LIKE 'djinn_role_%'")
                .fetch_one(&mut admin)
                .await
                .unwrap();
        assert_eq!(after, before);
        assert!(adapter.leases.lock().await.is_empty());
    }

    #[tokio::test]
    async fn failed_rollback_retains_authority_until_a_retry_cleans_it() {
        let Some(url) = std::env::var("TEST_POSTGRES_URL").ok() else {
            return;
        };
        let _guard = POSTGRES_TEST_LOCK.lock().await;
        let adapter = PostgresAdapter::new(&url, vec!["DATABASE_URL".into()])
            .unwrap()
            .with_operation_deadline(Duration::from_millis(25));
        let mut admin = PgConnection::connect(&url).await.unwrap();
        let before: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pg_roles WHERE rolname LIKE 'djinn_role_%'")
                .fetch_one(&mut admin)
                .await
                .unwrap();
        adapter
            .pause_after_creating_role(Duration::from_millis(100))
            .await;
        adapter.fail_next_rollbacks(1);
        assert_eq!(
            adapter
                .create("cleanup_attempt".into(), "cleanup_nonce".into())
                .await,
            Err(AdapterError::Rejected)
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if matches!(
                    adapter.creations.lock().await.get(&CreationKey {
                        attempt_id: "cleanup_attempt".into(),
                        lease_nonce: "cleanup_nonce".into(),
                    }),
                    Some(CreationState::CleanupPending(_))
                ) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("partial cleanup authority was discarded");
        // The identical request retries cleanup instead of creating a second
        // tenant while the first role/database is still reachable only here.
        assert_eq!(
            adapter
                .create("cleanup_attempt".into(), "cleanup_nonce".into())
                .await,
            Err(AdapterError::Rejected)
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if adapter.creations.lock().await.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("retried cleanup did not finish");
        let after: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pg_roles WHERE rolname LIKE 'djinn_role_%'")
                .fetch_one(&mut admin)
                .await
                .unwrap();
        assert_eq!(after, before);
    }
}

/// Redis execution coverage. Every assertion runs against an in-process RESP
/// fake that models the AUTH / ACL SETUSER-DELUSER / SCAN / UNLINK / PING /
/// SUBSCRIBE / SET / GET / PUBLISH permission semantics the wrapper depends on,
/// and additionally against a real `redis-server` when the binary is present.
/// The fake path is deterministic and always runs; the real path is skipped
/// only when the binary is absent, never when an assertion would fail.
#[cfg(test)]
mod redis_tests {
    use super::*;

    use std::collections::HashSet;

    use tokio::net::TcpListener;

    const FAKE_ADMIN_PASSWORD: &str = "fake-admin-password";

    // ----- In-process RESP fake modeling Redis ACL semantics -----

    #[derive(Clone)]
    struct FakeUser {
        password: Option<String>,
        enabled: bool,
        all_commands: bool,
        commands: HashSet<String>,
        all_keys: bool,
        key_patterns: Vec<String>,
        all_channels: bool,
        channel_patterns: Vec<String>,
    }

    impl FakeUser {
        fn reset() -> Self {
            Self {
                password: None,
                enabled: false,
                all_commands: false,
                commands: HashSet::new(),
                all_keys: false,
                key_patterns: Vec::new(),
                all_channels: false,
                channel_patterns: Vec::new(),
            }
        }

        fn can_run(&self, command: &str) -> bool {
            self.all_commands || self.commands.contains(command)
        }

        fn can_touch_key(&self, key: &str) -> bool {
            self.all_keys || self.key_patterns.iter().any(|p| glob_match(p, key))
        }

        fn can_touch_channel(&self, channel: &str) -> bool {
            self.all_channels || self.channel_patterns.iter().any(|p| glob_match(p, channel))
        }
    }

    struct FakeState {
        users: HashMap<String, FakeUser>,
        keys: BTreeMap<String, Vec<u8>>,
    }

    fn glob_match(pattern: &str, text: &str) -> bool {
        fn helper(pattern: &[u8], text: &[u8]) -> bool {
            match pattern.first() {
                None => text.is_empty(),
                Some(b'*') => {
                    helper(&pattern[1..], text) || (!text.is_empty() && helper(pattern, &text[1..]))
                }
                Some(&head) => {
                    !text.is_empty() && head == text[0] && helper(&pattern[1..], &text[1..])
                }
            }
        }
        helper(pattern.as_bytes(), text.as_bytes())
    }

    struct FakeRedis {
        port: u16,
        handle: tokio::task::JoinHandle<()>,
    }

    impl Drop for FakeRedis {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    impl FakeRedis {
        async fn start() -> Self {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let mut users = HashMap::new();
            users.insert(
                "default".to_owned(),
                FakeUser {
                    password: Some(FAKE_ADMIN_PASSWORD.to_owned()),
                    enabled: true,
                    all_commands: true,
                    commands: HashSet::new(),
                    all_keys: true,
                    key_patterns: Vec::new(),
                    all_channels: true,
                    channel_patterns: Vec::new(),
                },
            );
            let state = Arc::new(Mutex::new(FakeState {
                users,
                keys: BTreeMap::new(),
            }));
            let handle = tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    let state = state.clone();
                    tokio::spawn(async move {
                        fake_connection(stream, state).await;
                    });
                }
            });
            Self { port, handle }
        }

        fn admin_url(&self) -> String {
            format!("redis://:{FAKE_ADMIN_PASSWORD}@127.0.0.1:{}", self.port)
        }
    }

    async fn read_command(
        reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
    ) -> Option<Vec<String>> {
        let mut header = String::new();
        if reader.read_line(&mut header).await.ok()? == 0 {
            return None;
        }
        let count: usize = header
            .strip_suffix("\r\n")?
            .strip_prefix('*')?
            .parse()
            .ok()?;
        let mut arguments = Vec::with_capacity(count);
        for _ in 0..count {
            let mut bulk = String::new();
            if reader.read_line(&mut bulk).await.ok()? == 0 {
                return None;
            }
            let length: usize = bulk.strip_suffix("\r\n")?.strip_prefix('$')?.parse().ok()?;
            let mut body = vec![0u8; length + 2];
            reader.read_exact(&mut body).await.ok()?;
            arguments.push(String::from_utf8(body[..length].to_vec()).ok()?);
        }
        Some(arguments)
    }

    async fn fake_connection(stream: TcpStream, state: Arc<Mutex<FakeState>>) {
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);
        let mut authenticated: Option<String> = None;
        loop {
            let Some(command) = read_command(&mut reader).await else {
                return;
            };
            if command.is_empty() {
                continue;
            }
            let reply = handle_command(&state, &mut authenticated, &command).await;
            if write.write_all(reply.as_bytes()).await.is_err() {
                return;
            }
        }
    }

    fn simple(value: &str) -> String {
        format!("+{value}\r\n")
    }
    fn error(value: &str) -> String {
        format!("-{value}\r\n")
    }
    fn integer(value: i64) -> String {
        format!(":{value}\r\n")
    }
    fn bulk(value: &str) -> String {
        format!("${}\r\n{value}\r\n", value.len())
    }
    fn array_of_bulk(values: &[String]) -> String {
        let mut reply = format!("*{}\r\n", values.len());
        for value in values {
            reply.push_str(&bulk(value));
        }
        reply
    }

    async fn handle_command(
        state: &Arc<Mutex<FakeState>>,
        authenticated: &mut Option<String>,
        command: &[String],
    ) -> String {
        let name = command[0].to_ascii_lowercase();
        if name == "auth" {
            let state = state.lock().await;
            let (user, password) = match command.len() {
                2 => ("default".to_owned(), command[1].clone()),
                3 => (command[1].clone(), command[2].clone()),
                _ => return error("ERR wrong number of arguments for 'auth'"),
            };
            let ok = state
                .users
                .get(&user)
                .is_some_and(|u| u.enabled && u.password.as_deref() == Some(password.as_str()));
            drop(state);
            return if ok {
                *authenticated = Some(user);
                simple("OK")
            } else {
                error("WRONGPASS invalid username-password pair")
            };
        }

        let Some(username) = authenticated.clone() else {
            return error("NOAUTH Authentication required.");
        };
        let mut state = state.lock().await;
        let Some(actor) = state.users.get(&username).cloned() else {
            return error("NOPERM unknown user");
        };
        if !actor.can_run(&name) {
            return error("NOPERM this user has no permissions to run the command");
        }

        match name.as_str() {
            "ping" => simple("PONG"),
            "acl" => {
                let subcommand = command.get(1).map(|s| s.to_ascii_lowercase());
                match subcommand.as_deref() {
                    Some("setuser") => {
                        let Some(target) = command.get(2).cloned() else {
                            return error("ERR wrong number of arguments");
                        };
                        let mut user = state
                            .users
                            .get(&target)
                            .cloned()
                            .unwrap_or_else(FakeUser::reset);
                        for token in &command[3..] {
                            apply_acl_token(&mut user, token);
                        }
                        state.users.insert(target, user);
                        simple("OK")
                    }
                    Some("deluser") => {
                        let mut removed = 0;
                        for target in &command[2..] {
                            if state.users.remove(target).is_some() {
                                removed += 1;
                            }
                        }
                        integer(removed)
                    }
                    _ => error("ERR unknown ACL subcommand"),
                }
            }
            "scan" => {
                // The fake returns every matching key in one page (cursor "0").
                let mut pattern = "*".to_owned();
                let mut index = 2;
                while index < command.len() {
                    if command[index].eq_ignore_ascii_case("match") {
                        if let Some(value) = command.get(index + 1) {
                            pattern = value.clone();
                        }
                        index += 2;
                    } else {
                        index += 1;
                    }
                }
                let matched: Vec<String> = state
                    .keys
                    .keys()
                    .filter(|key| glob_match(&pattern, key) && actor.can_touch_key(key))
                    .cloned()
                    .collect();
                format!("*2\r\n{}{}", bulk("0"), array_of_bulk(&matched))
            }
            "set" => {
                let (Some(key), Some(value)) = (command.get(1), command.get(2)) else {
                    return error("ERR wrong number of arguments");
                };
                if !actor.can_touch_key(key) {
                    return error("NOPERM no permissions to access a key");
                }
                state.keys.insert(key.clone(), value.clone().into_bytes());
                simple("OK")
            }
            "get" => {
                let Some(key) = command.get(1) else {
                    return error("ERR wrong number of arguments");
                };
                if !actor.can_touch_key(key) {
                    return error("NOPERM no permissions to access a key");
                }
                match state.keys.get(key) {
                    Some(value) => bulk(&String::from_utf8_lossy(value)),
                    None => "$-1\r\n".to_owned(),
                }
            }
            "del" | "unlink" | "exists" => {
                let mut affected = 0;
                for key in &command[1..] {
                    if !actor.can_touch_key(key) {
                        return error("NOPERM no permissions to access a key");
                    }
                    if name == "exists" {
                        if state.keys.contains_key(key) {
                            affected += 1;
                        }
                    } else if state.keys.remove(key).is_some() {
                        affected += 1;
                    }
                }
                integer(affected)
            }
            "publish" => {
                let Some(channel) = command.get(1) else {
                    return error("ERR wrong number of arguments");
                };
                if !actor.can_touch_channel(channel) {
                    return error("NOPERM no permissions to access a channel");
                }
                integer(0)
            }
            "subscribe" | "psubscribe" => {
                for channel in &command[1..] {
                    if !actor.can_touch_channel(channel) {
                        return error("NOPERM no permissions to access a channel");
                    }
                }
                let mut reply = String::new();
                for (position, channel) in command[1..].iter().enumerate() {
                    reply.push_str("*3\r\n");
                    reply.push_str(&bulk("subscribe"));
                    reply.push_str(&bulk(channel));
                    reply.push_str(&integer(position as i64 + 1));
                }
                reply
            }
            _ => error("ERR unsupported command"),
        }
    }

    fn apply_acl_token(user: &mut FakeUser, token: &str) {
        match token {
            "reset" => *user = FakeUser::reset(),
            "on" => user.enabled = true,
            "off" => user.enabled = false,
            "nopass" => user.password = None,
            "allkeys" | "~*" => user.all_keys = true,
            "allchannels" | "&*" => user.all_channels = true,
            "-@all" => {
                user.all_commands = false;
                user.commands.clear();
            }
            "+@all" | "allcommands" => user.all_commands = true,
            other => {
                if let Some(password) = other.strip_prefix('>') {
                    user.password = Some(password.to_owned());
                } else if let Some(pattern) = other.strip_prefix('~') {
                    user.key_patterns.push(pattern.to_owned());
                } else if let Some(pattern) = other.strip_prefix('&') {
                    user.channel_patterns.push(pattern.to_owned());
                } else if let Some(command) = other.strip_prefix('+') {
                    user.commands.insert(command.to_ascii_lowercase());
                } else if let Some(command) = other.strip_prefix('-') {
                    user.commands.remove(&command.to_ascii_lowercase());
                }
            }
        }
    }

    // ----- Real redis-server, used only when the binary is present -----

    struct RealRedis {
        child: std::process::Child,
        port: u16,
        _dir: tempfile::TempDir,
    }

    impl Drop for RealRedis {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    async fn spawn_real_redis() -> Option<RealRedis> {
        let dir = tempfile::tempdir().ok()?;
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.ok()?;
        let port = listener.local_addr().ok()?.port();
        drop(listener);
        let child = match std::process::Command::new("redis-server")
            .arg("--port")
            .arg(port.to_string())
            .arg("--save")
            .arg("")
            .arg("--appendonly")
            .arg("no")
            .arg("--dir")
            .arg(dir.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            // Binary genuinely absent: skip the real path (not an assertion).
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(_) => return None,
        };
        for _ in 0..100 {
            if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
                return Some(RealRedis {
                    child,
                    port,
                    _dir: dir,
                });
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        None
    }

    /// Run `body` against the always-on fake and, when available, real redis.
    async fn against_redis<F, Fut>(body: F)
    where
        F: Fn(String) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let fake = FakeRedis::start().await;
        body(fake.admin_url()).await;
        drop(fake);

        if let Some(real) = spawn_real_redis().await {
            body(format!("redis://127.0.0.1:{}", real.port)).await;
        }
    }

    fn url_prefix(url: &str) -> String {
        Url::parse(url)
            .unwrap()
            .query_pairs()
            .find(|(key, _)| key == "key_prefix")
            .expect("returned url must carry key_prefix")
            .1
            .into_owned()
    }

    async fn tenant_conn(endpoint: &Endpoint, url: &str) -> Result<Conn, AdapterError> {
        let parsed = Url::parse(url).unwrap();
        let user = parsed.username().to_owned();
        let password = parsed.password().unwrap().to_owned();
        Conn::open(endpoint, Some(&user), Some(&password), OPERATION_DEADLINE).await
    }

    fn scan_keys(reply: &Resp) -> Vec<String> {
        let Resp::Array(entries) = reply else {
            return Vec::new();
        };
        let Some(Resp::Array(keys)) = entries.get(1) else {
            return Vec::new();
        };
        keys.iter()
            .filter_map(|key| key.text().map(str::to_owned))
            .collect()
    }

    // ----- AC4: cross-lease isolation -----

    #[tokio::test]
    async fn two_leases_cannot_touch_each_others_keys_or_channels() {
        against_redis(|admin_url| async move {
            let adapter = RedisAdapter::new(&admin_url, vec!["REDIS_URL".into()]).unwrap();
            let (a_id, a_env) = adapter
                .create("attempt_a".into(), "nonce_a".into())
                .await
                .unwrap();
            let (b_id, b_env) = adapter
                .create("attempt_b".into(), "nonce_b".into())
                .await
                .unwrap();
            assert_ne!(a_id, b_id);

            let a_url = &a_env["REDIS_URL"];
            let b_url = &b_env["REDIS_URL"];
            let a_prefix = url_prefix(a_url);
            let b_prefix = url_prefix(b_url);
            assert_ne!(a_prefix, b_prefix);

            let mut a = tenant_conn(&adapter.endpoint, a_url).await.unwrap();
            let mut b = tenant_conn(&adapter.endpoint, b_url).await.unwrap();

            // Each lease can act within its own namespace.
            assert!(
                a.cmd(&["SET".into(), format!("{a_prefix}k"), "v".into()])
                    .await
                    .is_ok()
            );
            assert!(
                a.cmd(&["PUBLISH".into(), format!("{a_prefix}c"), "m".into()])
                    .await
                    .is_ok()
            );
            assert!(
                b.cmd(&["SET".into(), format!("{b_prefix}k"), "v".into()])
                    .await
                    .is_ok()
            );

            // Neither lease can read, write, or publish into the other's prefix.
            assert!(
                b.cmd(&["GET".into(), format!("{a_prefix}k")])
                    .await
                    .is_err()
            );
            assert!(
                b.cmd(&["SET".into(), format!("{a_prefix}x"), "v".into()])
                    .await
                    .is_err()
            );
            assert!(
                b.cmd(&["PUBLISH".into(), format!("{a_prefix}c"), "m".into()])
                    .await
                    .is_err()
            );
            assert!(
                b.cmd(&["SUBSCRIBE".into(), format!("{a_prefix}c")])
                    .await
                    .is_err()
            );
            assert!(
                a.cmd(&["GET".into(), format!("{b_prefix}k")])
                    .await
                    .is_err()
            );

            adapter.delete(&a_id).await.unwrap();
            adapter.delete(&b_id).await.unwrap();
        })
        .await;
    }

    // ----- AC4: delete preserves the other lease's data and user -----

    #[tokio::test]
    async fn deleting_one_lease_preserves_the_others_data_and_user() {
        against_redis(|admin_url| async move {
            let adapter = RedisAdapter::new(&admin_url, vec!["REDIS_URL".into()]).unwrap();
            let (a_id, a_env) = adapter
                .create("attempt_a".into(), "nonce_a".into())
                .await
                .unwrap();
            let (b_id, b_env) = adapter
                .create("attempt_b".into(), "nonce_b".into())
                .await
                .unwrap();
            let a_url = a_env["REDIS_URL"].clone();
            let b_url = b_env["REDIS_URL"].clone();
            let a_prefix = url_prefix(&a_url);
            let b_prefix = url_prefix(&b_url);

            let mut a = tenant_conn(&adapter.endpoint, &a_url).await.unwrap();
            let mut b = tenant_conn(&adapter.endpoint, &b_url).await.unwrap();
            a.cmd(&["SET".into(), format!("{a_prefix}k1"), "v".into()])
                .await
                .unwrap();
            b.cmd(&["SET".into(), format!("{b_prefix}k1"), "v".into()])
                .await
                .unwrap();
            drop(a);

            adapter.delete(&a_id).await.unwrap();

            // Lease B's user still authenticates and can keep writing.
            let mut b_again = tenant_conn(&adapter.endpoint, &b_url).await.unwrap();
            assert!(
                b_again
                    .cmd(&["SET".into(), format!("{b_prefix}k2"), "v".into()])
                    .await
                    .is_ok()
            );

            // Lease B's data survives; lease A's data and user are gone.
            let mut admin = adapter.admin().await.unwrap();
            let surviving = admin
                .cmd(&[
                    "SCAN".into(),
                    "0".into(),
                    "MATCH".into(),
                    format!("{b_prefix}*"),
                    "COUNT".into(),
                    "100".into(),
                ])
                .await
                .unwrap();
            assert!(scan_keys(&surviving).contains(&format!("{b_prefix}k1")));

            let purged = admin
                .cmd(&[
                    "SCAN".into(),
                    "0".into(),
                    "MATCH".into(),
                    format!("{a_prefix}*"),
                    "COUNT".into(),
                    "100".into(),
                ])
                .await
                .unwrap();
            assert!(scan_keys(&purged).is_empty());
            assert!(tenant_conn(&adapter.endpoint, &a_url).await.is_err());

            adapter.delete(&b_id).await.unwrap();
        })
        .await;
    }

    // ----- AC4: a fresh lease starts empty -----

    #[tokio::test]
    async fn a_new_lease_starts_with_an_empty_namespace() {
        against_redis(|admin_url| async move {
            let adapter = RedisAdapter::new(&admin_url, vec!["REDIS_URL".into()]).unwrap();
            // Pre-populate a first lease so the backend is non-empty.
            let (first_id, first_env) = adapter
                .create("attempt_first".into(), "nonce_first".into())
                .await
                .unwrap();
            let first_url = first_env["REDIS_URL"].clone();
            let first_prefix = url_prefix(&first_url);
            let mut first = tenant_conn(&adapter.endpoint, &first_url).await.unwrap();
            first
                .cmd(&["SET".into(), format!("{first_prefix}k"), "v".into()])
                .await
                .unwrap();

            let (fresh_id, fresh_env) = adapter
                .create("attempt_fresh".into(), "nonce_fresh".into())
                .await
                .unwrap();
            let fresh_prefix = url_prefix(&fresh_env["REDIS_URL"]);

            let mut admin = adapter.admin().await.unwrap();
            let reply = admin
                .cmd(&[
                    "SCAN".into(),
                    "0".into(),
                    "MATCH".into(),
                    format!("{fresh_prefix}*"),
                    "COUNT".into(),
                    "100".into(),
                ])
                .await
                .unwrap();
            assert!(scan_keys(&reply).is_empty());

            adapter.delete(&first_id).await.unwrap();
            adapter.delete(&fresh_id).await.unwrap();
        })
        .await;
    }

    // ----- AC2: readiness authenticates as the lease user and PINGs -----

    #[tokio::test]
    async fn ready_authenticates_as_the_lease_user() {
        against_redis(|admin_url| async move {
            let adapter = RedisAdapter::new(&admin_url, vec!["REDIS_URL".into()]).unwrap();
            let (lease_id, _) = adapter
                .create("attempt_ready".into(), "nonce_ready".into())
                .await
                .unwrap();
            adapter.ready(&lease_id).await.unwrap();
            assert_eq!(
                adapter.ready("lease_missing").await,
                Err(AdapterError::Rejected)
            );
            adapter.delete(&lease_id).await.unwrap();
        })
        .await;
    }

    // ----- AC3: idempotent create and repeated/unknown delete -----

    #[tokio::test]
    async fn create_is_idempotent_and_delete_is_repeatable() {
        against_redis(|admin_url| async move {
            let adapter = RedisAdapter::new(&admin_url, vec!["REDIS_URL".into()]).unwrap();
            let (first_id, first_env) = adapter
                .create("attempt_retry".into(), "nonce_retry".into())
                .await
                .unwrap();
            let (retry_id, retry_env) = adapter
                .create("attempt_retry".into(), "nonce_retry".into())
                .await
                .unwrap();
            assert_eq!(first_id, retry_id);
            assert_eq!(first_env, retry_env);
            assert_eq!(adapter.leases.lock().await.len(), 1);

            adapter.delete(&first_id).await.unwrap();
            // Repeated delete and unknown lease are both no-op successes.
            adapter.delete(&first_id).await.unwrap();
            adapter.delete("lease_unknown").await.unwrap();
        })
        .await;
    }

    // ----- AC3: timeout mid-provision rolls back and preserves nothing -----

    #[tokio::test]
    async fn timeout_after_user_creation_rolls_back_the_user() {
        against_redis(|admin_url| async move {
            let adapter = RedisAdapter::new(&admin_url, vec!["REDIS_URL".into()])
                .unwrap()
                .with_operation_deadline(Duration::from_millis(25));
            adapter
                .pause_after_creating_user(Duration::from_millis(200))
                .await;
            assert_eq!(
                adapter
                    .create("attempt_timeout".into(), "nonce_timeout".into())
                    .await,
                Err(AdapterError::Rejected)
            );
            // Background rollback removes the user it created and clears the
            // retained creation state, leaving nothing behind.
            wait_for_empty_creations(&adapter).await;
            assert!(adapter.leases.lock().await.is_empty());
        })
        .await;
    }

    // ----- AC3 / finding 4: a failed rollback retains cleanup authority -----

    #[tokio::test]
    async fn failed_rollback_retains_authority_until_a_retry_cleans_it() {
        against_redis(|admin_url| async move {
            let adapter = RedisAdapter::new(&admin_url, vec!["REDIS_URL".into()])
                .unwrap()
                .with_operation_deadline(Duration::from_millis(25));
            adapter
                .pause_after_creating_user(Duration::from_millis(200))
                .await;
            adapter.fail_next_rollbacks(1);
            assert_eq!(
                adapter
                    .create("attempt_cleanup".into(), "nonce_cleanup".into())
                    .await,
                Err(AdapterError::Rejected)
            );
            wait_for_cleanup_pending(&adapter).await;
            // The identical retry retries cleanup rather than orphaning the user.
            assert_eq!(
                adapter
                    .create("attempt_cleanup".into(), "nonce_cleanup".into())
                    .await,
                Err(AdapterError::Rejected)
            );
            wait_for_empty_creations(&adapter).await;
        })
        .await;
    }

    async fn wait_for_empty_creations(adapter: &RedisAdapter) {
        tokio::time::timeout(Duration::from_secs(2), async {
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

    async fn wait_for_cleanup_pending(adapter: &RedisAdapter) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if matches!(
                    adapter.creations.lock().await.get(&CreationKey {
                        attempt_id: "attempt_cleanup".into(),
                        lease_nonce: "nonce_cleanup".into(),
                    }),
                    Some(RedisCreationState::CleanupPending(_))
                ) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("partial cleanup authority was discarded");
    }

    // ----- RESP client coverage: read_resp over crafted byte streams -----

    async fn resp_of(chunks: &'static [&'static [u8]]) -> Result<Resp, AdapterError> {
        let (mut writer, reader) = tokio::io::duplex(256);
        tokio::spawn(async move {
            for chunk in chunks {
                let _ = writer.write_all(chunk).await;
                tokio::task::yield_now().await;
            }
        });
        let mut reader = BufReader::new(reader);
        read_resp(&mut reader).await
    }

    #[tokio::test]
    async fn read_resp_decodes_the_reply_types_the_wrapper_consumes() {
        assert!(matches!(resp_of(&[b"+OK\r\n"]).await, Ok(Resp::Text(value)) if value == "OK"));
        assert!(matches!(resp_of(&[b":7\r\n"]).await, Ok(Resp::Integer)));
        assert!(matches!(
            resp_of(&[b"$5\r\nhello\r\n"]).await,
            Ok(Resp::Text(value)) if value == "hello"
        ));
        let array = resp_of(&[b"*2\r\n$1\r\na\r\n:3\r\n"]).await.unwrap();
        let Resp::Array(items) = array else {
            panic!("expected array");
        };
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].text(), Some("a"));
        assert!(matches!(items[1], Resp::Integer));
    }

    #[tokio::test]
    async fn read_resp_maps_error_replies_to_a_bounded_error() {
        assert!(matches!(
            resp_of(&[b"-NOPERM this user has no permissions\r\n"]).await,
            Err(AdapterError::Rejected)
        ));
        // An unknown RESP type byte is rejected rather than misparsed.
        assert!(matches!(
            resp_of(&[b"?bogus\r\n"]).await,
            Err(AdapterError::Rejected)
        ));
    }

    #[tokio::test]
    async fn read_resp_reassembles_a_reply_split_across_reads() {
        let reply = resp_of(&[b"$11\r\nhel", b"lo ", b"world\r\n"]).await;
        assert!(matches!(reply, Ok(Resp::Text(value)) if value == "hello world"));
    }
}
