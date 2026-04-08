pub use diagnostics::{Diagnostic, format_diagnostics_xml};
pub use manager::{LspManager, LspWarning};
pub use symbols::{SymbolQuery, parse_symbol_kind_filter};

mod client;
mod diagnostics;
mod manager;
mod requests;
mod server_config;
mod symbols;
mod workspace;

/// Timeout for LSP `initialize` — rust-analyzer can take 30-45s on first run
/// while it builds its index. Matches opencode's 45s timeout.
pub(super) const INIT_TIMEOUT: tokio::time::Duration = tokio::time::Duration::from_secs(45);

/// Timeout for regular LSP requests (hover, definition, references, symbols).
pub(super) const REQUEST_TIMEOUT: tokio::time::Duration = tokio::time::Duration::from_secs(10);

#[cfg(test)]
mod tests;
