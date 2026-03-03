---
tags:
    - research
    - stack
title: Stack Research
type: research
---
# Stack Research — Tauri 2.x + React 19 Desktop

## Core Stack

| Layer | Technology | Version | Notes |
|-------|-----------|---------|-------|
| Desktop shell | Tauri | 2.10.2 | OS webview, ~10 MB bundle |
| Frontend | React | 19.x | Works in Tauri webviews without issues |
| Build tool | Vite | 5.x | Tauri's official recommendation |
| Styling | Tailwind CSS | 4.x | Utility-first |
| Components | shadcn/ui (Base UI) | Latest | Base UI fully supported since Jan 2026 |
| State | Zustand | 5.x | SSE-driven stores with `subscribeWithSelector` |
| Data fetching | TanStack Query | 5.x | `queryOptions` pattern for MCP calls |
| Package manager | pnpm | 9.x | Known workspace bugs with `tauri add` — workaround: manual install |

## Tauri Plugins Required

| Plugin | Purpose |
|--------|---------|
| `tauri-plugin-shell` | Spawn server sidecar binary |
| `tauri-plugin-updater` | Auto-update via GitHub Releases (desktop-only, cfg-gated) |
| `tauri-plugin-process` | `relaunch()` after update, `exit()` |
| `tauri-plugin-single-instance` | Prevent duplicate app instances |

## Key Configuration

### Vite Build Targets
- Windows: `chrome105` (WebView2 is Chromium-based)
- macOS/Linux: `safari13` (WebKit-based webview)

### Version Sync Rule (Enforced)
The minor version of `tauri` crate, `tauri-build`, and `@tauri-apps/cli` must match. Plugin crate versions must exactly match their npm counterparts.

### shadcn/ui Base UI Setup
Base UI is a first-class primitive layer since January 2026. All 80+ components available. Selected at project creation time:
```bash
pnpm dlx shadcn@latest create --preset "https://ui.shadcn.com/init?base=base&style=mira&baseColor=zinc&theme=violet&iconLibrary=hugeicons&font=geist&menuAccent=subtle&menuColor=default&radius=small&template=vite&rtl=false" --template vite
```

### Communication Model
- **Desktop → Server**: Plain `fetch()` to `http://127.0.0.1:{port}`. No Tauri IPC proxy needed.
- **Server → Desktop**: SSE via `EventSource` to `/events` endpoint.
- **Tauri invoke**: Reserved for native-only operations (get server port, auth token management).

### SSE + Zustand Pattern
Use vanilla `createStore` for SSE lifecycle (outside React). Connect SSE events to TanStack Query via `queryClient.invalidateQueries()` or `queryClient.setQueryData()`.

### TanStack Query Config for Desktop
```typescript
staleTime: 30_000,        // 30s
gcTime: 300_000,          // 5m
retry: 1,
refetchOnWindowFocus: false,  // desktop, no tabs
refetchOnReconnect: true,
```

### Auto-Update Pipeline
- `tauri-plugin-updater` points to `latest.json` on GitHub Releases
- `tauri-action@v0` in CI generates and uploads `latest.json` automatically
- ed25519 signing keypair required (mandatory, cannot be disabled)
- `includeUpdaterJson: true` in the GitHub Action config

## Relations
- [[Project Brief]] — project context
- [[Features Research]] — feature needs inform stack choice
- [[Architecture Research]] — architecture patterns drive stack decisions
- [[Pitfalls Research]] — risks to consider in stack selection