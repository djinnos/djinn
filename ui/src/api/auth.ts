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
   * this deployment. Combined with `appCredentialsConfigured` to decide
   * which screen the gate renders:
   *   - `appCredentialsConfigured=false`         → "App not configured" (operator action).
   *   - `appCredentialsConfigured=true && needsAppInstall=true` → installation picker.
   *   - `needsAppInstall=false`                  → normal sign-in.
   */
  needsAppInstall: boolean;
  /**
   * True iff the GitHub App credentials (env / Secret) were resolved on
   * server startup. When `false`, the operator hasn't dropped the
   * `djinn-github-app` Secret yet and the UI can't recover automatically.
   */
  appCredentialsConfigured: boolean;
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
    app_credentials_configured?: boolean;
    org_login?: string | null;
  };
  return {
    needsAppInstall: body.needs_app_install,
    // Default to `false` so an older server (pre-this-PR) is treated as
    // "operator must fix" — the UI's existing static screen still works.
    appCredentialsConfigured: body.app_credentials_configured ?? false,
    orgLogin: body.org_login ?? null,
  };
}

// ─── Installation picker ──────────────────────────────────────────────────────

/**
 * One row in the installation picker. Mirrors the server's
 * `InstallationSummary` JSON shape from `GET /api/github/installations`.
 */
export interface InstallationSummary {
  installationId: number;
  accountLogin: string;
  accountId: number;
  /** "User" or "Organization" — surfaced as a hint next to the row. */
  accountType: string;
  /** "all" or "selected" — used to render the repo-scope hint. */
  repositorySelection: "all" | "selected" | string;
  /** Direct link to the installation's settings page on github.com. */
  htmlUrl: string;
}

/**
 * Fetch the list of GitHub App installations the operator can choose from.
 * Server returns `503 SERVICE_UNAVAILABLE` if App credentials are missing.
 */
export async function fetchInstallations(): Promise<InstallationSummary[]> {
  const res = await fetch(`${getBaseUrl()}/api/github/installations`, {
    credentials: "include",
    headers: { Accept: "application/json" },
  });
  if (!res.ok) {
    const text = await res.text().catch(() => "");
    throw new Error(
      `Failed to fetch installations: ${res.status} ${res.statusText}${text ? ` — ${text}` : ""}`,
    );
  }
  const raw = (await res.json()) as Array<{
    installation_id: number;
    account_login: string;
    account_id: number;
    account_type: string;
    repository_selection: string;
    html_url: string;
  }>;
  return raw.map((r) => ({
    installationId: r.installation_id,
    accountLogin: r.account_login,
    accountId: r.account_id,
    accountType: r.account_type,
    repositorySelection: r.repository_selection,
    htmlUrl: r.html_url,
  }));
}

/**
 * Bind the deployment to a chosen installation. Returns the installation id
 * + account login that the server actually persisted, so the caller can
 * render a confirmation toast without re-fetching.
 */
export async function selectInstallation(
  installationId: number,
): Promise<{ installationId: number; accountLogin: string }> {
  const res = await fetch(
    `${getBaseUrl()}/api/github/installations/select`,
    {
      method: "POST",
      credentials: "include",
      headers: {
        Accept: "application/json",
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ installation_id: installationId }),
    },
  );
  if (!res.ok) {
    const text = await res.text().catch(() => "");
    throw new Error(
      `Failed to bind installation: ${res.status} ${res.statusText}${text ? ` — ${text}` : ""}`,
    );
  }
  const body = (await res.json()) as {
    installation_id: number;
    account_login: string;
  };
  return {
    installationId: body.installation_id,
    accountLogin: body.account_login,
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
