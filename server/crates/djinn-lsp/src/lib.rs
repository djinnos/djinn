// LSP module — Language Server Protocol client management.
//
// Extracted from djinn-agent as a focused leaf crate. Provides LSP server
// lifecycle, diagnostics, symbol queries, and code-intelligence requests.

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
pub(crate) const INIT_TIMEOUT: tokio::time::Duration = tokio::time::Duration::from_secs(45);

/// Timeout for regular LSP requests (hover, definition, references, symbols).
pub(crate) const REQUEST_TIMEOUT: tokio::time::Duration = tokio::time::Duration::from_secs(10);

#[cfg(test)]
mod tests;

/// Self-contained tempdir helper for djinn-lsp tests.
///
/// Replaces `djinn_agent::test_helpers::test_tempdir` so the crate
/// stays independent of djinn-agent.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::PathBuf;

    pub fn test_tempdir(prefix: &str) -> tempfile::TempDir {
        let base = test_tmp_base();
        std::fs::create_dir_all(&base).expect("create test tempdir base");
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(base)
            .expect("create test tempdir")
    }

    fn test_tmp_base() -> PathBuf {
        if let Ok(base) = std::env::var("CARGO_TARGET_TMPDIR") {
            let base = PathBuf::from(base).join("djinn-lsp");
            if base.is_relative() {
                std::env::current_dir().expect("current dir").join(base)
            } else {
                base
            }
        } else {
            std::env::current_dir()
                .expect("current dir")
                .join("target")
                .join("test-tmp")
        }
    }
}
