use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use djinn_db::{DatabaseConnectConfig, MysqlBackendFlavor, MysqlDatabaseConfig};

const DEFAULT_DOLT_HOST: &str = "127.0.0.1";
const DEFAULT_DOLT_PORT: u16 = 3306;
const DEFAULT_DOLT_USER: &str = "root";
const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_HEALTHCHECK_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DoltSqlServerConfig {
    pub executable: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub repo_path: PathBuf,
    pub data_dir: Option<PathBuf>,
    pub startup_timeout: Duration,
    pub healthcheck_timeout: Duration,
}

impl DoltSqlServerConfig {
    pub fn from_env(mysql: &MysqlDatabaseConfig) -> Result<Option<Self>, DoltRuntimeError> {
        if mysql.flavor != MysqlBackendFlavor::Dolt {
            return Ok(None);
        }

        let endpoint = mysql_endpoint_from_url(&mysql.url)?;
        let repo_path = match std::env::var("DJINN_DOLT_SQL_SERVER_REPO") {
            Ok(value) if !value.trim().is_empty() => PathBuf::from(value),
            Ok(_) | Err(std::env::VarError::NotPresent) => return Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => {
                return Err(DoltRuntimeError::InvalidConfig(
                    "DJINN_DOLT_SQL_SERVER_REPO must be valid unicode".to_owned(),
                ));
            }
        };

        let executable = std::env::var("DJINN_DOLT_SQL_SERVER_BIN")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "dolt".to_owned());
        let user = std::env::var("DJINN_DOLT_SQL_SERVER_USER")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_DOLT_USER.to_owned());
        let data_dir = std::env::var("DJINN_DOLT_SQL_SERVER_DATA_DIR")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let startup_timeout = duration_from_env_secs(
            "DJINN_DOLT_SQL_SERVER_STARTUP_TIMEOUT_SECS",
            DEFAULT_STARTUP_TIMEOUT,
        )?;
        let healthcheck_timeout = duration_from_env_millis(
            "DJINN_DOLT_SQL_SERVER_HEALTHCHECK_TIMEOUT_MS",
            DEFAULT_HEALTHCHECK_TIMEOUT,
        )?;

        Ok(Some(Self {
            executable,
            host: endpoint.host,
            port: endpoint.port,
            user,
            repo_path,
            data_dir,
            startup_timeout,
            healthcheck_timeout,
        }))
    }

    pub fn endpoint(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

#[derive(Clone)]
pub struct DoltSqlServerManager {
    config: DoltSqlServerConfig,
    process: Arc<Mutex<Option<ManagedDoltSqlServer>>>,
}

impl DoltSqlServerManager {
    pub fn from_connect_config(
        connect: &DatabaseConnectConfig,
    ) -> Result<Option<Self>, DoltRuntimeError> {
        let DatabaseConnectConfig::Mysql(mysql) = connect else {
            return Ok(None);
        };
        let config = match DoltSqlServerConfig::from_env(mysql)? {
            Some(config) => config,
            None => return Ok(None),
        };
        Ok(Some(Self::new(config)))
    }

    pub fn new(config: DoltSqlServerConfig) -> Self {
        Self {
            config,
            process: Arc::new(Mutex::new(None)),
        }
    }

    pub fn config(&self) -> &DoltSqlServerConfig {
        &self.config
    }

    pub fn ensure_available(&self) -> Result<DoltSqlServerAvailability, DoltRuntimeError> {
        if probe_tcp_endpoint(
            &self.config.host,
            self.config.port,
            self.config.healthcheck_timeout,
        ) {
            return Ok(DoltSqlServerAvailability::AlreadyHealthy {
                endpoint: self.config.endpoint(),
            });
        }

        let mut guard = self.process.lock().expect("poisoned dolt process state");
        if let Some(managed) = guard.as_mut()
            && let Some(status) = managed
                .child
                .try_wait()
                .map_err(DoltRuntimeError::ProcessIo)?
        {
            return Err(DoltRuntimeError::ManagedProcessExited {
                endpoint: self.config.endpoint(),
                status: format_status(status),
            });
        }

        if guard.is_none() {
            let mut command = Command::new(&self.config.executable);
            command
                .arg("sql-server")
                .arg("--host")
                .arg(&self.config.host)
                .arg("--port")
                .arg(self.config.port.to_string())
                .arg("--user")
                .arg(&self.config.user)
                .current_dir(&self.config.repo_path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if let Some(data_dir) = &self.config.data_dir {
                command.arg("--data-dir").arg(data_dir);
            }
            let child = command.spawn().map_err(DoltRuntimeError::Spawn)?;
            *guard = Some(ManagedDoltSqlServer { child });
        }
        drop(guard);

        let deadline = Instant::now() + self.config.startup_timeout;
        while Instant::now() < deadline {
            if probe_tcp_endpoint(
                &self.config.host,
                self.config.port,
                self.config.healthcheck_timeout,
            ) {
                return Ok(DoltSqlServerAvailability::Spawned {
                    endpoint: self.config.endpoint(),
                });
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        Err(DoltRuntimeError::StartupTimeout {
            endpoint: self.config.endpoint(),
            timeout: self.config.startup_timeout,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DoltSqlServerAvailability {
    AlreadyHealthy { endpoint: String },
    Spawned { endpoint: String },
}

struct ManagedDoltSqlServer {
    child: Child,
}

impl Drop for ManagedDoltSqlServer {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DoltRuntimeError {
    #[error("invalid dolt sql-server configuration: {0}")]
    InvalidConfig(String),
    #[error("failed to parse mysql endpoint for dolt runtime: {0}")]
    InvalidMysqlUrl(String),
    #[error("failed to spawn dolt sql-server for {endpoint}: {source}")]
    Spawn {
        endpoint: String,
        #[source]
        source: std::io::Error,
    },
    #[error("managed dolt sql-server at {endpoint} exited before becoming healthy ({status})")]
    ManagedProcessExited { endpoint: String, status: String },
    #[error("timed out after {timeout:?} waiting for dolt sql-server at {endpoint}")]
    StartupTimeout {
        endpoint: String,
        timeout: Duration,
    },
    #[error("dolt runtime unavailable at {endpoint}; start `dolt sql-server` manually or set DJINN_DOLT_SQL_SERVER_REPO so djinn can manage it")]
    Unavailable { endpoint: String },
    #[error("dolt sql-server process io error: {0}")]
    ProcessIo(#[source] std::io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MysqlEndpoint {
    host: String,
    port: u16,
}

pub fn mysql_endpoint_from_url(url: &str) -> Result<MysqlEndpoint, DoltRuntimeError> {
    let trimmed = url.trim();
    let without_scheme = trimmed
        .strip_prefix("mysql://")
        .ok_or_else(|| DoltRuntimeError::InvalidMysqlUrl(trimmed.to_owned()))?;
    let host_and_path = without_scheme.rsplit('@').next().unwrap_or(without_scheme);
    let host_port = host_and_path
        .split(['/', '?'])
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| DoltRuntimeError::InvalidMysqlUrl(trimmed.to_owned()))?;

    let socket_addr = if host_port.starts_with('[') {
        let end = host_port
            .find(']')
            .ok_or_else(|| DoltRuntimeError::InvalidMysqlUrl(trimmed.to_owned()))?;
        let host = &host_port[1..end];
        let port = host_port[end + 1..]
            .strip_prefix(':')
            .filter(|value| !value.is_empty())
            .map(parse_port)
            .transpose()?
            .unwrap_or(DEFAULT_DOLT_PORT);
        MysqlEndpoint {
            host: host.to_owned(),
            port,
        }
    } else {
        let mut segments = host_port.splitn(2, ':');
        let host = segments.next().unwrap_or(DEFAULT_DOLT_HOST).trim();
        let port = segments.next().map(parse_port).transpose()?.unwrap_or(DEFAULT_DOLT_PORT);
        MysqlEndpoint {
            host: if host.is_empty() {
                DEFAULT_DOLT_HOST.to_owned()
            } else {
                host.to_owned()
            },
            port,
        }
    };

    Ok(socket_addr)
}

fn parse_port(value: &str) -> Result<u16, DoltRuntimeError> {
    u16::from_str(value)
        .map_err(|_| DoltRuntimeError::InvalidMysqlUrl(format!("invalid port `{value}`")))
}

fn duration_from_env_secs(key: &str, default: Duration) -> Result<Duration, DoltRuntimeError> {
    match std::env::var(key) {
        Ok(value) if !value.trim().is_empty() => value
            .trim()
            .parse::<u64>()
            .map(Duration::from_secs)
            .map_err(|_| DoltRuntimeError::InvalidConfig(format!("{key} must be an integer"))),
        Ok(_) | Err(std::env::VarError::NotPresent) => Ok(default),
        Err(std::env::VarError::NotUnicode(_)) => Err(DoltRuntimeError::InvalidConfig(format!(
            "{key} must be valid unicode"
        ))),
    }
}

fn duration_from_env_millis(key: &str, default: Duration) -> Result<Duration, DoltRuntimeError> {
    match std::env::var(key) {
        Ok(value) if !value.trim().is_empty() => value
            .trim()
            .parse::<u64>()
            .map(Duration::from_millis)
            .map_err(|_| DoltRuntimeError::InvalidConfig(format!("{key} must be an integer"))),
        Ok(_) | Err(std::env::VarError::NotPresent) => Ok(default),
        Err(std::env::VarError::NotUnicode(_)) => Err(DoltRuntimeError::InvalidConfig(format!(
            "{key} must be valid unicode"
        ))),
    }
}

pub fn ensure_dolt_runtime_for_connect_config(
    connect: &DatabaseConnectConfig,
) -> Result<Option<DoltSqlServerAvailability>, DoltRuntimeError> {
    let DatabaseConnectConfig::Mysql(mysql) = connect else {
        return Ok(None);
    };
    if mysql.flavor != MysqlBackendFlavor::Dolt {
        return Ok(None);
    }

    let endpoint = mysql_endpoint_from_url(&mysql.url)?;
    if probe_tcp_endpoint(&endpoint.host, endpoint.port, DEFAULT_HEALTHCHECK_TIMEOUT) {
        return Ok(Some(DoltSqlServerAvailability::AlreadyHealthy {
            endpoint: format!("{}:{}", endpoint.host, endpoint.port),
        }));
    }

    match DoltSqlServerManager::from_connect_config(connect)? {
        Some(manager) => manager.ensure_available().map(Some),
        None => Err(DoltRuntimeError::Unavailable {
            endpoint: format!("{}:{}", endpoint.host, endpoint.port),
        }),
    }
}

fn probe_tcp_endpoint(host: &str, port: u16, timeout: Duration) -> bool {
    let addr = format!("{host}:{port}");
    addr.to_socket_addrs()
        .ok()
        .and_then(|mut addrs| addrs.next())
        .map(|socket_addr| TcpStream::connect_timeout(&socket_addr, timeout).is_ok())
        .unwrap_or(false)
}

fn format_status(status: std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => "terminated by signal".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::net::TcpListener;
    use tempfile::tempdir;

    #[test]
    fn mysql_endpoint_parser_supports_defaults() {
        let endpoint = mysql_endpoint_from_url("mysql://root@127.0.0.1/djinn").unwrap();
        assert_eq!(endpoint.host, "127.0.0.1");
        assert_eq!(endpoint.port, 3306);
    }

    #[test]
    fn unavailable_runtime_surfaces_actionable_error() {
        let port = free_port();
        let result = ensure_dolt_runtime_for_connect_config(&DatabaseConnectConfig::Mysql(
            MysqlDatabaseConfig {
                url: format!("mysql://root@127.0.0.1:{port}/djinn"),
                flavor: MysqlBackendFlavor::Dolt,
            },
        ))
        .expect_err("missing managed config should fail");

        assert!(matches!(result, DoltRuntimeError::Unavailable { .. }));
        assert!(result.to_string().contains("DJINN_DOLT_SQL_SERVER_REPO"));
    }

    #[test]
    fn manager_can_spawn_and_probe_fake_sql_server() {
        let port = free_port();
        let temp = tempdir().unwrap();
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).unwrap();
        let script = temp.path().join("fake-dolt.py");
        fs::write(
            &script,
            format!(
                "#!/usr/bin/env python3\nimport signal, socket, sys\n\nargs = sys.argv[1:]\nif not args or args[0] != 'sql-server':\n    sys.exit(64)\nhost='127.0.0.1'\nport={port}\nfor idx, value in enumerate(args):\n    if value == '--host': host = args[idx+1]\n    if value == '--port': port = int(args[idx+1])\nsock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)\nsock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)\nsock.bind((host, port))\nsock.listen(8)\ndef stop(*_):\n    sock.close()\n    sys.exit(0)\nsignal.signal(signal.SIGTERM, stop)\nsignal.signal(signal.SIGINT, stop)\nwhile True:\n    conn, _ = sock.accept()\n    conn.close()\n"
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&script, perms).unwrap();
        }

        let manager = DoltSqlServerManager::new(DoltSqlServerConfig {
            executable: script.to_string_lossy().to_string(),
            host: "127.0.0.1".to_owned(),
            port,
            user: "root".to_owned(),
            repo_path,
            data_dir: None,
            startup_timeout: Duration::from_secs(5),
            healthcheck_timeout: Duration::from_millis(100),
        });

        let availability = manager.ensure_available().unwrap();
        assert!(matches!(availability, DoltSqlServerAvailability::Spawned { .. }));
        assert!(probe_tcp_endpoint("127.0.0.1", port, Duration::from_millis(100)));
    }

    fn free_port() -> u16 {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    }
}
