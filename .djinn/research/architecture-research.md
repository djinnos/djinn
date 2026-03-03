---
tags:
    - research
    - architecture
title: Architecture Research
type: research
---
# Architecture Research — Tauri 2.x Desktop with Child Server

## Sidecar Pattern (Server Lifecycle)

**Use `tauri-plugin-shell` + `ShellExt::sidecar()`.** Do not use `std::process::Command` directly.

### Configuration
```json
// src-tauri/tauri.conf.json
{ "bundle": { "externalBin": ["binaries/djinn-server"] } }
```
Binary naming: `djinn-server-{target-triple}` (e.g., `djinn-server-aarch64-apple-darwin`)

### Startup Sequence
1. `tauri::Builder::setup()` — spawn sidecar, start health poll
2. Health check loop: poll `http://127.0.0.1:{port}/health` every 2s
3. Window starts hidden (`visible: false` in config)
4. Show window after health check passes
5. Frontend connects SSE + starts fetching

### Graceful Shutdown
- Handle `RunEvent::ExitRequested` — kill child process
- On macOS: also handle `CloseRequested` (closing last window doesn't quit)
- On Windows: `TerminateProcess` (child.kill()). On Unix: SIGTERM then SIGKILL.

### State Management
```
Mutex<ServerState> { child: Option<CommandChild>, port: u16 }
```

## Communication Architecture

```
[Rust Server] ←HTTP fetch()→ [Webview/React]
     ↓ SSE                        ↑ invoke()
[/events]                    [Tauri Rust Layer]
                              (native ops only)
```

- **fetch()** for all MCP/HTTP calls to localhost server
- **EventSource** for SSE stream from server
- **invoke()** only for: get_server_port, auth token management, native file dialogs
- CSP: `connect-src: ipc: http://ipc.localhost http://127.0.0.1:{port}`

## State Architecture

**Server is source of truth. Frontend mirrors via SSE.**

- Zustand stores: SSE listener updates task/epic state directly from event payload
- TanStack Query: settings, providers, memory (request-driven, cached)
- Tauri Rust state: only process handle + port (minimal)
- No domain state duplication in Rust layer

## Authentication (Clerk JWT)

> **SUPERSEDED by [[ADR-001: Desktop-Only Clerk Authentication via System Browser PKCE]].**
> Server no longer requires Clerk JWT. Desktop owns auth self-contained via system browser OAuth PKCE.
> See [[CLI Auth Implementation Reference]] for the implementation pattern to port.

Original design (no longer applicable):
- ~~System browser + deep link callback pattern~~
- ~~Store in OS keychain via `tauri-plugin-stronghold`~~
- ~~Pass token to server at spawn time~~

## Auto-Update Architecture

- `tauri-plugin-updater` checks `latest.json` on GitHub Releases
- `tauri-action@v0` in CI builds, signs, generates `latest.json`, uploads to release
- Frontend checks on startup, shows update dialog, calls `downloadAndInstall()`
- `tauri-plugin-process` provides `relaunch()` after install

### Required CI Secrets
`TAURI_SIGNING_PRIVATE_KEY`, `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`, `GITHUB_TOKEN`, Apple signing secrets (macOS), Azure signing secrets (Windows)

## Project Structure

```
desktop/
├── package.json
├── vite.config.ts
├── index.html
├── src/                         # Frontend (React)
│   ├── main.tsx
│   ├── App.tsx
│   ├── pages/                   # Kanban, Roadmap, Settings, Onboarding
│   ├── components/              # shadcn/ui components
│   ├── stores/                  # Zustand stores
│   ├── hooks/                   # useServerEvents, custom hooks
│   ├── api/                     # fetch wrappers for MCP server
│   └── tauri/                   # invoke() wrappers + types
├── src-tauri/
│   ├── Cargo.toml
│   ├── tauri.conf.json
│   ├── capabilities/default.json
│   ├── binaries/                # Sidecar binaries (gitignored, CI-built)
│   └── src/
│       ├── main.rs              # Thin shim → lib::run()
│       ├── lib.rs               # Plugin registration, setup
│       ├── server.rs            # Sidecar spawn, health check
│       ├── commands.rs          # #[tauri::command] handlers
│       └── auth.rs              # Deep link, token management
└── .github/workflows/release.yml
```

### Key Rules
- All Rust logic in `lib.rs`, not `main.rs`
- All `#[tauri::command]` in `commands.rs`
- All `invoke()` calls wrapped in `src/tauri/commands.ts` — never direct in components
- Sidecar binaries gitignored, built in CI

## SSE in Tauri Webviews

- Works on all three engines (WebKit, WebView2, WebKitGTK)
- Server must emit `Access-Control-Allow-Origin: *` (Tauri enforces CORS even for localhost)
- Native EventSource auto-reconnects on network drop
- For high-frequency streams: consider Tauri's `Channel` API instead

## Single Instance

`tauri-plugin-single-instance` — focus existing window on duplicate launch attempt.

## Relations
- [[Project Brief]] — project context
- [[Stack Research]] — technology versions and config
- [[Features Research]] — UI patterns driving architecture
- [[Pitfalls Research]] — architecture-level risks