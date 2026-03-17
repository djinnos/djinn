---
title: djinn-mcp Extraction WIP
type: research
tags: []
---

# djinn-mcp Extraction — Session State (2026-03-17)

## Files Written This Session

All files below are complete and on disk in `crates/djinn-mcp/src/`:

- `process.rs` — verbatim copy ✓
- `lib.rs` — pub mod declarations ✓
- `server.rs` — `crate::mcp::state::McpState` → `crate::state::McpState` ✓
- `dispatch.rs` — all `crate::mcp::*` → `crate::*`, test block stripped ✓
- `tools/mod.rs` — verbatim copy ✓
- `tools/memory_tools/mod.rs` — imports transformed ✓
- `tools/memory_tools/types.rs` — `crate::models::*` → `djinn_core::models::*` ✓
- `tools/memory_tools/reads.rs` — verbatim copy ✓
- `tools/memory_tools/writes.rs` — inline `use crate::db::is_singleton` → `use djinn_db::is_singleton` ✓
- `tools/memory_tools/search.rs` — inline `use crate::models::NoteCompact` → `use djinn_core::models::NoteCompact` ✓
- `tools/task_tools/board.rs` — verbatim copy ✓

## Still TODO (next session)

1. `crates/djinn-mcp/src/tools/task_tools/mod.rs` — transforms:
   - `crate::db::EpicRepository` → `djinn_db::EpicRepository`
   - `crate::db::ProjectRepository` → `djinn_db::ProjectRepository`
   - `crate::db::SessionRepository` → `djinn_db::SessionRepository`
   - `crate::db::{ActivityQuery, CountQuery, ListQuery, ReadyQuery, TaskRepository}` → `djinn_db::{...}`
   - `crate::mcp::server::DjinnMcpServer` → `crate::server::DjinnMcpServer`
   - `crate::mcp::tools::AnyJson` → `crate::tools::AnyJson`
   - `crate::mcp::tools::validation::*` → `crate::tools::validation::*`
   - `crate::models::SessionStatus` → `djinn_core::models::SessionStatus`
   - `crate::models::{Task, TaskStatus, TransitionAction}` → `djinn_core::models::{Task, TaskStatus, TransitionAction}`
   - inline `crate::models::parse_json_array` → `djinn_core::models::parse_json_array`
   - Strip `#[cfg(test)] mod tests;`

2. `crates/djinn-mcp/src/tools/task_tools/types.rs` — verbatim copy from server's `src/mcp/tools/task_tools/types.rs` (uses `super::*`, all needed types flow through)

3. `crates/djinn-mcp/src/tools/session_tools/mod.rs` — transforms:
   - `crate::db::{ActivityQuery, SessionMessageRepository, SessionRepository, TaskRepository}` → `djinn_db::{...}`
   - `crate::mcp::server::DjinnMcpServer` → `crate::server::DjinnMcpServer`
   - `crate::models::SessionRecord` → `djinn_core::models::SessionRecord`
   - inline `crate::agent::message::Role` → `djinn_provider::message::Role` (line ~389 in session_messages fn)
   - Strip `#[cfg(test)] mod tests;`

4. `crates/djinn-mcp/src/tools/provider_tools.rs` — new flat file, transforms:
   - `crate::db::CredentialRepository` → `djinn_provider::repos::CredentialRepository`
   - `crate::db::CustomProviderRepository` → `djinn_provider::repos::CustomProviderRepository`
   - `crate::mcp::server::DjinnMcpServer` → `crate::server::DjinnMcpServer`
   - `crate::models::{CustomProvider, Model, Provider, SeedModel}` → `djinn_core::models::{CustomProvider, Model, Provider, SeedModel}`
   - `crate::provider::builtin` → `djinn_provider::catalog::builtin`
   - `crate::provider::health::ModelHealth` → `djinn_provider::catalog::health::ModelHealth`
   - `crate::provider::validate::{self, ValidationRequest}` → `djinn_provider::catalog::validate::{self, ValidationRequest}`
   - inline `crate::agent::oauth::{OAuthFlowKind, codex, copilot}` → `djinn_provider::oauth::{OAuthFlowKind, codex, copilot}`
   - inline `crate::models::Pricing` → `djinn_core::models::Pricing`
   - Strip `#[cfg(test)] mod contract_tests;` and `#[cfg(test)] mod tests { ... }`

5. `crates/djinn-mcp/src/tools/project_tools.rs` — flat file, transforms:
   - `crate::db::ProjectRepository` → `djinn_db::ProjectRepository`
   - `crate::mcp::server::DjinnMcpServer` → `crate::server::DjinnMcpServer`
   - `crate::process::output` stays (local module)
   - Strip `#[cfg(test)] #[path = "project_tools/tests.rs"] mod tests;`

6. Add `djinn-mcp` to workspace `Cargo.toml` members list

7. Implement bridge traits in server (wire `AppState` → `McpState` via bridge impls)

## Import Transform Reference

| Server `crate::` path | djinn-mcp path |
|---|---|
| `crate::db::NoteRepository` | `djinn_db::NoteRepository` |
| `crate::db::ProjectRepository` | `djinn_db::ProjectRepository` |
| `crate::db::EpicRepository` | `djinn_db::EpicRepository` |
| `crate::db::TaskRepository` | `djinn_db::TaskRepository` |
| `crate::db::SessionRepository` | `djinn_db::SessionRepository` |
| `crate::db::SessionMessageRepository` | `djinn_db::SessionMessageRepository` |
| `crate::db::CredentialRepository` | `djinn_provider::repos::CredentialRepository` |
| `crate::db::CustomProviderRepository` | `djinn_provider::repos::CustomProviderRepository` |
| `crate::db::{ActivityQuery,CountQuery,ListQuery,ReadyQuery}` | `djinn_db::{...}` |
| `crate::db::is_singleton` | `djinn_db::is_singleton` |
| `crate::models::*` (Task, Epic, Note, etc.) | `djinn_core::models::*` |
| `crate::models::{CustomProvider,Model,Provider,SeedModel,Pricing}` | `djinn_core::models::*` |
| `crate::mcp::server::DjinnMcpServer` | `crate::server::DjinnMcpServer` |
| `crate::mcp::state::McpState` | `crate::state::McpState` |
| `crate::mcp::tools::*` | `crate::tools::*` |
| `crate::provider::builtin` | `djinn_provider::catalog::builtin` |
| `crate::provider::health::ModelHealth` | `djinn_provider::catalog::health::ModelHealth` |
| `crate::provider::validate` | `djinn_provider::catalog::validate` |
| `crate::agent::message::Role` | `djinn_provider::message::Role` |
| `crate::agent::oauth::*` | `djinn_provider::oauth::*` |
| `crate::process::output` | `crate::process::output` (local) |
