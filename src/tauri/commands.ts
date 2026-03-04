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
