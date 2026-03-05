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

/**
 * Get the server port from the Tauri backend.
 * @returns The port number the backend server is running on
 */
export async function getServerPort(): Promise<number> {
  return invoke("get_server_port");
}

/**
 * Get the server status from the Tauri backend.
 * @returns The current server status including health and error state
 */
export async function getServerStatus(): Promise<{
  port: number | null;
  is_healthy: boolean;
  has_error: boolean;
  error_message: string | null;
}> {
  return invoke("get_server_status");
}

/**
 * Retry server discovery/spawn.
 * Called when the user clicks the retry button in the error state.
 * @returns The port number the server is running on
 */
export async function retryServerDiscovery(): Promise<number> {
  return invoke("retry_server_discovery");
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
 * Get current authentication state.
 */
export async function authGetState(): Promise<AuthState> {
  return invoke<AuthState>("auth_get_state");
}

/**
 * Start OAuth login flow.
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
