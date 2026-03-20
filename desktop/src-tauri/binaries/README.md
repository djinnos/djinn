# Sidecar Binaries

This directory contains external binary sidecars that are bundled with the DjinnOS Desktop application.

## djinn-server

The `djinn-server` binary is the DjinnOS server backend that runs as a sidecar process.

### Platform-Triple Naming Convention

Tauri looks for sidecar binaries using the platform target triple:

- `djinn-server-x86_64-pc-windows-msvc.exe` - Windows x64
- `djinn-server-x86_64-apple-darwin` - macOS x64
- `djinn-server-aarch64-apple-darwin` - macOS ARM64 (Apple Silicon)
- `djinn-server-x86_64-unknown-linux-gnu` - Linux x64

The `externalBin` configuration in `tauri.conf.json` references `binaries/djinn-server`, and Tauri automatically appends the platform triple and extension.

### Building

The server binary is built from the `/server` workspace and copied here before bundling.
