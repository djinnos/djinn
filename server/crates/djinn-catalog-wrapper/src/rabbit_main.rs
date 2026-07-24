use std::path::PathBuf;

use djinn_catalog_wrapper::{RabbitAdapter, RabbitWrapperServer};

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
    let socket = std::env::var_os("CATALOG_CONTROL_SOCKET")
        .map(PathBuf::from)
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing control socket")
        })?;
    let adapter = RabbitAdapter::from_environment().map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid wrapper configuration",
        )
    })?;
    RabbitWrapperServer::new(adapter).serve(socket).await
}
