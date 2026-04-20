/**
 * Resolve the base URL for the Djinn server.
 *
 * The client is a pure browser SPA — the server base URL is baked in at build
 * time via `VITE_DJINN_SERVER_URL`, and falls back to the default local daemon
 * port (3000) for dev.
 */
const DEFAULT_SERVER_URL = "http://127.0.0.1:3000";

function stripTrailingSlash(url: string): string {
  return url.endsWith("/") ? url.slice(0, -1) : url;
}

export function getServerBaseUrl(): string {
  const envUrl = import.meta.env?.VITE_DJINN_SERVER_URL as string | undefined;
  return stripTrailingSlash(envUrl && envUrl.length > 0 ? envUrl : DEFAULT_SERVER_URL);
}

export const getBaseUrl = getServerBaseUrl;
