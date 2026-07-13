/**
 * GitHub-backed authentication client.
 *
 * The server owns the OAuth flow end-to-end: we ask it who we are (GET /auth/me),
 * navigate the browser to its GitHub OAuth kickoff endpoint, and let it set a
 * session cookie via its callback. All requests forward credentials so the
 * session cookie flows both ways.
 */
import { getBaseUrl } from "@/api/serverUrl";
import { queryClient } from "@/lib/queryClient";

export interface User {
  id: string;
  login: string;
  name: string | null;
  avatarUrl: string | null;
  isAdmin: boolean;
  /** Proposal capability role: `proposer` | `pm` | `engineer`. */
  role: string;
}

interface ServerUser {
  id: string | number;
  login: string;
  name?: string | null;
  avatar_url?: string | null;
  is_admin?: boolean;
  role?: string;
}

function mapUser(raw: ServerUser): User {
  return {
    id: String(raw.id),
    login: raw.login,
    name: raw.name ?? null,
    avatarUrl: raw.avatar_url ?? null,
    isAdmin: raw.is_admin ?? false,
    role: raw.role ?? "proposer",
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
  /**
   * Whether the self-setup manifest flow is available for the operator to
   * create a new GitHub App without manually providing credentials. Only
   * `true` when `DJINN_ENABLE_SELF_SETUP=true` AND no usable credentials
   * exist yet. Defaults to `false` for older servers that don't send
   * `self_setup_available`.
   */
  selfSetupAvailable: boolean;
  /**
   * Whether this response established the server-owned launch capability
   * required by `POST /auth/github/setup-start`. This is intentionally
   * separate from `selfSetupAvailable`: self-setup may be enabled while the
   * current origin is not allowed to start it. Defaults to `false` for older
   * servers so the UI never renders a button that cannot work securely.
   */
  setupLaunchAvailable: boolean;
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
    self_setup_available?: boolean;
    setup_launch_available?: boolean;
  };
  return {
    configured: body.configured,
    missing: body.missing,
    setupDocUrl: body.setup_doc_url,
    // Default to `false` so older servers (pre-self-setup) don't advertise
    // a setup flow that doesn't exist.
    selfSetupAvailable: body.self_setup_available ?? false,
    // Launching is opt-in. Older servers and non-canonical origins must fall
    // back to the one-time URL instead of rendering a dead or unsafe CTA.
    setupLaunchAvailable: body.setup_launch_available ?? false,
  };
}

/**
 * Source from which the server resolved the active GitHub App credentials.
 * Mirrors the server's `CredentialSourceState` when usable, or `null` when
 * the credentials are not in a usable state (unconfigured / invalid / etc.).
 *
 * - `"secret"` — valid credentials loaded from env / Kubernetes Secret.
 * - `"persisted"` — valid credentials loaded from the encrypted store.
 * - `null` — no usable credentials (unconfigured, invalid Secret, etc.).
 */
export type CredentialSource = "secret" | "persisted" | null;

/**
 * High-level setup lifecycle state the server can report alongside the
 * credential-source fields. Mirrors the server's credential resolution
 * outcome.
 */
export type SetupState =
  | "unconfigured"
  | "valid"
  | "invalid_secret"
  | "unrecoverable"
  | null;

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

  // ─── Credential-source & recovery fields (vpr6 foundation) ───────────────
  //
  // These fields are optional on the server response and may be absent on
  // older builds. They surface the credential-source state machine so
  // AuthGate can distinguish "invalid Secret (fatal)" and "undecryptable
  // persisted credentials (recovery needed)" from the plain unconfigured
  // state. All default to safe/no-op values for backwards compatibility.

  /**
   * Which source produced the usable GitHub App credentials, if any.
   * `"secret"` for env/Secret, `"persisted"` for the encrypted store,
   * `null` when no usable credentials exist or the server doesn't report
   * this field.
   */
  credentialSource: CredentialSource;
  /**
   * Normalized setup/credential state derived from the server's
   * `credential_source` / `setup_state` fields. `"unconfigured"` when no
   * credentials exist, `"valid"` when usable, `"invalid_secret"` for a
   * fatal Secret error, `"unrecoverable"` for undecryptable persisted
   * credentials. `null` when the server doesn't report this field (older
   * builds).
   */
  setupState: SetupState;
  /**
   * Human-readable error message from the server when the credential
   * state is fatal or needs recovery. `null` when there's no error.
   */
  setupError: string | null;
  /**
   * Whether a failed setup can be retried. When `false`, the operator
   * must take manual action (fix env vars, re-provision). Defaults to
   * `false`.
   */
  setupRetryable: boolean;
  /**
   * Whether the persisted credentials are present but undecryptable (e.g.
   * wrong encryption key, corrupt data). This is a recovery-required
   * state — the operator must re-provision. Defaults to `false`.
   */
  credentialsUnrecoverable: boolean;
}

/**
 * Normalize the server's `credential_source` field into a typed
 * `CredentialSource`. Returns `null` for unknown / missing values so the
 * UI never crashes on an unexpected server response.
 */
function normalizeCredentialSource(raw: unknown): CredentialSource {
  if (raw === "secret") return "secret";
  if (raw === "persisted") return "persisted";
  return null;
}

/**
 * Normalize the server's `setup_state` field into a typed `SetupState`.
 * Accepts both the explicit `setup_state` string and an inferred value
 * from `credential_source` / `credentials_unrecoverable` when the server
 * only sends those. Returns `null` when no state information is present.
 */
function normalizeSetupState(
  setupStateRaw: unknown,
  credentialSourceRaw: unknown,
  credentialsUnrecoverable: boolean,
): SetupState {
  if (typeof setupStateRaw === "string" && setupStateRaw) {
    switch (setupStateRaw) {
      case "unconfigured":
        return "unconfigured";
      case "valid":
      case "valid_secret":
      case "valid_persisted":
        return "valid";
      case "invalid_secret":
      case "invalid":
      case "fatal":
        return "invalid_secret";
      case "unrecoverable":
      case "undecryptable":
      case "credentials_unrecoverable":
        return "unrecoverable";
      default:
        // Unknown but non-empty state string — fall through to inference.
        break;
    }
  }

  // Infer from the credential source / unrecoverable flag when the server
  // doesn't send an explicit `setup_state`.
  if (credentialsUnrecoverable) return "unrecoverable";
  const src = normalizeCredentialSource(credentialSourceRaw);
  if (src !== null) return "valid";
  // If we have no credential source and no explicit state, return null so
  // the UI treats it as "no information" (preserves legacy behavior).
  return null;
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
    credential_source?: string | null;
    setup_state?: string;
    setup_error?: string | null;
    setup_retryable?: boolean;
    credentials_unrecoverable?: boolean;
  };
  const credentialsUnrecoverable = body.credentials_unrecoverable ?? false;
  const credentialSource = normalizeCredentialSource(body.credential_source);
  return {
    needsAppInstall: body.needs_app_install,
    // Default to `false` so an older server (pre-this-PR) is treated as
    // "operator must fix" — the UI's existing static screen still works.
    appCredentialsConfigured: body.app_credentials_configured ?? false,
    orgLogin: body.org_login ?? null,
    credentialSource,
    setupState: normalizeSetupState(
      body.setup_state,
      body.credential_source,
      credentialsUnrecoverable,
    ),
    setupError: body.setup_error ?? null,
    setupRetryable: body.setup_retryable ?? false,
    credentialsUnrecoverable,
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
 * Log out and bounce to the app root under a fresh (unauth) state.
 *
 * Three layers, because cookie-clearing is surprisingly fragile:
 *  1. POST /auth/logout — server-side row delete + `Set-Cookie` with
 *     `Max-Age=0` and a far-past `Expires`.
 *  2. queryClient.clear() — drops every in-memory query, including the
 *     cached `["auth", "me"]` user that AuthGate keys off. Without this
 *     a soft reload paths sometimes pick up the cached user before the
 *     refetch lands.
 *  3. window.location.assign("/") — full navigation, not a reload, so
 *     the React tree is freshly mounted with no stale in-memory state
 *     (zustand stores, route history, etc.).
 */
export async function logout(): Promise<void> {
  try {
    await fetch(`${getBaseUrl()}/auth/logout`, {
      method: "POST",
      credentials: "include",
    });
  } catch (err) {
    console.warn("logout: request failed; continuing to clear local state", err);
  } finally {
    try {
      queryClient.clear();
    } catch (err) {
      console.warn("logout: queryClient.clear failed", err);
    }
    if (typeof window !== "undefined") {
      window.location.assign("/");
    }
  }
}
