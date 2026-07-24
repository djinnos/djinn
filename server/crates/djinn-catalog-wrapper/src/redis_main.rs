use djinn_catalog_wrapper::{RedisAdapter, RedisWrapperServer};
use std::path::PathBuf;
#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
    let _socket = std::env::var_os("CATALOG_CONTROL_SOCKET")
        .map(PathBuf::from)
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing control socket")
        })?;
    let _adapter = RedisAdapter::from_environment().map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid wrapper configuration",
        )
    })?;
    RedisWrapperServer::new(_adapter).serve(_socket).await
}
