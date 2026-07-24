//! Private protocol-v1 catalog-service wrapper server and Postgres adapter.
//!
//! This crate deliberately exposes only bounded protocol errors. In particular,
//! backend errors, tenant identifiers, and connection credentials never cross
//! the Unix control socket or enter logs.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
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
const REDIS_KEY_PREFIX: &str = "djinn_redis_tenant_";
const MAX_REDIS_LEASES: usize = 1024;
#[derive(Clone)]
pub struct RedisAdapter {
    endpoint: Endpoint,
    names: Vec<String>,
    leases: Arc<Mutex<HashMap<String, RLease>>>,
    created: Arc<Mutex<HashMap<CreationKey, CreatedLease>>>,
}
#[derive(Clone)]
struct Endpoint {
    host: String,
    port: u16,
    user: Option<String>,
    password: Option<String>,
}
#[derive(Clone)]
struct RLease {
    user: String,
    password: String,
    prefix: String,
}
impl Endpoint {
    fn parse(x: &str) -> Result<Self, AdapterError> {
        let u = Url::parse(x).map_err(|_| AdapterError::Rejected)?;
        if u.scheme() != "redis" || !matches!(u.path(), "" | "/") {
            return Err(AdapterError::Rejected);
        }
        Ok(Self {
            host: u.host_str().ok_or(AdapterError::Rejected)?.into(),
            port: u.port().unwrap_or(6379),
            user: (!u.username().is_empty()).then(|| u.username().into()),
            password: u.password().map(str::to_owned),
        })
    }
}
impl RedisAdapter {
    pub fn new(x: &str, mut names: Vec<String>) -> Result<Self, AdapterError> {
        if names.is_empty() || names.iter().any(|x| !valid_environment_name(x)) {
            return Err(AdapterError::Rejected);
        }
        names.sort();
        names.dedup();
        Ok(Self {
            endpoint: Endpoint::parse(x)?,
            names,
            leases: Arc::new(Mutex::new(HashMap::new())),
            created: Arc::new(Mutex::new(HashMap::new())),
        })
    }
    pub fn from_environment() -> Result<Self, AdapterError> {
        let x = std::env::var("REDIS_WRAPPER_ADMIN_URL")
            .or_else(|_| std::env::var("REDIS_URL"))
            .map_err(|_| AdapterError::Rejected)?;
        Self::new(
            &x,
            std::env::var("CATALOG_REDIS_ENV_NAMES")
                .unwrap_or_else(|_| "REDIS_URL".into())
                .split(',')
                .map(str::trim)
                .filter(|x| !x.is_empty())
                .map(str::to_owned)
                .collect(),
        )
    }
    async fn create(
        &self,
        a: String,
        n: String,
    ) -> Result<(String, BTreeMap<String, String>), AdapterError> {
        let k = CreationKey {
            attempt_id: a,
            lease_nonce: n,
        };
        let mut c = self.created.lock().await;
        if c.len() >= MAX_REDIS_LEASES {
            return Err(AdapterError::Rejected);
        }
        if let Some(x) = c.get(&k).cloned() {
            return Ok((x.lease_id, x.environment));
        }
        if self.leases.lock().await.len() >= MAX_REDIS_LEASES {
            return Err(AdapterError::Rejected);
        }
        let id = generated(LEASE_PREFIX, 20)?;
        let l = RLease {
            user: generated(REDIS_USER_PREFIX, 20)?,
            password: random_password()?,
            prefix: generated(REDIS_KEY_PREFIX, 20)?,
        };
        if self.provision(&l).await.is_err() {
            let _ = self.remove(&l.user).await;
            return Err(AdapterError::Rejected);
        }
        let host = if self.endpoint.host.contains(':') {
            format!("[{}]", self.endpoint.host)
        } else {
            self.endpoint.host.clone()
        };
        let url = format!(
            "redis://{}:{}@{}:{}",
            l.user, l.password, host, self.endpoint.port
        );
        let environment: BTreeMap<String, String> = self
            .names
            .iter()
            .cloned()
            .map(|x| (x, url.clone()))
            .collect();
        self.leases.lock().await.insert(id.clone(), l);
        c.insert(
            k,
            CreatedLease {
                lease_id: id.clone(),
                environment: environment.clone(),
            },
        );
        Ok((id, environment))
    }
    async fn provision(&self, l: &RLease) -> Result<(), AdapterError> {
        let mut c = self.admin().await?;
        let p = format!("{}:*", l.prefix);
        let mut x = vec![
            "ACL".into(),
            "SETUSER".into(),
            l.user.clone(),
            "reset".into(),
            "on".into(),
            format!(">{}", l.password),
            format!("~{p}"),
            format!("&{p}"),
            "-@all".into(),
        ];
        x.extend(ACL.iter().map(|x| (*x).into()));
        c.cmd(&x).await.map(|_| ())
    }
    async fn ready(&self, id: &str) -> Result<(), AdapterError> {
        let l = self
            .leases
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or(AdapterError::Rejected)?;
        Conn::open(&self.endpoint, Some(&l.user), Some(&l.password))
            .await?
            .cmd(&["PING".into()])
            .await
            .map(|_| ())
    }
    async fn delete(&self, id: &str) -> Result<(), AdapterError> {
        let Some(l) = self.leases.lock().await.get(id).cloned() else {
            return Ok(());
        };
        let mut c = self.admin().await?;
        let mut cursor = "0".into();
        loop {
            let r = c
                .cmd(&[
                    "SCAN".into(),
                    cursor,
                    "MATCH".into(),
                    format!("{}:*", l.prefix),
                    "COUNT".into(),
                    "100".into(),
                ])
                .await?;
            let Resp::Array(v) = r else {
                return Err(AdapterError::Rejected);
            };
            let [next, Resp::Array(keys)] = v.as_slice() else {
                return Err(AdapterError::Rejected);
            };
            cursor = next.text().ok_or(AdapterError::Rejected)?.into();
            let mut u = vec!["UNLINK".into()];
            for key in keys {
                u.push(key.text().ok_or(AdapterError::Rejected)?.into())
            }
            if u.len() > 1 {
                c.cmd(&u).await?;
            }
            if cursor == "0" {
                break;
            }
        }
        self.remove(&l.user).await?;
        self.leases.lock().await.remove(id);
        self.created
            .lock()
            .await
            .retain(|_, created| created.lease_id != id);
        Ok(())
    }
    async fn admin(&self) -> Result<Conn, AdapterError> {
        Conn::open(
            &self.endpoint,
            self.endpoint.user.as_deref(),
            self.endpoint.password.as_deref(),
        )
        .await
    }
    async fn remove(&self, u: &str) -> Result<(), AdapterError> {
        self.admin()
            .await?
            .cmd(&["ACL".into(), "DELUSER".into(), u.into()])
            .await
            .map(|_| ())
    }
}
const ACL: &[&str] = &[
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
struct Conn {
    r: BufReader<OwnedReadHalf>,
    w: OwnedWriteHalf,
}
enum Resp {
    Text(String),
    Integer,
    Array(Vec<Resp>),
}
impl Resp {
    fn text(&self) -> Option<&str> {
        match self {
            Self::Text(x) => Some(x),
            _ => None,
        }
    }
}
impl Conn {
    async fn open(e: &Endpoint, u: Option<&str>, p: Option<&str>) -> Result<Self, AdapterError> {
        let s = TcpStream::connect((e.host.as_str(), e.port))
            .await
            .map_err(|_| AdapterError::Rejected)?;
        let (r, w) = s.into_split();
        let mut c = Self {
            r: BufReader::new(r),
            w,
        };
        if let Some(p) = p {
            c.cmd(&match u {
                Some(u) => vec!["AUTH".into(), u.into(), p.into()],
                None => vec!["AUTH".into(), p.into()],
            })
            .await?;
        }
        Ok(c)
    }
    async fn cmd(&mut self, x: &[String]) -> Result<Resp, AdapterError> {
        let mut b = format!("*{}\r\n", x.len()).into_bytes();
        for a in x {
            b.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
            b.extend_from_slice(a.as_bytes());
            b.extend_from_slice(b"\r\n")
        }
        self.w
            .write_all(&b)
            .await
            .map_err(|_| AdapterError::Rejected)?;
        read_resp(&mut self.r).await
    }
}
fn read_resp<'a>(
    r: &'a mut BufReader<OwnedReadHalf>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Resp, AdapterError>> + Send + 'a>> {
    Box::pin(async move {
        let mut k = [0];
        r.read_exact(&mut k)
            .await
            .map_err(|_| AdapterError::Rejected)?;
        let mut x = String::new();
        r.read_line(&mut x)
            .await
            .map_err(|_| AdapterError::Rejected)?;
        let x = x.strip_suffix("\r\n").ok_or(AdapterError::Rejected)?;
        match k[0] {
            b'+' => Ok(Resp::Text(x.into())),
            b':' => Ok(Resp::Integer),
            b'-' => Err(AdapterError::Rejected),
            b'$' => {
                let n: usize = x.parse().map_err(|_| AdapterError::Rejected)?;
                let mut b = vec![0; n + 2];
                r.read_exact(&mut b)
                    .await
                    .map_err(|_| AdapterError::Rejected)?;
                Ok(Resp::Text(
                    std::str::from_utf8(&b[..n])
                        .map_err(|_| AdapterError::Rejected)?
                        .into(),
                ))
            }
            b'*' => {
                let n: usize = x.parse().map_err(|_| AdapterError::Rejected)?;
                let mut v = vec![];
                for _ in 0..n {
                    v.push(read_resp(r).await?)
                }
                Ok(Resp::Array(v))
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
    let result = match request {
        Request::CreateFresh {
            attempt_id,
            lease_nonce,
            ..
        } if valid_identifier(&attempt_id) && valid_identifier(&lease_nonce) => adapter
            .create(attempt_id, lease_nonce)
            .await
            .map(|(lease_id, environment)| Response::Created {
                revision: CONTROL_PROTOCOL_REVISION,
                lease_id,
                environment,
            }),
        Request::Ready { lease_id, .. } if valid_identifier(&lease_id) => {
            adapter.ready(&lease_id).await.map(|()| Response::Ready {
                revision: CONTROL_PROTOCOL_REVISION,
            })
        }
        Request::Delete { lease_id, .. } if valid_identifier(&lease_id) => {
            adapter.delete(&lease_id).await.map(|()| Response::Deleted {
                revision: CONTROL_PROTOCOL_REVISION,
            })
        }
        _ => Err(AdapterError::Rejected),
    };
    result.unwrap_or_else(|_| error_response("rejected"))
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
        assert!(ACL.contains(&"+publish"));
        for forbidden in [
            "+acl",
            "+config",
            "+flushdb",
            "+flushall",
            "+script",
            "+module",
            "+function",
        ] {
            assert!(!ACL.contains(&forbidden));
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
