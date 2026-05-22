use std::net::{TcpStream, ToSocketAddrs};
use std::str::FromStr;
use std::time::Duration;

use djinn_db::DatabaseConnectConfig;

#[cfg(test)]
use djinn_db::MysqlDatabaseConfig;

const DEFAULT_MYSQL_HOST: &str = "127.0.0.1";
const DEFAULT_MYSQL_PORT: u16 = 3306;
const DEFAULT_HEALTHCHECK_TIMEOUT: Duration = Duration::from_millis(250);

/// Outcome of probing the mysql endpoint.
///
/// Under compose / Helm-managed deploy MySQL is a sibling container or
/// StatefulSet — the server does not spawn it itself. We only return
/// `AlreadyHealthy` when the TCP probe succeeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MysqlSqlServerAvailability {
    AlreadyHealthy { endpoint: String },
}

#[derive(Debug, thiserror::Error)]
pub enum MysqlRuntimeError {
    #[error("invalid mysql sql-server configuration: {0}")]
    InvalidConfig(String),
    #[error("failed to parse mysql endpoint for runtime probe: {0}")]
    InvalidMysqlUrl(String),
    #[error(
        "mysql service unreachable at {endpoint}; check that the mysql container is running (docker compose up -d mysql-test) or that the StatefulSet is Ready"
    )]
    Unreachable { endpoint: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MysqlEndpoint {
    host: String,
    port: u16,
}

fn mysql_endpoint_from_url(url: &str) -> Result<MysqlEndpoint, MysqlRuntimeError> {
    let trimmed = url.trim();
    let without_scheme = trimmed
        .strip_prefix("mysql://")
        .ok_or_else(|| MysqlRuntimeError::InvalidMysqlUrl(trimmed.to_owned()))?;
    let host_and_path = without_scheme.rsplit('@').next().unwrap_or(without_scheme);
    let host_port = host_and_path
        .split(['/', '?'])
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| MysqlRuntimeError::InvalidMysqlUrl(trimmed.to_owned()))?;

    let socket_addr = if host_port.starts_with('[') {
        let end = host_port
            .find(']')
            .ok_or_else(|| MysqlRuntimeError::InvalidMysqlUrl(trimmed.to_owned()))?;
        let host = &host_port[1..end];
        let port = host_port[end + 1..]
            .strip_prefix(':')
            .filter(|value| !value.is_empty())
            .map(parse_port)
            .transpose()?
            .unwrap_or(DEFAULT_MYSQL_PORT);
        MysqlEndpoint {
            host: host.to_owned(),
            port,
        }
    } else {
        let mut segments = host_port.splitn(2, ':');
        let host = segments.next().unwrap_or(DEFAULT_MYSQL_HOST).trim();
        let port = segments
            .next()
            .map(parse_port)
            .transpose()?
            .unwrap_or(DEFAULT_MYSQL_PORT);
        MysqlEndpoint {
            host: if host.is_empty() {
                DEFAULT_MYSQL_HOST.to_owned()
            } else {
                host.to_owned()
            },
            port,
        }
    };

    Ok(socket_addr)
}

fn parse_port(value: &str) -> Result<u16, MysqlRuntimeError> {
    u16::from_str(value)
        .map_err(|_| MysqlRuntimeError::InvalidMysqlUrl(format!("invalid port `{value}`")))
}

/// Probe the mysql service over TCP.
///
/// Under compose / Helm-managed deploy MySQL is a sibling container or
/// StatefulSet; the server does not spawn or supervise it. If the probe
/// fails, we return an error that tells the operator to check the runtime.
pub fn ensure_mysql_runtime_for_connect_config(
    connect: &DatabaseConnectConfig,
) -> Result<Option<MysqlSqlServerAvailability>, MysqlRuntimeError> {
    let DatabaseConnectConfig::Mysql(mysql) = connect;

    let endpoint = mysql_endpoint_from_url(&mysql.url)?;
    let endpoint_label = format!("{}:{}", endpoint.host, endpoint.port);
    if probe_tcp_endpoint(&endpoint.host, endpoint.port, DEFAULT_HEALTHCHECK_TIMEOUT) {
        return Ok(Some(MysqlSqlServerAvailability::AlreadyHealthy {
            endpoint: endpoint_label,
        }));
    }

    Err(MysqlRuntimeError::Unreachable {
        endpoint: endpoint_label,
    })
}

fn probe_tcp_endpoint(host: &str, port: u16, timeout: Duration) -> bool {
    let addr = format!("{host}:{port}");
    addr.to_socket_addrs()
        .ok()
        .and_then(|mut addrs| addrs.next())
        .map(|socket_addr| TcpStream::connect_timeout(&socket_addr, timeout).is_ok())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mysql_endpoint_parser_supports_defaults() {
        let endpoint = mysql_endpoint_from_url("mysql://root@127.0.0.1/djinn").unwrap();
        assert_eq!(endpoint.host, "127.0.0.1");
        assert_eq!(endpoint.port, 3306);
    }

    #[test]
    fn mysql_endpoint_parser_accepts_service_host() {
        let endpoint = mysql_endpoint_from_url("mysql://root@djinn-mysql:3306/djinn").unwrap();
        assert_eq!(endpoint.host, "djinn-mysql");
        assert_eq!(endpoint.port, 3306);
    }

    #[test]
    fn unreachable_endpoint_surfaces_compose_hint() {
        use std::net::{SocketAddr, TcpListener};
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let result = ensure_mysql_runtime_for_connect_config(&DatabaseConnectConfig::Mysql(
            MysqlDatabaseConfig {
                url: format!("mysql://root@127.0.0.1:{port}/djinn"),
            },
        ))
        .expect_err("unreachable mysql should fail");

        assert!(matches!(result, MysqlRuntimeError::Unreachable { .. }));
        assert!(result.to_string().contains("docker compose"));
    }
}
