---
tags:
    - research
    - pitfalls
title: Pitfalls Research
type: research
---
# Pitfalls Research — Electron-to-Tauri Rebuild Risks

## Critical: Startup Crash Blocks Updater

The updater runs inside the app process. If a bug crashes the app at startup, the updater never executes. Users are permanently stuck until manual reinstall. **No built-in recovery mechanism.**

**Mitigation:** Keep `setup()` minimal and panic-free. Move startup logic to async tasks. Consider a separate launcher binary for critical recovery (future consideration).

## Critical: Signing Key Loss is Permanent

Losing the ed25519 private signing key means no future updates can be pushed to installed copies. Users are stranded forever.

**Mitigation:** Store key in a secrets manager (GitHub Encrypted Secrets, AWS Secrets Manager) on day one. Never in `.env` files.

## High: Sidecar Lifecycle Boilerplate (~200 lines)

No official plugin for sidecar lifecycle management. Every team must manually implement:
1. Startup health check polling
2. Port conflict detection (use `TcpListener::bind("127.0.0.1:0")` for random port)
3. Orphan process cleanup from previous crashed sessions
4. Crash detection and restart with backoff
5. Cross-platform signal handling (SIGTERM/SIGKILL on Unix, TerminateProcess on Windows)
6. stdout/stderr pipe management (buffer overflow risk)
7. macOS quarantine attribute removal for bundled binaries

## High: WebKitGTK Linux Limitations

- Ubuntu 22.04 LTS is the minimum (WebKitGTK 4.1 required)
- No WebRTC, no WebGPU on Linux
- CSS animations can cause blurry rendering (unresolved WebKitGTK bug)
- `contenteditable` behaves incorrectly
- Updater only supports AppImage (not deb/rpm)

**Mitigation:** Test on Linux early and often. Treat Safari's CanIUse profile as lowest CSS bar.

## High: Windows Code Signing Changed

- Since June 2023: OV certificates require HSM (no exportable files)
- Since March 2024: EV certificates no longer bypass SmartScreen instantly
- Cloud HSM (Azure Key Vault) required for CI signing
- SmartScreen reputation builds organically over time

**Mitigation:** Budget for Azure Key Vault or equivalent. Accept SmartScreen warnings for initial releases.

## Medium: macOS Notarization Gotchas

- `notarytool` required (`altool` deprecated and broken)
- JIT entitlement `com.apple.security.cs.allow-jit` **mandatory** — WKWebView crashes without it
- Sidecar binaries must be individually codesigned before bundling
- Notarization can hang indefinitely — add timeout + retry in CI

## Medium: IPC Performance on Windows

- ~200ms for 10MB payload vs ~5ms on macOS
- Never send large datasets via single `invoke()` call
- Use `fetch()` to local server instead (HTTP stack is faster for data transfer)

## Medium: Tauri 2.x Breaking Changes from 1.x

- `allowlist` replaced by capabilities/permissions ACL
- `tauri::api::*` modules removed (now separate plugin crates)
- Windows URL scheme changed (`https://tauri.localhost` → `http://tauri.localhost`)
- Event system: `listen_global()` renamed to `listen_any()`
- Auto-update dialog removed — must implement manually
- `system-tray` renamed to `tray-icon`

**Mitigation:** Use Tauri 2.x docs exclusively. Ignore any pre-2024 tutorials.

## Medium: Webview Freezing

Known active bug: webview windows freeze randomly on multiple platforms under memory pressure. No reliable workaround. Particularly pronounced on Windows.

**Mitigation:** Keep DOM lightweight. Use virtualization for lists >100 items. Monitor memory usage.

## Low: pnpm Workspace Bugs with `tauri add`

Package manager detection fails in pnpm workspaces. `tauri add` detects npm instead of pnpm.

**Workaround:** Manually install npm packages and Cargo deps separately.

## Low: Performance Anti-Patterns

- Webview startup delay: 2-3s possible. Show loading state, don't show blank window.
- Every `invoke()` has overhead. Use events for push, not polling.
- Large DOM renders block the single-thread webview. Use virtualization.
- CSS transitions expensive on Linux WebKitGTK. Reduce/disable animations.
- Synchronous code in `setup()` delays window appearance. Offload to `tokio::spawn`.

## Testing Gaps

- **macOS has no WebDriver for WKWebView** — no automated E2E testing
- Playwright can't exercise Tauri IPC (mock only)
- Linux CI requires `Xvfb` virtual display
- Rust compilation: 10-20 min cold cache in CI. Use `sccache` + cargo registry cache.
- macOS CI runners cost 10x Linux. Run only on release branches.

## Scope Creep Risks

Cut these Electron patterns, don't port them:
- Background-process-heavy features (use user-triggered instead of always-on)
- Complex tray menus (simplify for Tauri)
- Node.js native addons (rewrite in Rust or use sidecar)
- Electron `remote` module patterns (redesign as explicit commands)
- DevTools in production (design explicit log export instead)

## Relations
- [[Project Brief]] — project context
- [[Stack Research]] — stack-level risks
- [[Features Research]] — feature-level risks
- [[Architecture Research]] — architecture-level mitigations