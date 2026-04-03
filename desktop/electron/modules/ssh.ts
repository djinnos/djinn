/**
 * SSH host persistence and tunnel management.
 *
 * SSH host management and tunnel handling
 */

import * as fs from "node:fs";
import * as net from "node:net";
import * as path from "node:path";
import { homedir } from "node:os";
import { ChildProcess, spawn, execSync } from "node:child_process";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface SshHost {
  id: string;
  label: string;
  hostname: string;
  user: string;
  port: number;
  key_path: string | null;
  remote_daemon_port: number;
  deployed: boolean;
  server_version: string | null;
}

/** Observable tunnel status emitted to the frontend. */
export type TunnelStatus =
  | { status: "disconnected" }
  | { status: "connecting" }
  | { status: "connected"; local_port: number }
  | { status: "reconnecting" }
  | { status: "error"; message: string };

/** Active SSH tunnel handle. */
export interface SshTunnel {
  process: ChildProcess;
  localPort: number;
  remotePort: number;
  hostId: string;
}

// ---------------------------------------------------------------------------
// SSH Host persistence
// ---------------------------------------------------------------------------

function hostsPath(): string {
  return path.join(homedir(), ".djinn", "ssh_hosts.json");
}

/** Load all saved SSH hosts from disk. */
export function loadHosts(): SshHost[] {
  const p = hostsPath();
  try {
    const content = fs.readFileSync(p, "utf-8");
    return JSON.parse(content) as SshHost[];
  } catch {
    return [];
  }
}

/** Persist the full host list to disk. */
export function saveHosts(hosts: SshHost[]): void {
  const p = hostsPath();
  const parent = path.dirname(p);
  fs.mkdirSync(parent, { recursive: true });
  const content = JSON.stringify(hosts, null, 2);
  fs.writeFileSync(p, content);
}

/** Find a host by its ID. */
export function findHost(id: string): SshHost | undefined {
  return loadHosts().find((h) => h.id === id);
}

/** Add a new host (or replace one with the same ID) and persist. */
export function addHost(host: SshHost): void {
  const hosts = loadHosts();
  const idx = hosts.findIndex((h) => h.id === host.id);
  if (idx >= 0) {
    hosts[idx] = host;
  } else {
    hosts.push(host);
  }
  saveHosts(hosts);
}

/** Remove a host by ID and persist. Throws if not found. */
export function removeHost(id: string): void {
  const hosts = loadHosts();
  const lenBefore = hosts.length;
  const filtered = hosts.filter((h) => h.id !== id);
  if (filtered.length === lenBefore) {
    throw new Error(`SSH host '${id}' not found`);
  }
  saveHosts(filtered);
}

/** Update an existing host in place and persist. Throws if not found. */
export function updateHost(host: SshHost): void {
  const hosts = loadHosts();
  const idx = hosts.findIndex((h) => h.id === host.id);
  if (idx < 0) {
    throw new Error(`SSH host '${host.id}' not found`);
  }
  hosts[idx] = host;
  saveHosts(hosts);
}

// ---------------------------------------------------------------------------
// Global tunnel state
// ---------------------------------------------------------------------------

let activeTunnel: SshTunnel | null = null;

/** Store a newly-created tunnel as the active one, stopping any previous tunnel. */
export function setActiveTunnel(tunnel: SshTunnel): void {
  if (activeTunnel) {
    stopTunnel(activeTunnel);
  }
  activeTunnel = tunnel;
}

/** Stop and remove the active tunnel (if any). */
export function stopActiveTunnel(): void {
  if (activeTunnel) {
    stopTunnel(activeTunnel);
    activeTunnel = null;
  }
}

/** Check whether the active tunnel's SSH process is still alive. */
export function isActiveTunnelAlive(): boolean {
  if (!activeTunnel) {
    return false;
  }
  return !activeTunnel.process.killed && activeTunnel.process.exitCode === null;
}

/** Get the local port of the active tunnel (if connected). */
export function activeTunnelLocalPort(): number | null {
  return activeTunnel?.localPort ?? null;
}

/** Get the host ID of the active tunnel (if any). */
export function activeTunnelHostId(): string | null {
  return activeTunnel?.hostId ?? null;
}

// ---------------------------------------------------------------------------
// Tunnel operations
// ---------------------------------------------------------------------------

/**
 * Start an SSH tunnel to the given host.
 *
 * Finds a free local port, then spawns:
 * `ssh -N -L {localPort}:127.0.0.1:{remoteDaemonPort} -p {port} [-i key] user@host`
 */
export async function startTunnel(host: SshHost): Promise<SshTunnel> {
  const localPort = await findFreePort();

  const args: string[] = [
    "-N",
    "-L",
    `${localPort}:127.0.0.1:${host.remote_daemon_port}`,
    "-p",
    host.port.toString(),
    "-o",
    "ExitOnForwardFailure=yes",
    "-o",
    "ServerAliveInterval=15",
    "-o",
    "BatchMode=yes",
    "-o",
    "StrictHostKeyChecking=accept-new",
  ];

  if (host.key_path) {
    args.push("-i", host.key_path);
  }

  args.push(`${host.user}@${host.hostname}`);

  const child = spawn("ssh", args, {
    stdio: ["ignore", "ignore", "pipe"],
  });

  child.on("error", (err) => {
    console.error(
      `Failed to spawn SSH tunnel to ${host.user}@${host.hostname}:${host.port}: ${err.message}`,
    );
  });

  console.log(
    `SSH tunnel started: local ${localPort} -> ${host.user}@${host.hostname}:${host.remote_daemon_port}`,
  );

  return {
    process: child,
    localPort,
    remotePort: host.remote_daemon_port,
    hostId: host.id,
  };
}

/** Kill the SSH child process. */
export function stopTunnel(tunnel: SshTunnel): void {
  console.log(
    `Stopping SSH tunnel (local port ${tunnel.localPort}, host ${tunnel.hostId})`,
  );
  tunnel.process.kill();
}

// ---------------------------------------------------------------------------
// Remote daemon management
// ---------------------------------------------------------------------------

const GITHUB_REPO = "djinnos/djinn";

/**
 * Ensure the remote djinn-server daemon is running on the host.
 *
 * If the binary isn't present on the remote, downloads the latest
 * Linux x64 release from GitHub before starting the daemon.
 */
export function ensureRemoteDaemon(host: SshHost): void {
  const downloadAndStart = `
if ! command -v ~/.djinn/bin/djinn-server &>/dev/null; then
    echo "Downloading djinn-server on remote..." >&2
    mkdir -p ~/.djinn/bin
    ASSET_URL=$(curl -sL "https://api.github.com/repos/${GITHUB_REPO}/releases" \\
        | grep -o '"browser_download_url":\\s*"[^"]*djinn-server-linux-x64[^"]*"' \\
        | head -1 | cut -d'"' -f4)
    if [ -z "$ASSET_URL" ]; then
        echo "Failed to find server release asset" >&2
        exit 1
    fi
    curl -sL "$ASSET_URL" -o ~/.djinn/bin/djinn-server
    chmod +x ~/.djinn/bin/djinn-server
fi
pgrep -f djinn-server || (nohup ~/.djinn/bin/djinn-server --port ${host.remote_daemon_port} &>/dev/null &)
`;
  const output = sshExec(host, downloadAndStart);
  console.log(`ensureRemoteDaemon output: ${output.trim()}`);
}

/** Test SSH connectivity to a host. Returns the output of `echo ok && uname -a` on success. */
export function testConnection(host: SshHost): string {
  return sshExecWithTimeout(host, "echo ok && uname -a", 5);
}

/** Run a command on the remote host via SSH. */
export function sshExec(host: SshHost, command: string): string {
  return sshExecWithTimeout(host, command, 10);
}

/** Run a command on the remote host via SSH with a custom timeout. */
function sshExecWithTimeout(
  host: SshHost,
  command: string,
  timeoutSec: number,
): string {
  const args: string[] = [
    "-p",
    host.port.toString(),
    "-o",
    `ConnectTimeout=${timeoutSec}`,
    "-o",
    "BatchMode=yes",
    "-o",
    "StrictHostKeyChecking=accept-new",
  ];

  if (host.key_path) {
    args.push("-i", host.key_path);
  }

  args.push(`${host.user}@${host.hostname}`);
  args.push(command);

  try {
    const output = execSync(`ssh ${args.map(shellEscape).join(" ")}`, {
      timeout: (timeoutSec + 5) * 1000, // extra buffer beyond ConnectTimeout
      encoding: "utf-8",
      stdio: ["pipe", "pipe", "pipe"],
    });
    return output;
  } catch (err: unknown) {
    const e = err as {
      status?: number | null;
      stderr?: string;
      stdout?: string;
      signal?: string | null;
    };
    const code =
      e.status !== null && e.status !== undefined
        ? e.status.toString()
        : e.signal ?? "signal";
    const stderr = (e.stderr ?? "").trim();
    const stdout = (e.stdout ?? "").trim();
    const detail = stderr || stdout || `exit code ${code} (no output)`;

    console.error(
      `ssh_exec command=${JSON.stringify(command)} host=${host.hostname} exit=${code} stderr=${JSON.stringify(stderr)} stdout=${JSON.stringify(stdout)}`,
    );
    throw new Error(`SSH command failed (exit ${code}): ${detail}`);
  }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Find a free TCP port on localhost by binding to port 0. */
export function findFreePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.listen(0, "127.0.0.1", () => {
      const addr = server.address();
      if (!addr || typeof addr === "string") {
        server.close();
        reject(new Error("Failed to find free port: unexpected address type"));
        return;
      }
      const port = addr.port;
      server.close(() => resolve(port));
    });
    server.on("error", (err) => reject(err));
  });
}

/** Escape a string for safe use in a shell command. */
function shellEscape(s: string): string {
  if (/^[a-zA-Z0-9_./:@=,-]+$/.test(s)) {
    return s;
  }
  return `'${s.replace(/'/g, "'\\''")}'`;
}
