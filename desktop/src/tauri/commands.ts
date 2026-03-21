/**
 * Tauri command wrappers
 *
 * All invoke() calls go through this module. Never call invoke() directly
 * in components - always use these wrapper functions.
 */

import { invoke } from "@tauri-apps/api/core";

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
 * Get the server port from the Tauri backend.
 * @returns The port number the backend server is running on
 */
export async function getServerPort(): Promise<number> {
  return invoke("get_server_port");
}

/**
 * Get the full server base URL from the Tauri backend.
 */
export async function getServerUrl(): Promise<string> {
  return invoke("get_server_url");
}

/**
 * Get the server status from the Tauri backend.
 * @returns The current server status including health and error state
 */
export async function getServerStatus(): Promise<{
  base_url: string | null;
  port: number | null;
  is_healthy: boolean;
  has_error: boolean;
  error_message: string | null;
}> {
  return invoke("get_server_status");
}

/**
 * Retry connecting to the server (embedded or remote).
 * Called when the user clicks the retry button in the error state.
 * @returns The base URL the server is reachable at
 */
export async function retryServerConnection(): Promise<string> {
  return invoke("retry_server_connection");
}

/** @deprecated Use retryServerConnection instead */
export async function retryServerDiscovery(): Promise<number> {
  const url = await retryServerConnection();
  const match = url.match(/:(\d+)/);
  return match ? parseInt(match[1], 10) : 8372;
}

export type ConnectionMode =
  | { type: "embedded" }
  | { type: "remote"; url: string };

export async function getConnectionMode(): Promise<ConnectionMode> {
  return invoke("get_connection_mode");
}

export async function setConnectionMode(mode: ConnectionMode): Promise<void> {
  return invoke("set_connection_mode", { mode });
}

/**
 * Open a native directory picker dialog.
 * @param title Optional dialog title
 * @returns The selected directory path or null if cancelled
 */
export async function selectDirectory(title?: string): Promise<string | null> {
  const result = await invoke<string | null>("select_directory", { title });
  return result;
}


/**
 * Check if a git repository has an 'origin' remote configured.
 * @param projectPath Absolute path to the project directory
 * @returns The remote URL if configured, null otherwise
 */
export async function checkGitRemote(projectPath: string): Promise<string | null> {
  return invoke<string | null>("check_git_remote", { projectPath });
}

/**
 * Set up a git remote and push the current branch.
 * @param projectPath Absolute path to the project directory
 * @param remoteUrl The git remote URL (HTTPS or SSH)
 * @returns Success message
 */
export async function setupGitRemote(projectPath: string, remoteUrl: string): Promise<string> {
  return invoke<string>("setup_git_remote", { projectPath, remoteUrl });
}

/**
 * Get current authentication state.
 */
export async function authGetState(): Promise<AuthState> {
  return invoke<AuthState>("auth_get_state");
}

/**
 * Start OAuth login flow (backwards-compatible wrapper for authStore).
 */
export async function authLogin(): Promise<void> {
  await invoke("auth_login");
}

/**
 * Logout current authenticated user.
 */
export async function authLogout(): Promise<void> {
  await invoke("auth_logout");
}
