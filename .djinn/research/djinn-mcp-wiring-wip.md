# djinn-mcp Server Wiring — WIP

## Status
Crate scaffold complete (ca251b6). Server wiring in progress — NOT committed.
Working changes are unstaged in the working tree.

## What's done (uncommitted)
- `Cargo.toml`: added `djinn-mcp = { path = "crates/djinn-mcp" }`
- `src/mcp_bridge.rs`: bridge trait impls for all 6 traits + `AppState::mcp_state()`
- `src/server/state/mod.rs`: removed old `mcp_state()`, added `coordinator_sync()` + `pool_sync()`
- `src/lib.rs`: replaced `pub mod mcp;` with `pub mod mcp_bridge;` + `mod mcp_contract_tests;`
- `src/server/mod.rs`: `use djinn_mcp` instead of `use crate::mcp`
- `src/server/chat.rs`: `use djinn_mcp::server::DjinnMcpServer`
- `src/mcp_contract_tests.rs`: stub (incomplete)

## Blocking Issue: Orphan Rule
`src/mcp_bridge.rs` implements `djinn_mcp::bridge::CoordinatorOps` for
`djinn_agent::actors::coordinator::CoordinatorHandle` — but both types are
from external crates, violating the orphan rule.

## Fix Required
Use **newtype wrappers** in the server:

```rust
// src/mcp_bridge.rs
pub struct CoordinatorBridge(djinn_agent::actors::coordinator::CoordinatorHandle);
pub struct SlotPoolBridge(djinn_agent::actors::slot::SlotPoolHandle);
pub struct LspBridge(djinn_agent::lsp::LspManager);
pub struct SyncBridge(crate::sync::SyncManager);

#[async_trait]
impl djinn_mcp::bridge::CoordinatorOps for CoordinatorBridge { ... }
// etc.
```

Then in `AppState::mcp_state()`:
```rust
coordinator.map(|c| Arc::new(CoordinatorBridge(c)) as Arc<dyn CoordinatorOps>)
```

`AppState` (server-owned type) can implement `RuntimeOps` and `GitOps` directly — those are fine.

## Other errors to fix after orphan fix
- `async_trait` not in scope in mcp_bridge.rs → add `use async_trait::async_trait;` ✓ (already there)
- `McpState` not public → check djinn-mcp lib.rs exports
- `src/mcp/` still exists — delete after build passes
- `src/mcp_contract_tests.rs` is incomplete — needs all tests from `src/mcp/tools/*/tests.rs`
  with `crate::mcp::*` → `djinn_mcp::*` imports

## Remaining Steps (ordered)
1. Fix orphan rule: wrap CoordinatorHandle, SlotPoolHandle, LspManager, SyncManager in newtypes
2. Fix McpState visibility: check `djinn_mcp::McpState` is `pub use`d in lib.rs
3. Run `cargo build` until clean
4. Move tests from `src/mcp/tools/*/tests.rs` to `src/mcp_contract_tests.rs`
5. Delete `src/mcp/`
6. Run `RUSTFLAGS="-D warnings" cargo test --workspace`
7. Commit and close task rhqv
