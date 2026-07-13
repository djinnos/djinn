// Sandbox facade — re-exports from djinn-sandbox.
//
// The implementation has moved to the `djinn-sandbox` crate. This module
// preserves existing `crate::sandbox::...` import paths for djinn-agent
// consumers.

// Re-export the entire djinn_sandbox public API so that existing
// `crate::sandbox::SANDBOX`, `crate::sandbox::Sandbox`, etc. paths
// continue to resolve without changes in call sites.
pub use djinn_sandbox::*;

// Re-export submodules so `crate::sandbox::chat_shell::*`,
// `crate::sandbox::linux::*`, and `crate::sandbox::macos::*` paths
// continue to resolve.
#[cfg(target_os = "linux")]
#[allow(unused_imports)]
pub use djinn_sandbox::chat_shell;
#[cfg(target_os = "linux")]
#[allow(unused_imports)]
pub use djinn_sandbox::linux;
#[cfg(target_os = "macos")]
#[allow(unused_imports)]
pub use djinn_sandbox::macos;
