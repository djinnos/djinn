/**
 * Daemon management, health monitoring, binary download.
 *
 * Ported from src-tauri/src/server.rs and server/crates/djinn-daemon/src/lib.rs
 */

import * as fs from "node:fs";
import * as path from "node:path";
import { homedir, platform, arch } from "node:os";
import { spawn } from "node:child_process";

import * as connectionMode from "./connection-mode.js";
import * as ssh from "./ssh.js";
import * as wsl from "./wsl.js";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export type SendEvent = (event: string, payload?: unknown) => void;

/** Runtime connection state — uses snake_case to match Tauri/Rust serialization. */
export interface ServerStatus {
  base_url: string | null;
  port: number | null;
  is_healthy: boolean;
  has_error: boolean;
  error_message: string | null;
  server_version: string | null;
  update_available: boolean;
}

// ---------------------------------------------------------------------------
// ServerState
// ---------------------------------------------------------------------------

export class ServerState {
  baseUrl: string | null = null;
  port: number | null = null;
  ready = false;
  isHealthy = false;
  hasError = false;
  errorMessage: string | null = null;
  tunnelStatus: ssh.TunnelStatus = { status: "disconnected" };
  serverVersion: string | null = null;
  updateAvailable = false;

  markHealthy(baseUrl: string): void {
    this.port = parsePort(baseUrl);
    this.baseUrl = baseUrl;
    this.isHealthy = true;
    this.hasError = false;
    this.errorMessage = null;
    this.ready = true;
    persistActiveConnection(baseUrl);
  }

  markError(message: string): void {
    this.isHealthy = false;
    this.hasError = true;
    this.errorMessage = message;
    this.ready = false;
  }

  toStatus(): ServerStatus {
    return {
      base_url: this.baseUrl,
      port: this.port,
      is_healthy: this.isHealthy,
      has_error: this.hasError,
      error_message: this.errorMessage,
      server_version: this.serverVersion,
      update_available: this.updateAvailable,
    };
  }
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const DEFAULT_PORT = 8372;
const GITHUB_REPO = "djinnos/djinn";
export const MIN_SERVER_VERSION = "0.1.0";

// ---------------------------------------------------------------------------
// Daemon info (mirrors djinn-daemon crate)
// ---------------------------------------------------------------------------

interface DaemonInfo {
  pid: number;
  port: number;
  started_at: string;
}

function daemonFilePath(): string {
  return path.join(homedir(), ".djinn", "daemon.json");
}

function readDaemonInfo(): DaemonInfo | null {
  const p = daemonFilePath();
  try {
    const content = fs.readFileSync(p, "utf-8");
    return JSON.parse(content) as DaemonInfo;
  } catch {
    return null;
  }
}

function writeDaemonInfo(info: DaemonInfo): void {
  const p = daemonFilePath();
  const parent = path.dirname(p);
  fs.mkdirSync(parent, { recursive: true });
  const content = JSON.stringify(info, null, 2) + "\n";
  fs.writeFileSync(p, content, { mode: 0o600 });
}

/** Check if a process with the given PID is alive. */
function pidIsAlive(pid: number): boolean {
  if (pid === 0) return false;
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

// ---------------------------------------------------------------------------
// Daemon lifecycle
// ---------------------------------------------------------------------------

/**
 * Ensure the djinn daemon is running and return the base URL.
 *
 * Checks `~/.djinn/daemon.json` for an existing daemon; if none is found
 * (or the recorded PID is dead), spawns a new `djinn-server` process that
 * detaches into its own session. The daemon survives desktop restarts.
 */
export async function ensureDaemon(): Promise<string> {
  let serverBin: string;
  try {
    serverBin = resolveServerBinary();
  } catch {
    serverBin = await downloadServerBinary();
  }

  // Check for existing daemon.
  const existing = readDaemonInfo();
  if (existing && pidIsAlive(existing.pid)) {
    console.log(
      `Daemon already running (pid=${existing.pid}, port=${existing.port})`,
    );
    const baseUrl = `http://127.0.0.1:${existing.port}`;

    // Wait for health endpoint.
    for (let i = 0; i < 40; i++) {
      if (await healthCheck(baseUrl)) {
        return baseUrl;
      }
      await sleep(100);
    }

    throw new Error(
      `Daemon process running (pid=${existing.pid}) but health endpoint at ${baseUrl}/health did not become ready`,
    );
  }

  // Spawn new daemon.
  const port = DEFAULT_PORT;
  const child = spawn(serverBin, ["--port", port.toString()], {
    detached: true,
    stdio: ["ignore", "ignore", "pipe"],
  });
  child.unref();

  const pid = child.pid;
  if (!pid) {
    throw new Error("Failed to spawn daemon process: no PID returned");
  }

  // Write daemon.json.
  writeDaemonInfo({
    pid,
    port,
    started_at: new Date().toISOString(),
  });

  const baseUrl = `http://127.0.0.1:${port}`;

  // Wait for health endpoint.
  for (let i = 0; i < 40; i++) {
    if (await healthCheck(baseUrl)) {
      return baseUrl;
    }
    await sleep(100);
  }

  throw new Error(
    `Daemon process started (pid=${pid}) but health endpoint at ${baseUrl}/health did not become ready`,
  );
}

// ---------------------------------------------------------------------------
// Health checks
// ---------------------------------------------------------------------------

/** HTTP GET `{baseUrl}/health` -- returns true if the server responds 2xx. */
export async function healthCheck(baseUrl: string): Promise<boolean> {
  const url = `${baseUrl.replace(/\/+$/, "")}/health`;
  try {
    const resp = await fetch(url);
    return resp.ok;
  } catch {
    return false;
  }
}

interface HealthResponseBody {
  status: string;
  version?: string;
}

/** HTTP GET `{baseUrl}/health` -- returns [healthy, version]. */
export async function healthCheckWithVersion(
  baseUrl: string,
): Promise<[boolean, string | null]> {
  const url = `${baseUrl.replace(/\/+$/, "")}/health`;
  try {
    const resp = await fetch(url);
    if (!resp.ok) return [false, null];
    try {
      const body = (await resp.json()) as HealthResponseBody;
      return [true, body.version ?? null];
    } catch {
      return [true, null]; // healthy but no version (old server)
    }
  } catch {
    return [false, null];
  }
}

// ---------------------------------------------------------------------------
// Version comparison
// ---------------------------------------------------------------------------

function parseSemver(v: string): [number, number, number] | null {
  const parts = v.split(".");
  if (parts.length !== 3) return null;
  const nums = parts.map(Number);
  if (nums.some(isNaN)) return null;
  return nums as [number, number, number];
}

export function versionLt(current: string, minimum: string): boolean {
  const c = parseSemver(current);
  const m = parseSemver(minimum);
  if (!c || !m) return false;
  if (c[0] !== m[0]) return c[0] < m[0];
  if (c[1] !== m[1]) return c[1] < m[1];
  return c[2] < m[2];
}

// ---------------------------------------------------------------------------
// Connection retry
// ---------------------------------------------------------------------------

/**
 * Retry connecting to the server according to the current connection mode.
 */
export async function retryConnection(
  state: ServerState,
  sendEvent: SendEvent,
): Promise<string> {
  // Clear existing error state.
  state.hasError = false;
  state.errorMessage = null;

  // When DJINN_SERVER_BIN is set, force local daemon mode.
  const mode: connectionMode.ConnectionMode =
    process.env.DJINN_SERVER_BIN !== undefined
      ? { type: "daemon" }
      : connectionMode.load();

  let baseUrl: string;

  switch (mode.type) {
    case "daemon":
      baseUrl = await ensureDaemon();
      break;

    case "remote":
      if (await healthCheck(mode.url)) {
        baseUrl = mode.url;
      } else {
        throw new Error(`Remote server at ${mode.url} is not reachable`);
      }
      break;

    case "ssh": {
      const host = ssh.findHost(mode.host_id);
      if (!host) {
        throw new Error(`SSH host '${mode.host_id}' not found`);
      }

      ssh.ensureRemoteDaemon(host);

      const tunnel = await ssh.startTunnel(host);
      baseUrl = `http://127.0.0.1:${tunnel.localPort}`;

      // Wait for health through tunnel.
      let tunnelHealthy = false;
      for (let i = 0; i < 40; i++) {
        if (await healthCheck(baseUrl)) {
          tunnelHealthy = true;
          break;
        }
        await sleep(250);
      }

      if (!tunnelHealthy) {
        ssh.stopTunnel(tunnel);
        throw new Error(
          "SSH tunnel established but daemon not reachable through it",
        );
      }

      const localPort = tunnel.localPort;
      ssh.setActiveTunnel(tunnel);

      const [, sshVersion] = await healthCheckWithVersion(baseUrl);
      state.tunnelStatus = { status: "connected", local_port: localPort };
      state.markHealthy(baseUrl);
      state.serverVersion = sshVersion;
      state.updateAvailable = sshVersion
        ? versionLt(sshVersion, MIN_SERVER_VERSION)
        : false;

      startHealthMonitor(state, sendEvent);
      startTunnelMonitor(state, sendEvent);
      return baseUrl;
    }

    case "wsl":
      baseUrl = await wsl.ensureWslDaemon(DEFAULT_PORT);
      break;
  }

  const [, version] = await healthCheckWithVersion(baseUrl);
  state.markHealthy(baseUrl);
  state.serverVersion = version;
  state.updateAvailable = version
    ? versionLt(version, MIN_SERVER_VERSION)
    : false;

  startHealthMonitor(state, sendEvent);

  return baseUrl;
}

// ---------------------------------------------------------------------------
// Health monitor
// ---------------------------------------------------------------------------

/**
 * Spawn a background task that periodically health-checks the server.
 *
 * On failure it tries to re-discover (re-read `baseUrl` from state) and
 * emits `server:reconnected` or `server:disconnected` events.
 */
export function startHealthMonitor(
  state: ServerState,
  sendEvent: SendEvent,
): void {
  (async () => {
    // Wait for initial startup to settle.
    await sleep(5000);

    let wasHealthy = true;

    // eslint-disable-next-line no-constant-condition
    while (true) {
      await sleep(3000);

      const currentUrl = state.baseUrl;
      if (!currentUrl) continue; // startup hasn't finished

      const [healthy, version] = await healthCheckWithVersion(currentUrl);
      if (healthy) {
        if (!wasHealthy) {
          console.log(`Health monitor: server recovered at ${currentUrl}`);
          state.markHealthy(currentUrl);
          state.serverVersion = version;
          state.updateAvailable = version
            ? versionLt(version, MIN_SERVER_VERSION)
            : false;
          sendEvent("server:reconnected", currentUrl);

          // Re-sync tokens on reconnection is handled by the main process
          // since it has access to auth state -- we just emit the event.
          wasHealthy = true;
        }
        continue;
      }

      console.warn(
        `Health monitor: server at ${currentUrl} is unreachable`,
      );

      if (wasHealthy) {
        state.isHealthy = false;
        sendEvent("server:disconnected");
        wasHealthy = false;
      }
    }
  })().catch((err) => {
    console.error("Health monitor error:", err);
  });
}

// ---------------------------------------------------------------------------
// Tunnel monitor
// ---------------------------------------------------------------------------

/**
 * Spawn a background task that monitors the SSH tunnel health.
 *
 * Every 5 seconds it checks whether the SSH child process is still alive.
 * If the tunnel dies, it emits `tunnel:disconnected` and attempts to
 * reconnect. On successful reconnection it emits `tunnel:reconnected`.
 */
export function startTunnelMonitor(
  state: ServerState,
  sendEvent: SendEvent,
): void {
  (async () => {
    // Let the initial connection settle.
    await sleep(5000);

    // eslint-disable-next-line no-constant-condition
    while (true) {
      await sleep(5000);

      if (!ssh.isActiveTunnelAlive()) {
        console.warn("Tunnel monitor: SSH tunnel process has exited");

        state.tunnelStatus = { status: "reconnecting" };
        sendEvent("tunnel:disconnected");

        // Attempt reconnection.
        const hostId = ssh.activeTunnelHostId();
        if (!hostId) {
          // No host_id -- tunnel was cleared, stop monitoring.
          console.log(
            "Tunnel monitor: no active tunnel, stopping monitor",
          );
          break;
        }

        const host = ssh.findHost(hostId);
        if (!host) {
          console.error(
            `Tunnel monitor: host ${hostId} not found, stopping monitor`,
          );
          break;
        }

        try {
          const tunnel = await ssh.startTunnel(host);
          const localPort = tunnel.localPort;
          const baseUrl = `http://127.0.0.1:${localPort}`;
          ssh.setActiveTunnel(tunnel);

          // Wait briefly for health.
          let reconnected = false;
          for (let i = 0; i < 20; i++) {
            if (await healthCheck(baseUrl)) {
              reconnected = true;
              break;
            }
            await sleep(250);
          }

          if (reconnected) {
            console.log(
              `Tunnel monitor: reconnected on local port ${localPort}`,
            );
            state.tunnelStatus = {
              status: "connected",
              local_port: localPort,
            };
            state.markHealthy(baseUrl);
            sendEvent("tunnel:reconnected", baseUrl);
          } else {
            console.error(
              "Tunnel monitor: reconnected tunnel but daemon not reachable",
            );
            state.tunnelStatus = {
              status: "error",
              message:
                "Tunnel reconnected but daemon not reachable",
            };
          }
        } catch (e) {
          const msg = e instanceof Error ? e.message : String(e);
          console.error(
            `Tunnel monitor: failed to reconnect: ${msg}`,
          );
          state.tunnelStatus = { status: "error", message: msg };
        }
      }
    }
  })().catch((err) => {
    console.error("Tunnel monitor error:", err);
  });
}

// ---------------------------------------------------------------------------
// Binary resolution & download
// ---------------------------------------------------------------------------

/**
 * Resolve the `djinn-server` binary path.
 *
 * Search order:
 * 1. `DJINN_SERVER_BIN` environment variable (explicit override)
 * 2. `~/.djinn/bin/djinn-server` (previously downloaded)
 * 3. Throw -- caller should download via `downloadServerBinary()`
 */
export function resolveServerBinary(): string {
  // 1. Explicit override via env var.
  const envPath = process.env.DJINN_SERVER_BIN;
  if (envPath !== undefined) {
    if (fs.existsSync(envPath)) {
      return envPath;
    }
    throw new Error(
      `DJINN_SERVER_BIN is set to ${envPath} but that file does not exist`,
    );
  }

  // 2. Managed binary at ~/.djinn/bin/djinn-server.
  const managed = managedBinaryPath();
  if (managed && fs.existsSync(managed)) {
    return managed;
  }

  // 3. Not found.
  throw new Error(
    "djinn-server binary not found. It will be downloaded on first connection.",
  );
}

/** Return the path where the managed server binary lives: `~/.djinn/bin/djinn-server`. */
export function managedBinaryPath(): string {
  const name = platform() === "win32" ? "djinn-server.exe" : "djinn-server";
  return path.join(homedir(), ".djinn", "bin", name);
}

/** Map the current platform to the server release asset name. */
function serverAssetName(): string | null {
  const p = platform();
  const a = arch();
  if (p === "linux" && a === "x64") return "djinn-server-linux-x64";
  if (p === "darwin" && a === "arm64") return "djinn-server-macos-arm64";
  return null;
}

/**
 * Download the latest server binary from GitHub releases to `~/.djinn/bin/`.
 */
export async function downloadServerBinary(): Promise<string> {
  const dest = managedBinaryPath();
  const parent = path.dirname(dest);
  fs.mkdirSync(parent, { recursive: true });

  const assetName = serverAssetName();
  if (!assetName) {
    throw new Error("Unsupported platform for server download");
  }

  console.log(
    `Downloading djinn-server (${assetName}) from GitHub releases...`,
  );

  // Find the latest server-v* release.
  const releasesUrl = `https://api.github.com/repos/${GITHUB_REPO}/releases`;
  const resp = await fetch(releasesUrl, {
    headers: { "User-Agent": "djinn-desktop" },
  });
  if (!resp.ok) {
    throw new Error(`Fetch releases failed: ${resp.status}`);
  }

  const releases = (await resp.json()) as Array<{
    tag_name: string;
    assets: Array<{ name: string; browser_download_url: string }>;
  }>;

  const release = releases.find((r) => r.tag_name.startsWith("server-v"));
  if (!release) {
    throw new Error("No server release found on GitHub");
  }

  const asset = release.assets.find((a) => a.name === assetName);
  if (!asset) {
    throw new Error(`Asset ${assetName} not found in release`);
  }

  const dlResp = await fetch(asset.browser_download_url, {
    headers: { "User-Agent": "djinn-desktop" },
  });
  if (!dlResp.ok) {
    throw new Error(`Download ${assetName} failed: ${dlResp.status}`);
  }

  const bytes = Buffer.from(await dlResp.arrayBuffer());
  fs.writeFileSync(dest, bytes);

  ensureExecutable(dest);

  console.log(`Downloaded djinn-server to ${dest}`);
  return dest;
}

/** Ensure a file has executable permission (u+x). Unix only. */
function ensureExecutable(filePath: string): void {
  if (platform() === "win32") return;
  try {
    const stat = fs.statSync(filePath);
    const mode = stat.mode;
    if ((mode & 0o111) === 0) {
      fs.chmodSync(filePath, mode | 0o755);
    }
  } catch {
    // best-effort
  }
}

// ---------------------------------------------------------------------------
// Active connection persistence
// ---------------------------------------------------------------------------

/**
 * Write `~/.djinn/active_connection.json` so that out-of-band callers
 * (e.g. token_sync) can resolve the server URL without access to state.
 */
export function persistActiveConnection(baseUrl: string): void {
  const p = path.join(homedir(), ".djinn", "active_connection.json");
  const payload = JSON.stringify(
    { base_url: baseUrl, port: parsePort(baseUrl) },
    null,
    2,
  );
  try {
    fs.writeFileSync(p, payload);
  } catch {
    // best-effort
  }
}

/**
 * Read the active server base URL from `~/.djinn/active_connection.json`.
 */
export function loadActiveConnectionUrl(): string | null {
  const p = path.join(homedir(), ".djinn", "active_connection.json");
  try {
    const content = fs.readFileSync(p, "utf-8");
    const json = JSON.parse(content) as { base_url?: string };
    return json.base_url ?? null;
  } catch {
    return null;
  }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Best-effort port extraction from a URL string. */
function parsePort(url: string): number | null {
  const afterScheme = url.split("//")[1] ?? url;
  const hostPort = afterScheme.split("/")[0] ?? afterScheme;
  const portStr = hostPort.split(":").pop();
  if (!portStr) return null;
  const port = parseInt(portStr, 10);
  return isNaN(port) ? null : port;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
