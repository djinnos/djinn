/**
 * Shared Storybook auth fetch stub.
 *
 * Several stories wrap their page in the real `AuthGate`, which resolves the
 * caller through `/auth/me` (+ `/setup/status`, `/auth/config`, `/health`).
 * Storybook bundles EVERY story module into one preview, so per-file
 * `window.fetch` shims chain — and whichever installed last answers
 * `/auth/me` with ITS module's admin flag, regardless of which story is on
 * screen. This module is the single source of truth instead: it installs one
 * shim at first import, and stories flip the flag through
 * `setStoryIsAdmin(...)` in a decorator (before AuthGate mounts).
 *
 * Unknown URLs fall through to the real fetch, so MCP mocking and
 * error-state stories keep working unchanged.
 */

let storyIsAdmin = true;

/** Flip the admin flag the shim reports on `/auth/me`. Call from a story
 * decorator BEFORE the story (and AuthGate) render. */
export function setStoryIsAdmin(isAdmin: boolean): void {
  storyIsAdmin = isAdmin;
}

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

let installed = false;

/** Install the shim once. Safe to call from every story module. */
export function installAuthFetchStub(): void {
  if (installed) return;
  installed = true;
  const realFetch = window.fetch.bind(window);
  const stub: typeof window.fetch = (input, init) => {
    const url =
      typeof input === "string"
        ? input
        : input instanceof URL
          ? input.href
          : input.url;
    if (url.endsWith("/health")) {
      return Promise.resolve(json({ status: "ok", version: "storybook" }));
    }
    if (url.includes("/auth/me")) {
      return Promise.resolve(
        json({
          id: "u-fernando",
          login: "fernando",
          name: "Fernando Bandeira",
          avatar_url: null,
          is_admin: storyIsAdmin,
          role: "engineer",
        }),
      );
    }
    if (url.includes("/setup/status")) {
      return Promise.resolve(
        json({
          needs_app_install: false,
          app_credentials_configured: true,
          org_login: "djinnos",
          setup_state: "valid",
        }),
      );
    }
    if (url.includes("/auth/config")) {
      return Promise.resolve(
        json({
          configured: true,
          missing: [],
          setup_doc_url: "https://www.djinnai.io/docs/setup",
          self_setup_available: false,
        }),
      );
    }
    return realFetch(input, init);
  };
  window.fetch = stub;
}
