/**
 * GitHub OAuth device code flow, token storage, and user profile fetching.
 *
 * GitHub OAuth authentication flow
 */

import * as fs from "node:fs";
import * as fsp from "node:fs/promises";
import * as path from "node:path";
import { homedir } from "node:os";

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

export const GITHUB_CLIENT_ID = "Ov23liBIL080Vt6WJs69";
export const DEVICE_CODE_URL = "https://github.com/login/device/code";
export const ACCESS_TOKEN_URL = "https://github.com/login/oauth/access_token";
export const GITHUB_API_URL = "https://api.github.com";
export const GITHUB_SCOPES = "repo read:org user:email workflow";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/** Response from GitHub device code endpoint. */
export interface DeviceCodeResponse {
  deviceCode: string;
  userCode: string;
  verificationUri: string;
  expiresIn: number;
  interval: number;
}

/** Response from GitHub access token endpoint. */
export interface TokenResponse {
  access_token: string;
  token_type: string;
  scope: string;
  refresh_token?: string;
  refresh_token_expires_in?: number;
  expires_in?: number;
}

/** GitHub user profile from /user endpoint. */
export interface GitHubUser {
  login: string;
  id: number;
  avatar_url: string;
  name?: string;
  email?: string;
}

/** Stored token blob persisted to ~/.djinn/auth_token.json. */
export interface StoredTokens {
  access_token: string;
  refresh_token: string;
  expires_at: number;
  user_login: string;
  avatar_url: string;
}

/** User profile surfaced to the frontend. */
export interface UserProfile {
  sub: string;
  name?: string;
  email?: string;
  picture?: string;
}

/** In-memory auth session (mirrors the Rust AUTH_SESSION). */
export interface AuthSession {
  accessToken: string;
  userProfile?: UserProfile;
}

// ---------------------------------------------------------------------------
// In-memory session state
// ---------------------------------------------------------------------------

let currentAuthSession: AuthSession | null = null;

export function getAuthSession(): AuthSession | null {
  return currentAuthSession;
}

export function setAuthSession(session: AuthSession | null): void {
  currentAuthSession = session;
}

// ---------------------------------------------------------------------------
// Djinn directory & token file helpers
// ---------------------------------------------------------------------------

/** Returns the ~/.djinn/ directory path, creating it if needed. */
export function getDjinnDir(): string {
  const dir = path.join(homedir(), ".djinn");
  fs.mkdirSync(dir, { recursive: true });
  return dir;
}

function tokenFilePath(): string {
  return path.join(getDjinnDir(), "auth_token.json");
}

// ---------------------------------------------------------------------------
// File-based token storage
// ---------------------------------------------------------------------------

/** Write token JSON to ~/.djinn/auth_token.json with mode 0o600. */
export async function storeToken(tokenJson: string): Promise<void> {
  const p = tokenFilePath();
  await fsp.writeFile(p, tokenJson, { mode: 0o600 });
}

/**
 * Read token file. Returns null if the file is missing or empty.
 */
export async function retrieveToken(): Promise<string | null> {
  const p = tokenFilePath();
  try {
    const contents = await fsp.readFile(p, "utf-8");
    return contents.length > 0 ? contents : null;
  } catch (err: unknown) {
    if ((err as NodeJS.ErrnoException).code === "ENOENT") {
      return null;
    }
    throw new Error(`Failed to read token file: ${err}`);
  }
}

/** Delete token file. No-op if already absent. */
export async function clearToken(): Promise<void> {
  const p = tokenFilePath();
  try {
    await fsp.unlink(p);
  } catch (err: unknown) {
    if ((err as NodeJS.ErrnoException).code === "ENOENT") {
      return;
    }
    throw new Error(`Failed to remove token file: ${err}`);
  }
}

// ---------------------------------------------------------------------------
// GitHub OAuth helpers (error response shape)
// ---------------------------------------------------------------------------

interface GitHubErrorResponse {
  error: string;
  error_description?: string;
}

function isGitHubError(obj: unknown): obj is GitHubErrorResponse {
  return typeof obj === "object" && obj !== null && "error" in obj;
}

// ---------------------------------------------------------------------------
// Device flow
// ---------------------------------------------------------------------------

/** Start the GitHub device code flow. */
export async function startDeviceFlow(): Promise<DeviceCodeResponse> {
  const body = new URLSearchParams({
    client_id: GITHUB_CLIENT_ID,
    scope: GITHUB_SCOPES,
  });

  const resp = await fetch(DEVICE_CODE_URL, {
    method: "POST",
    headers: { Accept: "application/json" },
    body,
  });

  if (!resp.ok) {
    const text = await resp.text().catch(() => "<unreadable>");
    throw new Error(`Device code endpoint returned ${resp.status}: ${text}`);
  }

  const json = (await resp.json()) as Record<string, unknown>;

  return {
    deviceCode: json.device_code as string,
    userCode: json.user_code as string,
    verificationUri: json.verification_uri as string,
    expiresIn: json.expires_in as number,
    interval: json.interval as number,
  };
}

/**
 * Poll GitHub for device flow authorization.
 *
 * Handles authorization_pending (retry), slow_down (+5s), expired_token and
 * access_denied (throw).
 */
export async function pollDeviceFlow(
  deviceCode: string,
  interval: number,
): Promise<TokenResponse> {
  let pollInterval = Math.max(interval, 5) * 1000; // ms

  // eslint-disable-next-line no-constant-condition
  while (true) {
    await sleep(pollInterval);

    const body = new URLSearchParams({
      client_id: GITHUB_CLIENT_ID,
      device_code: deviceCode,
      grant_type: "urn:ietf:params:oauth:grant-type:device_code",
    });

    const resp = await fetch(ACCESS_TOKEN_URL, {
      method: "POST",
      headers: { Accept: "application/json" },
      body,
    });

    if (!resp.ok) {
      const text = await resp.text().catch(() => "<unreadable>");
      throw new Error(`Token endpoint returned ${resp.status}: ${text}`);
    }

    const text = await resp.text();
    let parsed: unknown;
    try {
      parsed = JSON.parse(text);
    } catch {
      throw new Error(`Failed to parse token response: ${text}`);
    }

    // Check for error response first
    if (isGitHubError(parsed)) {
      switch (parsed.error) {
        case "authorization_pending":
          continue;
        case "slow_down":
          // GitHub asks us to slow down; add 5 seconds
          pollInterval += 5000;
          continue;
        case "expired_token":
          throw new Error("Device code expired. Please try again.");
        case "access_denied":
          throw new Error("Authorization was denied by the user.");
        default:
          // If the response also has access_token, it's a successful response
          // with extra fields — not an error. Otherwise, throw.
          if (
            typeof parsed === "object" &&
            parsed !== null &&
            "access_token" in parsed
          ) {
            return parsed as unknown as TokenResponse;
          }
          throw new Error(
            `OAuth error: ${parsed.error} - ${parsed.error_description ?? ""}`,
          );
      }
    }

    return parsed as TokenResponse;
  }
}

// ---------------------------------------------------------------------------
// Token refresh
// ---------------------------------------------------------------------------

/** Refresh an expired GitHub token using a refresh_token grant. */
export async function refreshGithubToken(
  refreshToken: string,
): Promise<TokenResponse> {
  const body = new URLSearchParams({
    client_id: GITHUB_CLIENT_ID,
    grant_type: "refresh_token",
    refresh_token: refreshToken,
  });

  const resp = await fetch(ACCESS_TOKEN_URL, {
    method: "POST",
    headers: { Accept: "application/json" },
    body,
  });

  if (!resp.ok) {
    const text = await resp.text().catch(() => "<unreadable>");
    throw new Error(`Token refresh returned ${resp.status}: ${text}`);
  }

  return (await resp.json()) as TokenResponse;
}

// ---------------------------------------------------------------------------
// GitHub user profile
// ---------------------------------------------------------------------------

/** Fetch the authenticated GitHub user's profile. */
export async function fetchGithubUser(accessToken: string): Promise<GitHubUser> {
  const resp = await fetch(`${GITHUB_API_URL}/user`, {
    headers: {
      Authorization: `Bearer ${accessToken}`,
      "User-Agent": "djinnos-desktop",
      Accept: "application/vnd.github+json",
    },
  });

  if (!resp.ok) {
    const text = await resp.text().catch(() => "<unreadable>");
    throw new Error(`GitHub /user returned ${resp.status}: ${text}`);
  }

  return (await resp.json()) as GitHubUser;
}

// ---------------------------------------------------------------------------
// Utility
// ---------------------------------------------------------------------------

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
