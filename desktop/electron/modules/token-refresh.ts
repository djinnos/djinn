/**
 * Silent token refresh with rotation serialization.
 *
 * Implements:
 * - Promise-chain mutex for serializing concurrent refresh calls
 * - 30-second expiry buffer before refreshing
 * - Token rotation handling (GitHub returns new refresh token)
 * - Automatic cleanup on refresh failure
 *
 * Automatic OAuth token refresh
 */

import {
  type StoredTokens,
  type TokenResponse,
  clearToken,
  refreshGithubToken,
  retrieveToken,
  storeToken,
} from "./auth.js";
import { syncTokensToServer } from "./token-sync.js";

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/** Refresh tokens 30 seconds before actual expiry. */
const EXPIRY_BUFFER_SECONDS = 30;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/** Current authentication state with expiry tracking. */
export interface TokenState {
  accessToken: string;
  refreshToken: string;
  /** Unix timestamp (seconds) when the token expires. */
  expiresAt: number;
  tokenType?: string;
  scope?: string;
  userId?: string;
}

export type RefreshResult =
  | { kind: "success"; state: TokenState }
  | { kind: "no-token" }
  | { kind: "failed"; reason: string };

// ---------------------------------------------------------------------------
// Global state
// ---------------------------------------------------------------------------

let currentTokenState: TokenState | null = null;

// ---------------------------------------------------------------------------
// Async mutex via promise chain
// ---------------------------------------------------------------------------

let mutexChain: Promise<void> = Promise.resolve();

/**
 * Run `fn` while holding the async mutex. Concurrent callers will queue
 * behind the current holder.
 */
function withRefreshMutex<T>(fn: () => Promise<T>): Promise<T> {
  let resolve!: (value: T) => void;
  let reject!: (err: unknown) => void;
  const result = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });

  mutexChain = mutexChain.then(async () => {
    try {
      resolve(await fn());
    } catch (err) {
      reject(err);
    }
  });

  return result;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function nowUnix(): number {
  return Math.floor(Date.now() / 1000);
}

// ---------------------------------------------------------------------------
// Public API — token state accessors
// ---------------------------------------------------------------------------

export function getTokenState(): TokenState | null {
  return currentTokenState;
}

export function setTokenState(state: TokenState): void {
  currentTokenState = state;
}

export function clearTokenState(): void {
  currentTokenState = null;
}

/** Returns true when no token exists or the token is within 30s of expiry. */
export function isTokenExpiredOrStale(): boolean {
  if (!currentTokenState) return true;
  return nowUnix() + EXPIRY_BUFFER_SECONDS >= currentTokenState.expiresAt;
}

/** Return the access token if it is still valid (not within 30s of expiry). */
export function getValidAccessToken(): string | null {
  if (!currentTokenState) return null;
  if (nowUnix() + EXPIRY_BUFFER_SECONDS < currentTokenState.expiresAt) {
    return currentTokenState.accessToken;
  }
  return null;
}

// ---------------------------------------------------------------------------
// Core refresh logic
// ---------------------------------------------------------------------------

/**
 * Perform a silent token refresh using the stored refresh token.
 *
 * Serialized via an async mutex to prevent concurrent refresh race conditions
 * and token-rotation issues.
 */
export function performSilentRefresh(): Promise<RefreshResult> {
  return withRefreshMutex(async (): Promise<RefreshResult> => {
    // Double-check after acquiring lock — another caller may have refreshed.
    if (getValidAccessToken() && currentTokenState) {
      return { kind: "success", state: { ...currentTokenState } };
    }

    // Retrieve stored token blob.
    let storedJson: string | null;
    try {
      storedJson = await retrieveToken();
    } catch (err) {
      return { kind: "failed", reason: `Failed to retrieve token: ${err}` };
    }

    if (!storedJson) {
      return { kind: "no-token" };
    }

    let stored: StoredTokens;
    try {
      stored = JSON.parse(storedJson) as StoredTokens;
    } catch (err) {
      // Corrupt stored data — wipe it.
      await clearToken().catch(() => {});
      clearTokenState();
      return { kind: "failed", reason: `Invalid stored token format: ${err}` };
    }

    // GitHub OAuth App tokens don't have refresh tokens — they never expire.
    // If no refresh token is stored, restore session from stored access token.
    if (!stored.refresh_token) {
      const state: TokenState = {
        accessToken: stored.access_token,
        refreshToken: "",
        // OAuth App tokens don't expire — use a far-future timestamp (1 year).
        expiresAt: nowUnix() + 365 * 24 * 3600,
      };
      setTokenState(state);
      return { kind: "success", state: { ...state } };
    }

    // Call GitHub token endpoint.
    let tokenResponse: TokenResponse;
    try {
      tokenResponse = await refreshGithubToken(stored.refresh_token);
    } catch (err) {
      await clearToken().catch(() => {});
      clearTokenState();
      return { kind: "failed", reason: `Network error: ${err}` };
    }

    // Calculate expiry as unix timestamp.
    const expiresIn = tokenResponse.expires_in ?? 28800;
    const expiresAtUnix = nowUnix() + expiresIn;

    // Handle token rotation: GitHub may return a new refresh token.
    const newRefreshToken =
      tokenResponse.refresh_token ?? stored.refresh_token;

    // Update stored tokens.
    const updatedStored: StoredTokens = {
      access_token: tokenResponse.access_token,
      refresh_token: newRefreshToken,
      expires_at: expiresAtUnix,
      user_login: stored.user_login,
      avatar_url: stored.avatar_url,
    };

    try {
      await storeToken(JSON.stringify(updatedStored));
    } catch (err) {
      // Continue anyway — we have a valid access token for now.
      console.error("Failed to store refreshed tokens:", err);
    }

    // Update in-memory state.
    const tokenState: TokenState = {
      accessToken: tokenResponse.access_token,
      refreshToken: newRefreshToken,
      expiresAt: expiresAtUnix,
    };
    setTokenState(tokenState);

    // Sync refreshed tokens to server credential vault.
    await syncTokensToServer(
      tokenState.accessToken,
      tokenState.refreshToken,
      expiresAtUnix,
      updatedStored.user_login || undefined,
    );

    return { kind: "success", state: { ...tokenState } };
  });
}

/**
 * Check for stored refresh token on startup and attempt silent refresh.
 *
 * Call during app initialization to restore the user's session without
 * requiring re-authentication.
 */
export async function attemptSilentAuthOnStartup(): Promise<RefreshResult> {
  let storedJson: string | null;
  try {
    storedJson = await retrieveToken();
  } catch (err) {
    return { kind: "failed", reason: `Storage error: ${err}` };
  }

  if (!storedJson) {
    return { kind: "no-token" };
  }

  return performSilentRefresh();
}

/**
 * Clear all authentication state (logout).
 *
 * Acquires the refresh mutex to prevent a concurrent refresh from
 * re-populating state after we clear it.
 */
export function logout(): Promise<void> {
  return withRefreshMutex(async () => {
    clearTokenState();
    await clearToken();
  });
}
