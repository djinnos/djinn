//! Thin facade re-exporting the public LSP API from the `djinn-lsp` crate.
//!
/// Implementation lives in `server/crates/djinn-lsp`; this module preserves
/// the historical `djinn_agent::lsp::*` import paths used by server and
/// internal consumers.
pub use djinn_lsp::{
    Diagnostic, LspManager, LspWarning, SymbolQuery, format_diagnostics_xml,
    parse_symbol_kind_filter,
};
