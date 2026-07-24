//! Private protocol-v1 catalog-service wrapper server and Postgres adapter.
//!
//! This crate deliberately exposes only bounded protocol errors. In particular,
//! backend errors, tenant identifiers, and connection credentials never cross
//! the Unix control socket or enter logs.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use djinn_sandbox::service_provisioning::{CONTROL_PROTOCOL_REVISION, Request, Response};
use ring::rand::{SecureRandom, SystemRandom};
use sqlx::{Connection, PgConnection};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
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

enum CreationState {
    Creating(Arc<Notify>),
    Created(CreatedLease),
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

    async fn create(
        &self,
        attempt_id: String,
        lease_nonce: String,
    ) -> Result<(String, BTreeMap<String, String>), AdapterError> {
        let key = CreationKey {
            attempt_id,
            lease_nonce,
        };
        loop {
            let notify = {
                let mut creations = self.creations.lock().await;
                match creations.get(&key) {
                    Some(CreationState::Created(created)) => {
                        return Ok((created.lease_id.clone(), created.environment.clone()));
                    }
                    Some(CreationState::Creating(notify)) => notify.clone(),
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
            if tokio::time::timeout(self.operation_deadline, notify.notified())
                .await
                .is_err()
            {
                return Err(AdapterError::Rejected);
            }
        }
    }

    async fn complete_creation(&self, key: CreationKey, notify: Arc<Notify>) {
        let result = self.create_backend().await;
        let mut creations = self.creations.lock().await;
        match result {
            Ok((lease_id, lease, environment)) => {
                self.leases.lock().await.insert(lease_id.clone(), lease);
                creations.insert(
                    key,
                    CreationState::Created(CreatedLease {
                        lease_id,
                        environment,
                    }),
                );
            }
            Err(_) => {
                creations.remove(&key);
            }
        }
        drop(creations);
        notify.notify_waiters();
    }

    async fn create_backend(
        &self,
    ) -> Result<(String, Lease, BTreeMap<String, String>), AdapterError> {
        let lease_id = generated(LEASE_PREFIX, 20)?;
        let role = generated(ROLE_PREFIX, 20)?;
        let database = generated(DATABASE_PREFIX, 20)?;
        let password = random_password()?;
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
        if tokio::time::timeout(self.operation_deadline, provision)
            .await
            .ok()
            .and_then(Result::ok)
            .is_none()
        {
            // A timed-out SQL connection is dropped before cleanup. Use a
            // fresh connection so a stalled database creation cannot strand a
            // previously-created role.
            self.rollback(&database, &role).await;
            return Err(AdapterError::Rejected);
        }

        let lease = Lease {
            role,
            database,
            password,
        };
        let connection_url = self.lease_url(&lease)?;
        let environment = self
            .environment_names
            .iter()
            .cloned()
            .map(|name| (name, connection_url.clone()))
            .collect();
        Ok((lease_id, lease, environment))
    }

    async fn rollback(&self, database: &str, role: &str) {
        if let Ok(mut admin) = self.admin_connection().await {
            let _ = cleanup(&mut admin, database, role).await;
        }
    }

    #[cfg(test)]
    async fn pause_after_creating_role(&self, pause: Duration) {
        *self.pause_after_role.lock().await = Some(pause);
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

    async fn delete(&self, lease_id: &str) -> Result<(), AdapterError> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
    async fn postgres_leases_are_fresh_and_isolated() {
        let Some(url) = std::env::var("TEST_POSTGRES_URL").ok() else {
            return;
        };
        let adapter = PostgresAdapter::new(&url, vec!["DATABASE_URL".into()]).unwrap();
        let (first, second) = tokio::join!(
            adapter.create("attempt_first".into(), "nonce_first".into()),
            adapter.create("attempt_second".into(), "nonce_second".into())
        );
        let (first_id, first_env) = first.unwrap();
        let (second_id, second_env) = second.unwrap();
        let first_url = &first_env["DATABASE_URL"];
        let second_url = &second_env["DATABASE_URL"];
        assert_ne!(first_url, second_url);
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
        let adapter = PostgresAdapter::new(&url, vec!["DATABASE_URL".into()]).unwrap();
        let request = || Request::CreateFresh {
            revision: CONTROL_PROTOCOL_REVISION,
            attempt_id: "retry_attempt".into(),
            lease_nonce: "retry_nonce".into(),
        };
        let first = dispatch(request(), adapter.clone()).await;
        // Model a lost first response: retry the identical protocol request.
        let retry = dispatch(request(), adapter.clone()).await;
        assert_eq!(first, retry);
        let Response::Created { lease_id, .. } = retry else {
            panic!("CreateFresh did not succeed");
        };
        assert_eq!(adapter.leases.lock().await.len(), 1);
        adapter.delete(&lease_id).await.unwrap();
    }

    #[tokio::test]
    async fn timeout_after_role_creation_rolls_back_before_replying() {
        let Some(url) = std::env::var("TEST_POSTGRES_URL").ok() else {
            return;
        };
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
        let after: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pg_roles WHERE rolname LIKE 'djinn_role_%'")
                .fetch_one(&mut admin)
                .await
                .unwrap();
        assert_eq!(after, before);
        assert!(adapter.leases.lock().await.is_empty());
    }
}
