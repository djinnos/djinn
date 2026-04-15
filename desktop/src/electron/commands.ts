/**
 * Electron IPC command wrappers
 *
 * All invoke() calls go through this module. Never call invoke() directly
 * in components - always use these wrapper functions.
 *
 * The server now runs via docker-compose on localhost:8372, so these
 * wrappers are thin — no daemon lifecycle, no SSH, no binary download.
 */

import { invoke } from "./shims/invoke";

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
  return invoke("get_server_port");
}

/**
 * Get the full server base URL from the Electron backend.
 */
export async function getServerUrl(): Promise<string> {
  return invoke("get_server_url");
}

/**
 * Probe the server /health endpoint.
 * @returns Object describing whether the configured server URL is reachable.
 */
export async function checkServerAvailable(): Promise<{
  ok: boolean;
  baseUrl: string;
  error?: string;
}> {
  return invoke("check_server_available");
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
 * List local git branches for a project.
 * @param projectPath Absolute path to the project directory
 * @returns Array of branch names
 */
export async function listGitBranches(projectPath: string): Promise<string[]> {
  return invoke<string[]>("list_git_branches", { projectPath });
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
 * Re-sync locally stored GitHub tokens to the server credential vault.
 * Call when the server may have lost credential state (e.g. after restart).
 * @returns true if tokens were synced, false if no local tokens available
 */
export async function syncGithubTokens(): Promise<boolean> {
  return invoke<boolean>("sync_github_tokens");
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

/**
 * Open a native file picker dialog.
 * @param title Optional dialog title
 * @returns The selected file path or null if cancelled
 */
export async function selectFile(title?: string): Promise<string | null> {
  return invoke<string | null>("select_file", { title });
}

/**
 * Attempt silent authentication using stored refresh tokens.
 * Called after the server is connected.
 * @returns true if authentication succeeded
 */
export async function attemptSilentAuth(): Promise<boolean> {
  return invoke<boolean>("attempt_silent_auth");
}
