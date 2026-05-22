/**
 * Resolve the base URL for the Djinn server.
 *
 * The UI ships embedded inside the `djinn-server` binary (rust-embed) and is
 * served by the same process that hosts the API, so the default is to issue
 * requests against the same origin as the page — empty string makes `fetch`
 * resolve against `window.location.origin` automatically. This means a customer
 * deployment behind any ingress/ALB/custom domain works with zero rebuild.
 *
 * `VITE_DJINN_SERVER_URL` overrides this only when the UI is served from a
 * different origin than the API — e.g. running `pnpm dev` on `:1420` against a
 * server on `:8372`. Make sure the override's host matches the server's
 * `DJINN_PUBLIC_URL`; `localhost` and `127.0.0.1` are distinct cookie origins
 * and mixing them breaks the OAuth state-cookie round-trip.
 */
function stripTrailingSlash(url: string): string {
  return url.endsWith("/") ? url.slice(0, -1) : url;
}

export function getServerBaseUrl(): string {
  const envUrl = import.meta.env?.VITE_DJINN_SERVER_URL as string | undefined;
  if (envUrl && envUrl.length > 0) {
    return stripTrailingSlash(envUrl);
  }
  return "";
}

export const getBaseUrl = getServerBaseUrl;
