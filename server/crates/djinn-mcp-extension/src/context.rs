//! Extension context capability trait.
//!
//! This module defines the [`ExtensionContext`] trait that provides a narrow
//! capability interface for extension handlers, replacing direct dependency on
//! the concrete `AgentContext` god struct.  Handlers dispatched through
//! `djinn-mcp-extension` will be generic over this trait rather than importing
//! `djinn-agent` internals.
//!
//! # Design principles
//!
//! * **Narrow surface** — only capabilities actually required by extension
//!   handlers are exposed.  Each method returns an existing, well-typed
//!   capability handle rather than the whole application context.
//! * **Object-safe** — all methods return owned or `Clone`-able types so the
//!   trait is object-safe (`dyn ExtensionContext` is usable).
//! * **Async-friendly** — uses `async_trait` so future handler methods that
//!   need async context access can be added without breaking the boundary.
//! * **No `djinn-agent` dependency** — this crate depends only on
//!   `djinn-control-plane`, `djinn-db`, and leaf capability crates.

use std::path::{Path, PathBuf};

use djinn_control_plane::McpState;
use djinn_db::Database;

/// Narrow capability seam for extension tool handlers.
///
/// Concrete implementations live in the hosting crate (e.g. `djinn-agent`
/// implements this for `AgentContext`).  The extension crate never names the
/// concrete context type — it operates entirely through this trait.
///
/// # Exposed capabilities
///
/// | Method | Returns | Used by |
/// |--------|---------|---------|
/// | [`db`](Self::db) | `Database` | repository construction (task, epic, project, session, agent, proposal repos) |
/// | [`event_bus`](Self::event_bus) | `EventBus` | repository construction + event emission |
/// | [`mcp_state`](Self::mcp_state) | `McpState` | `DjinnMcpServer` dispatch for shared memory/task/epic ops, `code_graph` via `repo_graph()` |
/// | [`lsp`](Self::lsp) | `LspManager` | LSP tool dispatch (hover, definition, references, symbols, diagnostics) |
/// | [`working_root_for`](Self::working_root_for) | `PathBuf` | code-reading tools (`read`, `shell`, `lsp`, `code_graph`) |
/// | [`default_project_id`](Self::default_project_id) | `Option<&str>` | project resolution fallback for K8s worker pods |
#[async_trait::async_trait]
pub trait ExtensionContext: Send + Sync {
    /// The application database handle.
    ///
    /// Used by handler code to construct typed repository instances
    /// (`TaskRepository`, `EpicRepository`, `ProjectRepository`, etc.).
    fn db(&self) -> Database;

    /// The application event bus.
    ///
    /// Passed alongside [`db`](Self::db) to repository constructors so that
    /// domain events are emitted on mutations.
    fn event_bus(&self) -> djinn_core::events::EventBus;

    /// Build the MCP-layer state bridge.
    ///
    /// Returns a fully-wired [`McpState`] suitable for constructing a
    /// `DjinnMcpServer` to dispatch shared control-plane operations (memory
    /// tools, task mutation ops, epic ops, `code_graph` via `repo_graph()`).
    ///
    /// Implementations should prefer caching the result when construction is
    /// expensive; the trait returns an owned value so callers do not need to
    /// hold a borrow on the context.
    fn mcp_state(&self) -> McpState;

    /// The language-server manager.
    ///
    /// Used directly by the `lsp` tool handler (hover, definition,
    /// references, symbols) and by `write`/`edit`/`apply_patch` handlers
    /// that run diagnostics after file modifications.
    fn lsp(&self) -> djinn_lsp::LspManager;

    /// Resolve the working root for code-reading tools.
    ///
    /// When the context has an explicit working root override (e.g. architect
    /// reading from the canonical index checkout), that path takes precedence.
    /// Otherwise `fallback` (typically the per-task worktree path) is returned
    /// as-is.
    fn working_root_for(&self, fallback: &Path) -> PathBuf;

    /// The default project identifier, if set.
    ///
    /// K8s worker pods set this from `TaskRunSpec.project_id` so single-project
    /// pods do not require every MCP tool call to carry an explicit `project`
    /// argument.  Returns `None` on host-side contexts where the caller is
    /// expected to disambiguate.
    fn default_project_id(&self) -> Option<&str>;
}

#[cfg(test)]
mod boundary_tests {
    //! Compile-time and runtime boundary checks ensuring `djinn-mcp-extension`
    //! does not depend on `djinn-agent` internals.

    /// Verify that the crate's own `Cargo.toml` does not list `djinn-agent`
    /// as a dependency.  This is a runtime surrogate for a cargo-metadata
    /// graph check — the real enforcement is the dependency graph itself, but
    /// this test catches accidental additions during development.
    #[test]
    fn no_djinn_agent_dependency() {
        let cargo_toml = include_str!("../Cargo.toml");
        assert!(
            !cargo_toml.contains("djinn-agent"),
            "djinn-mcp-extension must not depend on djinn-agent — the \
             ExtensionContext trait is the sole boundary seam"
        );
    }

    /// Verify that the crate's own `Cargo.toml` does not list `sqlx`
    /// as a direct dependency.  Raw-SQL access must go through `djinn-db`
    /// helpers.
    #[test]
    fn no_direct_sqlx_dependency() {
        let cargo_toml = include_str!("../Cargo.toml");
        assert!(
            !cargo_toml.contains("sqlx"),
            "djinn-mcp-extension must not depend on sqlx directly — \
             use djinn-db repository helpers instead"
        );
    }

    /// Verify that `djinn-supervisor` (the actors crate) is not listed
    /// as a dependency.  Supervisor/actor integration goes through the
    /// `djinn-agent` façade.
    #[test]
    fn no_actors_dependency() {
        let cargo_toml = include_str!("../Cargo.toml");
        assert!(
            !cargo_toml.contains("djinn-supervisor"),
            "djinn-mcp-extension must not depend on djinn-supervisor (actors) — \
             use ExtensionContext trait methods instead"
        );
    }

    /// Scan all non-test `.rs` source files for `use djinn_agent` imports.
    /// This catches accidental references to the façade crate in production
    /// code (test modules are allowed to be absent from the scan since they
    /// do not ship).
    #[test]
    fn no_source_imports_from_djinn_agent() {
        let src_root = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
        let forbidden = scan_source_imports(src_root, &["djinn_agent"]);
        assert!(
            forbidden.is_empty(),
            "djinn-mcp-extension source files must not import from djinn_agent:\n{}",
            forbidden.join("\n")
        );
    }

    /// Scan all non-test `.rs` source files for direct `use sqlx` imports.
    #[test]
    fn no_source_imports_from_sqlx() {
        let src_root = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
        let forbidden = scan_source_imports(src_root, &["sqlx"]);
        assert!(
            forbidden.is_empty(),
            "djinn-mcp-extension source files must not import from sqlx:\n{}",
            forbidden.join("\n")
        );
    }

    /// Scan all non-test `.rs` source files for imports from `djinn_supervisor`
    /// (the actors crate).
    #[test]
    fn no_source_imports_from_actors() {
        let src_root = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
        let forbidden = scan_source_imports(src_root, &["djinn_supervisor"]);
        assert!(
            forbidden.is_empty(),
            "djinn-mcp-extension source files must not import from djinn_supervisor:\n{}",
            forbidden.join("\n")
        );
    }

    /// Scan all non-test `.rs` source files for imports from `crate::context`
    /// patterns that reference `AgentContext` or `context.rs` internals of
    /// `djinn-agent`.
    ///
    /// `djinn-mcp-extension` must reference context capabilities only through
    /// its own `ExtensionContext` trait.
    #[test]
    fn no_source_imports_from_djinn_agent_context() {
        let src_root = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
        let forbidden = scan_source_imports(src_root, &["djinn_agent::context"]);
        assert!(
            forbidden.is_empty(),
            "djinn-mcp-extension source files must not import from djinn_agent::context:\n{}",
            forbidden.join("\n")
        );
    }

    /// Walk `src_root` looking for `use <forbidden_crate>` statements in
    /// non-test `.rs` files.  Returns a list of `path:line: content` for
    /// each hit, excluding test modules (`#[cfg(test)]` blocks).
    fn scan_source_imports(src_root: &str, forbidden_crates: &[&str]) -> Vec<String> {
        let mut hits = Vec::new();
        let Ok(entries) = std::fs::read_dir(src_root) else {
            return hits;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && path.file_name().is_some_and(|n| n != "tests") {
                hits.extend(scan_source_imports(
                    &path.to_string_lossy(),
                    forbidden_crates,
                ));
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                // Skip test files themselves
                let fname = path.file_name().unwrap().to_string_lossy();
                if fname == "tests.rs" || path.to_string_lossy().contains("/tests/") {
                    continue;
                }
                let Ok(content) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let mut in_test_cfg = false;
                let mut brace_depth: u32 = 0;
                for (line_no, line) in content.lines().enumerate() {
                    let trimmed = line.trim();
                    // Track `#[cfg(test)]` blocks to skip imports inside them
                    if trimmed.contains("#[cfg(test)]") {
                        in_test_cfg = true;
                        brace_depth = 0;
                        continue;
                    }
                    if in_test_cfg {
                        brace_depth += trimmed.chars().filter(|&c| c == '{').count() as u32;
                        brace_depth = brace_depth
                            .saturating_sub(trimmed.chars().filter(|&c| c == '}').count() as u32);
                        if brace_depth == 0 && trimmed.contains('}') {
                            in_test_cfg = false;
                        }
                        continue;
                    }
                    if trimmed.starts_with("//") || trimmed.starts_with("//!") {
                        continue;
                    }
                    for &krate in forbidden_crates {
                        let pattern = format!("use {krate}");
                        if trimmed.contains(&pattern)
                            || trimmed.contains(&format!("use crate::{krate}"))
                        {
                            hits.push(format!("{}:{}: {}", path.display(), line_no + 1, trimmed));
                        }
                    }
                }
            }
        }
        hits
    }
}
