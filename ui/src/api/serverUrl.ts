/**
 * Resolve the base URL for the Djinn server.
 *
 * The client is a pure browser SPA — the server base URL is baked in at build
 * time via `VITE_DJINN_SERVER_URL`, and falls back to the default local daemon
 * port (3000) for dev.
 */
// Use `localhost` rather than `127.0.0.1` so the cookie origin matches the
// `DJINN_PUBLIC_URL` the server hands to GitHub OAuth (also `localhost:3000`).
// `localhost` and `127.0.0.1` are distinct cookie origins — mixing them
// breaks the OAuth state-cookie round-trip.
const DEFAULT_SERVER_URL = "http://localhost:3000";

function stripTrailingSlash(url: string): string {
  return url.endsWith("/") ? url.slice(0, -1) : url;
}

export function getServerBaseUrl(): string {
  const envUrl = import.meta.env?.VITE_DJINN_SERVER_URL as string | undefined;
  return stripTrailingSlash(envUrl && envUrl.length > 0 ? envUrl : DEFAULT_SERVER_URL);
}

export const getBaseUrl = getServerBaseUrl;
