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
 * By default every endpoint answers the "provisioned, signed-in admin" happy
 * path so pages that only wrap `AuthGate` to read `useAuthUser()` fall
 * straight through to their content. Stories that exercise AuthGate's OWN
 * setup/sign-in branches override individual endpoints with the setters below
 * (again from a decorator, keyed by story id so the tree remounts). Call
 * `resetStoryAuth()` first so a story never inherits the previous story's
 * overrides if Storybook reused the module without a full reload.
 *
 * Unknown URLs fall through to the real fetch, so MCP mocking and
 * error-state stories keep working unchanged.
 */

import type { InstallationSummary } from "@/api/auth";

const DEFAULT_SETUP_STATUS: Record<string, unknown> = {
  needs_app_install: false,
  app_credentials_configured: true,
  org_login: "djinnos",
  setup_state: "valid",
};

const DEFAULT_AUTH_CONFIG: Record<string, unknown> = {
  configured: true,
  missing: [],
  setup_doc_url: "https://www.djinnai.io/docs/setup",
  self_setup_available: false,
};

let storyIsAdmin = true;
/** `undefined` → synthesize the default user (honoring `storyIsAdmin`).
 *  `null` → answer `/auth/me` with 401 (signed-out). An object → return it. */
let authUserOverride: Record<string, unknown> | null | undefined = undefined;
let setupStatusBody: Record<string, unknown> = DEFAULT_SETUP_STATUS;
let setupStatusErrorCode: number | null = null;
let authConfigBody: Record<string, unknown> = DEFAULT_AUTH_CONFIG;
let installationsBody: InstallationSummary[] = [];

/** Flip the admin flag the shim reports on `/auth/me`. Call from a story
 * decorator BEFORE the story (and AuthGate) render. */
export function setStoryIsAdmin(isAdmin: boolean): void {
  storyIsAdmin = isAdmin;
}

/** Override the `/auth/me` response. `null` returns 401 (signed-out); an
 * object returns that user body verbatim. */
export function setStoryAuthUser(user: Record<string, unknown> | null): void {
  authUserOverride = user;
}

/** Override the `/setup/status` body (snake_case, as the server sends it). */
export function setStorySetupStatus(body: Record<string, unknown>): void {
  setupStatusBody = body;
  setupStatusErrorCode = null;
}

/** Make `/setup/status` fail so AuthGate renders its "can't reach server"
 * branch. */
export function setStorySetupStatusError(status = 503): void {
  setupStatusErrorCode = status;
}

/** Override the `/auth/config` body (snake_case, as the server sends it). */
export function setStoryAuthConfig(body: Record<string, unknown>): void {
  authConfigBody = body;
}

/** Rows the InstallationPicker's `/api/github/installations` returns. */
export function setStoryInstallations(list: InstallationSummary[]): void {
  installationsBody = list;
}

/** Restore every endpoint to the default happy path. Call at the top of an
 * AuthGate-branch story's decorator before applying its own overrides. */
export function resetStoryAuth(): void {
  storyIsAdmin = true;
  authUserOverride = undefined;
  setupStatusBody = DEFAULT_SETUP_STATUS;
  setupStatusErrorCode = null;
  authConfigBody = DEFAULT_AUTH_CONFIG;
  installationsBody = [];
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
      if (authUserOverride !== undefined) {
        return Promise.resolve(
          authUserOverride === null
            ? json({ error: "unauthenticated" }, 401)
            : json(authUserOverride),
        );
      }
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
        setupStatusErrorCode !== null
          ? json({ error: "setup status unavailable" }, setupStatusErrorCode)
          : json(setupStatusBody),
      );
    }
    if (url.includes("/auth/config")) {
      return Promise.resolve(json(authConfigBody));
    }
    if (url.includes("/api/github/installations")) {
      return Promise.resolve(
        json(
          installationsBody.map((inst) => ({
            installation_id: inst.installationId,
            account_login: inst.accountLogin,
            account_id: inst.accountId,
            account_type: inst.accountType,
            repository_selection: inst.repositorySelection,
            html_url: inst.htmlUrl,
          })),
        ),
      );
    }
    return realFetch(input, init);
  };
  window.fetch = stub;
}
