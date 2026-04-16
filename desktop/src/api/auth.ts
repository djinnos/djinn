/**
 * GitHub-backed authentication client.
 *
 * The server owns the OAuth flow end-to-end: we ask it who we are (GET /auth/me),
 * navigate the browser to its GitHub OAuth kickoff endpoint, and let it set a
 * session cookie via its callback. All requests forward credentials so the
 * session cookie flows both ways.
 */
import { getBaseUrl } from "@/api/serverUrl";

export interface User {
  id: string;
  login: string;
  name: string | null;
  avatarUrl: string | null;
}

interface ServerUser {
  id: string | number;
  login: string;
  name?: string | null;
  avatar_url?: string | null;
}

function mapUser(raw: ServerUser): User {
  return {
    id: String(raw.id),
    login: raw.login,
    name: raw.name ?? null,
    avatarUrl: raw.avatar_url ?? null,
  };
}

/**
 * Fetch the currently authenticated user from the server.
 * Returns null on 401 (unauthenticated); throws on network or server errors.
 */
export async function fetchCurrentUser(): Promise<User | null> {
  const res = await fetch(`${getBaseUrl()}/auth/me`, {
    credentials: "include",
    headers: { Accept: "application/json" },
  });
  if (res.status === 401) return null;
  if (!res.ok) {
    throw new Error(`Failed to fetch current user: ${res.status} ${res.statusText}`);
  }
  const body = (await res.json()) as ServerUser;
  return mapUser(body);
}

function defaultRedirect(): string {
  if (typeof window === "undefined") return "/";
  return `${window.location.pathname}${window.location.search}`;
}

/**
 * Navigate the browser to the server's GitHub OAuth kickoff endpoint.
 * The server will 302 to GitHub; after the callback it redirects back to `redirect`.
 */
export function startGithubLogin(redirect?: string): void {
  const target = redirect ?? defaultRedirect();
  const url = `${getBaseUrl()}/auth/github/start?redirect=${encodeURIComponent(target)}`;
  window.location.assign(url);
}

export interface AuthConfig {
  configured: boolean;
  missing: string[];
  setupDocUrl: string;
  /**
   * Path on the Djinn server (relative — no host) that kicks off the GitHub
   * App manifest auto-provision flow. Always present; the UI only surfaces
   * it as a button when `configured === false`.
   */
  createAppUrl: string | null;
}

/**
 * Report which GitHub App env vars are present on the server, so the sign-in
 * screen can show setup guidance instead of a dead-end button.
 */
export async function fetchAuthConfig(): Promise<AuthConfig> {
  const res = await fetch(`${getBaseUrl()}/auth/config`, {
    credentials: "include",
    headers: { Accept: "application/json" },
  });
  if (!res.ok) {
    throw new Error(`Failed to fetch auth config: ${res.status}`);
  }
  const body = (await res.json()) as {
    configured: boolean;
    missing: string[];
    setup_doc_url: string;
    create_app_url?: string | null;
  };
  return {
    configured: body.configured,
    missing: body.missing,
    setupDocUrl: body.setup_doc_url,
    createAppUrl: body.create_app_url ?? null,
  };
}

export interface SetupStatus {
  /**
   * True when either the GitHub App credentials haven't been provisioned on
   * the server OR no `org_config` row is bound to this deployment. The UI
   * treats these identically: route the operator through the manifest flow.
   */
  needsAppInstall: boolean;
  /**
   * The GitHub org this deployment is locked to, once known. `null` until
   * the manifest callback writes `org_config`.
   */
  orgLogin: string | null;
}

/**
 * Public, no-auth endpoint used to decide whether to gate the app on the
 * manifest-install screen vs. the sign-in screen. Mirrors the server's
 * `GET /setup/status` response (snake_case → camelCase).
 */
export async function fetchSetupStatus(): Promise<SetupStatus> {
  const res = await fetch(`${getBaseUrl()}/setup/status`, {
    credentials: "include",
    headers: { Accept: "application/json" },
  });
  if (!res.ok) {
    throw new Error(`Failed to fetch setup status: ${res.status}`);
  }
  const body = (await res.json()) as {
    needs_app_install: boolean;
    org_login?: string | null;
  };
  return {
    needsAppInstall: body.needs_app_install,
    orgLogin: body.org_login ?? null,
  };
}

/**
 * Navigate the browser to the server's manifest auto-provision endpoint.
 * The server returns an HTML page that auto-submits a form to GitHub.
 *
 * When `organization` is provided, the manifest is POSTed to
 * `github.com/organizations/<org>/settings/apps/new` so the App is created
 * under that org (the caller must be an org owner). Omit it to create under
 * the signed-in user's personal account.
 */
export function startManifestProvision(
  createAppUrl: string,
  organization?: string,
): void {
  const base = `${getBaseUrl()}${createAppUrl}`;
  const org = organization?.trim();
  const url = org
    ? `${base}${base.includes("?") ? "&" : "?"}organization=${encodeURIComponent(org)}`
    : base;
  window.location.assign(url);
}

/**
 * Log out and reload so every query refetches under the new (unauth) session.
 */
export async function logout(): Promise<void> {
  try {
    await fetch(`${getBaseUrl()}/auth/logout`, {
      method: "POST",
      credentials: "include",
    });
  } finally {
    if (typeof window !== "undefined") {
      window.location.reload();
    }
  }
}
