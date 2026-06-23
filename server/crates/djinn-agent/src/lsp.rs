// LSP facade — re-exports from djinn-lsp.
//
// The implementation has moved to the `djinn-lsp` crate. This module
// preserves existing `djinn_agent::lsp::...` import paths for server
// and crate consumers.

// Re-export the entire djinn_lsp public API so that existing
// `djinn_agent::lsp::LspManager`, `djinn_agent::lsp::format_diagnostics_xml`,
// `djinn_agent::lsp::SymbolQuery`, etc. paths continue to resolve.
pub use djinn_lsp::*;
