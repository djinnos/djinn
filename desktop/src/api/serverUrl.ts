/**
 * Server URL configuration for the web client.
 *
 * The Djinn server runs in Docker on the host at 127.0.0.1:8372.
 * Override with `VITE_DJINN_SERVER_URL` at build/dev time if needed.
 */

const DEFAULT_SERVER_URL = "http://127.0.0.1:8372";

export function getServerBaseUrl(): string {
  const envUrl = (import.meta.env?.VITE_DJINN_SERVER_URL as string | undefined)?.trim();
  const fromEnv = envUrl && envUrl.length > 0 ? envUrl : DEFAULT_SERVER_URL;
  return fromEnv.replace(/\/+$/, "");
}

/**
 * Return the default server port — kept for backwards compatibility
 * with call-sites that previously fetched a dynamic port from Electron.
 */
export async function getServerPort(): Promise<number> {
  const url = new URL(getServerBaseUrl());
  return Number(url.port) || (url.protocol === "https:" ? 443 : 80);
}

/** Return the full server base URL (async wrapper for call-site compat). */
export async function getServerUrl(): Promise<string> {
  return getServerBaseUrl();
}
