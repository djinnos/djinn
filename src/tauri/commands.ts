/**
 * Tauri command wrappers
 *
 * All invoke() calls go through this module. Never call invoke() directly
 * in components - always use these wrapper functions.
 */

import { invoke } from "@tauri-apps/api/core";

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
