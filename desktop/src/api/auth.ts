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
  /**
   * `true` once the server has successfully resolved a GitHub App config
   * from the `GITHUB_APP_*` env vars / mounted `djinn-github-app` Secret.
   */
  configured: boolean;
  /**
   * Names of the env vars the server detected as missing — surfaced so the
   * "App not configured" message can call out the specific gap to operators.
   */
  missing: string[];
  setupDocUrl: string;
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
  };
  return {
    configured: body.configured,
    missing: body.missing,
    setupDocUrl: body.setup_doc_url,
  };
}

export interface SetupStatus {
  /**
   * True when either the GitHub App credentials haven't been provisioned on
   * the server (env / Secret missing) OR no `org_config` row is bound to
   * this deployment. Both surface the same "App not configured" UI.
   */
  needsAppInstall: boolean;
  /**
   * The GitHub org this deployment is locked to, once known. `null` until
   * the App-setup callback writes `org_config`.
   */
  orgLogin: string | null;
}

/**
 * Public, no-auth endpoint used to decide whether to gate the app on the
 * "App not configured" screen vs. the sign-in screen. Mirrors the server's
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
