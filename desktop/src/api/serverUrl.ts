/**
 * Server URL resolution.
 *
 * The Djinn server runs via docker-compose on localhost:8372 by default.
 * The URL can be overridden at build time via VITE_DJINN_SERVER_URL for
 * pointing the Electron shell at a different host (e.g. a remote dev server).
 */

const DEFAULT_SERVER_URL = "http://127.0.0.1:8372";

function stripTrailingSlash(url: string): string {
  return url.replace(/\/+$/, "");
}

function readViteEnvOverride(): string | null {
  try {
    const env = (import.meta as unknown as { env?: Record<string, string | undefined> }).env;
    const override = env?.VITE_DJINN_SERVER_URL;
    if (typeof override === "string" && override.length > 0) {
      return stripTrailingSlash(override);
    }
  } catch {
    // import.meta.env not available (e.g. Jest) — fall through.
  }
  return null;
}

/** Synchronous base URL resolution — safe to call from any context. */
export function getServerBaseUrl(): string {
  return readViteEnvOverride() ?? DEFAULT_SERVER_URL;
}

/** Convenience helper: extract the port portion of the configured URL. */
export function getServerPort(): number {
  try {
    const url = new URL(getServerBaseUrl());
    const port = url.port ? Number(url.port) : url.protocol === "https:" ? 443 : 80;
    return port || 8372;
  } catch {
    return 8372;
  }
}
