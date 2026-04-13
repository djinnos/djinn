/**
 * Electron IPC command wrappers
 *
 * All invoke() calls go through this module. Never call invoke() directly
 * in components - always use these wrapper functions.
 */

import { invoke } from "./shims/invoke";

const BROWSER_SERVER_URL_STORAGE_KEY = "djinn.serverBaseUrl";
const BROWSER_DEFAULT_SERVER_URL = "http://127.0.0.1:3000";

export interface ServerStatus {
  base_url: string | null;
  port: number | null;
  is_healthy: boolean;
  has_error: boolean;
  error_message: string | null;
  server_version: string | null;
  update_available: boolean;
}

function hasElectronApi(): boolean {
  return typeof window !== "undefined" && typeof window.electronAPI?.invoke === "function";
}

function trimTrailingSlash(value: string): string {
  return value.replace(/\/+$/, "");
}

function getBrowserStoredServerUrl(): string | null {
  if (typeof window === "undefined") return null;

  try {
    return window.localStorage.getItem(BROWSER_SERVER_URL_STORAGE_KEY);
  } catch {
    return null;
  }
}

function setBrowserStoredServerUrl(url: string | null): void {
  if (typeof window === "undefined") return;

  try {
    if (url) {
      window.localStorage.setItem(BROWSER_SERVER_URL_STORAGE_KEY, url);
    } else {
      window.localStorage.removeItem(BROWSER_SERVER_URL_STORAGE_KEY);
    }
  } catch {
    // Ignore localStorage failures in restricted browser environments.
  }
}

function getBrowserServerUrl(): string {
  const configuredUrl = import.meta.env.VITE_DJINN_SERVER_URL?.trim();
  if (configuredUrl) {
    return trimTrailingSlash(configuredUrl);
  }

  const storedUrl = getBrowserStoredServerUrl()?.trim();
  if (storedUrl) {
    return trimTrailingSlash(storedUrl);
  }

  if (typeof window !== "undefined" && /^https?:$/.test(window.location.protocol)) {
    return trimTrailingSlash(window.location.origin);
  }

  return BROWSER_DEFAULT_SERVER_URL;
}

function portFromUrl(baseUrl: string): number | null {
  try {
    const url = new URL(baseUrl);
    if (url.port) return Number(url.port);
    if (url.protocol === "https:") return 443;
    if (url.protocol === "http:") return 80;
    return null;
  } catch {
    return null;
  }
}

async function getBrowserServerStatus(): Promise<ServerStatus> {
  const baseUrl = getBrowserServerUrl();

  try {
    const response = await fetch(`${baseUrl}/health`);
    if (!response.ok) {
      return {
        base_url: baseUrl,
        port: portFromUrl(baseUrl),
        is_healthy: false,
        has_error: true,
        error_message: `Health check failed: ${response.status}`,
        server_version: null,
        update_available: false,
      };
    }

    let serverVersion: string | null = null;
    try {
      const payload = (await response.json()) as { version?: string };
      serverVersion = typeof payload.version === "string" ? payload.version : null;
    } catch {
      serverVersion = null;
    }

    return {
      base_url: baseUrl,
      port: portFromUrl(baseUrl),
      is_healthy: true,
      has_error: false,
      error_message: null,
      server_version: serverVersion,
      update_available: false,
    };
  } catch (error) {
    return {
      base_url: baseUrl,
      port: portFromUrl(baseUrl),
      is_healthy: false,
      has_error: true,
      error_message: error instanceof Error ? error.message : "Failed to reach server",
      server_version: null,
      update_available: false,
    };
  }
}

export interface AuthUser {
  sub: string;
  name?: string;
  email?: string;
  picture?: string;
}

export interface AuthState {
  isAuthenticated: boolean;
  user: AuthUser | null;
}

export interface DeviceCodeInfo {
  userCode: string;
  verificationUri: string;
}

/**
 * Start GitHub device code login flow.
 * Returns the user code and verification URI for the user to enter on github.com.
 * The backend will poll in the background and emit auth:state-changed when done.
 */
export async function startGithubLogin(): Promise<DeviceCodeInfo> {
  return invoke<DeviceCodeInfo>("start_github_login");
}

/**
 * Get the server port from the Electron backend.
 * @returns The port number the backend server is running on
 */
export async function getServerPort(): Promise<number> {
  if (!hasElectronApi()) {
    return portFromUrl(getBrowserServerUrl()) ?? 80;
  }
  return invoke("get_server_port");
}

/**
 * Get the full server base URL from the Electron backend.
 */
export async function getServerUrl(): Promise<string> {
  if (!hasElectronApi()) {
    return getBrowserServerUrl();
  }
  return invoke("get_server_url");
}

/**
 * Get the server status from the Electron backend.
 * @returns The current server status including health and error state
 */
export async function getServerStatus(): Promise<ServerStatus> {
  if (!hasElectronApi()) {
    return getBrowserServerStatus();
  }
  return invoke("get_server_status");
}

/**
 * Retry connecting to the server (embedded or remote).
 * Called when the user clicks the retry button in the error state.
 * @returns The base URL the server is reachable at
 */
export async function retryServerConnection(): Promise<string> {
  if (!hasElectronApi()) {
    const status = await getBrowserServerStatus();
    if (!status.is_healthy || !status.base_url) {
      throw new Error(status.error_message ?? "Failed to connect to server");
    }
    return status.base_url;
  }
  return invoke("retry_server_connection");
}

export async function retryServerDiscovery(): Promise<number> {
  if (!hasElectronApi()) {
    return getServerPort();
  }
  return invoke("retry_server_discovery");
}

export type ConnectionMode =
  | { type: "daemon" }
  | { type: "remote"; url: string }
  | { type: "ssh"; host_id: string }
  | { type: "wsl" };

export async function getConnectionMode(): Promise<ConnectionMode> {
  if (!hasElectronApi()) {
    return { type: "remote", url: getBrowserServerUrl() };
  }
  return invoke("get_connection_mode");
}

export async function setConnectionMode(mode: ConnectionMode): Promise<void> {
  if (!hasElectronApi()) {
    if (mode.type === "remote") {
      setBrowserStoredServerUrl(trimTrailingSlash(mode.url));
      return;
    }

    if (mode.type === "daemon") {
      setBrowserStoredServerUrl(null);
      return;
    }

    throw new Error(`Connection mode '${mode.type}' is not supported in browser mode`);
  }
  return invoke("set_connection_mode", { mode });
}

/**
 * Open a native directory picker dialog.
 * @param title Optional dialog title
 * @returns The selected directory path or null if cancelled
 */
export async function selectDirectory(title?: string): Promise<string | null> {
  if (!hasElectronApi()) {
    void title;
    throw new Error("Directory selection is only available in Electron mode");
  }
  const result = await invoke<string | null>("select_directory", { title });
  return result;
}


/**
 * List local git branches for a project.
 * @param projectPath Absolute path to the project directory
 * @returns Array of branch names
 */
export async function listGitBranches(projectPath: string): Promise<string[]> {
  if (!hasElectronApi()) {
    void projectPath;
    return [];
  }
  return invoke<string[]>("list_git_branches", { projectPath });
}

/**
 * Check if a git repository has an 'origin' remote configured.
 * @param projectPath Absolute path to the project directory
 * @returns The remote URL if configured, null otherwise
 */
export async function checkGitRemote(projectPath: string): Promise<string | null> {
  if (!hasElectronApi()) {
    void projectPath;
    throw new Error("Git remote checks are only available in Electron mode");
  }
  return invoke<string | null>("check_git_remote", { projectPath });
}

/**
 * Set up a git remote and push the current branch.
 * @param projectPath Absolute path to the project directory
 * @param remoteUrl The git remote URL (HTTPS or SSH)
 * @returns Success message
 */
export async function setupGitRemote(projectPath: string, remoteUrl: string): Promise<string> {
  if (!hasElectronApi()) {
    void projectPath;
    void remoteUrl;
    throw new Error("Git remote setup is only available in Electron mode");
  }
  return invoke<string>("setup_git_remote", { projectPath, remoteUrl });
}

/**
 * Re-sync locally stored GitHub tokens to the server credential vault.
 * Call when the server may have lost credential state (e.g. after restart).
 * @returns true if tokens were synced, false if no local tokens available
 */
export async function syncGithubTokens(): Promise<boolean> {
  if (!hasElectronApi()) {
    return false;
  }
  return invoke<boolean>("sync_github_tokens");
}

/**
 * Get current authentication state.
 */
export async function authGetState(): Promise<AuthState> {
  if (!hasElectronApi()) {
    return { isAuthenticated: true, user: null };
  }
  return invoke<AuthState>("auth_get_state");
}

/**
 * Start OAuth login flow (backwards-compatible wrapper for authStore).
 */
export async function authLogin(): Promise<void> {
  if (!hasElectronApi()) {
    return;
  }
  await invoke("auth_login");
}

/**
 * Logout current authenticated user.
 */
export async function authLogout(): Promise<void> {
  if (!hasElectronApi()) {
    return;
  }
  await invoke("auth_logout");
}

// --- Connection Settings Types ---

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

export type TunnelStatus =
  | { status: "disconnected" }
  | { status: "connecting" }
  | { status: "connected"; local_port: number }
  | { status: "reconnecting" }
  | { status: "error"; message: string };

// --- Connection Settings Commands ---

/**
 * Get saved SSH hosts from the backend.
 */
export async function getSshHosts(): Promise<SshHost[]> {
  if (!hasElectronApi()) {
    return [];
  }
  return invoke<SshHost[]>("get_ssh_hosts");
}

/**
 * Save (create or update) an SSH host configuration.
 */
export async function saveSshHost(host: SshHost): Promise<void> {
  if (!hasElectronApi()) {
    void host;
    throw new Error("SSH host management is only available in Electron mode");
  }
  return invoke<void>("save_ssh_host", { host });
}

/**
 * Remove an SSH host configuration by ID.
 */
export async function removeSshHost(id: string): Promise<void> {
  if (!hasElectronApi()) {
    void id;
    throw new Error("SSH host management is only available in Electron mode");
  }
  return invoke<void>("remove_ssh_host", { id });
}

/**
 * Test SSH connectivity to a saved host.
 * @returns A success message or throws on failure.
 */
export async function testSshConnection(hostId: string): Promise<string> {
  if (!hasElectronApi()) {
    void hostId;
    throw new Error("SSH connection testing is only available in Electron mode");
  }
  return invoke<string>("test_ssh_connection", { hostId });
}

/**
 * Get the current SSH tunnel status.
 */
export async function getTunnelStatus(): Promise<TunnelStatus> {
  if (!hasElectronApi()) {
    return { status: "disconnected" };
  }
  return invoke<TunnelStatus>("get_tunnel_status");
}

/**
 * Deploy the djinn-server binary to a remote host via SSH.
 * @returns A status message.
 */
export async function deployServerToHost(hostId: string): Promise<string> {
  if (!hasElectronApi()) {
    void hostId;
    throw new Error("Server deployment is only available in Electron mode");
  }
  return invoke<string>("deploy_server_to_host", { hostId });
}

/**
 * Check if WSL is available on this machine.
 */
export async function checkWslAvailable(): Promise<boolean> {
  if (!hasElectronApi()) {
    return false;
  }
  return invoke<boolean>("check_wsl_available");
}

/**
 * Open a native file picker dialog.
 * @param title Optional dialog title
 * @returns The selected file path or null if cancelled
 */
export async function selectFile(title?: string): Promise<string | null> {
  if (!hasElectronApi()) {
    void title;
    throw new Error("File selection is only available in Electron mode");
  }
  return invoke<string | null>("select_file", { title });
}

/**
 * Download the server binary from GitHub releases.
 * @returns The path to the downloaded binary
 */
export async function downloadServerBinary(): Promise<string> {
  if (!hasElectronApi()) {
    throw new Error("Server binary downloads are only available in Electron mode");
  }
  return invoke<string>("download_server_binary");
}

/**
 * Check if a saved connection mode exists (first-launch detection).
 */
export async function hasSavedConnectionMode(): Promise<boolean> {
  if (!hasElectronApi()) {
    return true;
  }
  return invoke<boolean>("has_saved_connection_mode");
}

/**
 * Attempt silent authentication using stored refresh tokens.
 * Called after the server is connected.
 * @returns true if authentication succeeded
 */
export async function attemptSilentAuth(): Promise<boolean> {
  if (!hasElectronApi()) {
    return true;
  }
  return invoke<boolean>("attempt_silent_auth");
}

export function isElectronRuntime(): boolean {
  return hasElectronApi();
}
