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

/**
 * Navigate the browser to the server's manifest auto-provision endpoint.
 * The server returns an HTML page that auto-submits a form to GitHub.
 */
export function startManifestProvision(createAppUrl: string): void {
  window.location.assign(`${getBaseUrl()}${createAppUrl}`);
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
