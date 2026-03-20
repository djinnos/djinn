/**
 * Tauri command wrappers
 * 
 * All invoke() calls go through this module. Never call invoke() directly
 * in components - always use these wrapper functions.
 */

import { invoke } from "@tauri-apps/api/core";

// Server commands
export async function getServerPort(): Promise<number> {
  return invoke("get_server_port");
}

// Auth commands
export async function getAuthToken(): Promise<string | null> {
  return invoke("get_auth_token");
}

export async function setAuthToken(token: string): Promise<void> {
  return invoke("set_auth_token", { token });
}

export async function clearAuthToken(): Promise<void> {
  return invoke("clear_auth_token");
}

// Test commands
export async function greet(name: string): Promise<string> {
  return invoke("greet", { name });
}
