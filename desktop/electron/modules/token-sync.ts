/**
 * Sync GitHub OAuth tokens from the desktop token file to the server's
 * credential vault via the MCP JSON-RPC protocol.
 *
 * Ported from src-tauri/src/token_sync.rs
 */

import * as fs from "node:fs";
import * as path from "node:path";
import { homedir } from "node:os";

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/** The credential key the server uses for GitHub App OAuth tokens. */
const GITHUB_APP_OAUTH_DB_KEY = "__OAUTH_GITHUB_APP";

/** Provider ID used when storing the credential. */
const GITHUB_APP_PROVIDER_ID = "github_app";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/**
 * Token payload matching the server's `GitHubAppTokens` struct.
 * (server/crates/djinn-provider/src/oauth/github_app.rs)
 */
interface ServerGitHubAppTokens {
  access_token: string;
  refresh_token: string;
  /** Unix timestamp (seconds) when the access token expires. */
  expires_at: number;
  /** Unix timestamp when the refresh token expires (optional). */
  refresh_token_expires_at: number | null;
  /** GitHub user login. */
  user_login: string | null;
}

// ---------------------------------------------------------------------------
// Resolve server URL
// ---------------------------------------------------------------------------

/**
 * Read `~/.djinn/active_connection.json` for the server base URL,
 * falling back to `~/.djinn/daemon.json` for backward compatibility.
 */
export function resolveServerBaseUrl(): string | null {
  const home = homedir();
  const djinnDir = path.join(home, ".djinn");

  // Prefer active_connection.json — written for all connection modes.
  try {
    const content = fs.readFileSync(
      path.join(djinnDir, "active_connection.json"),
      "utf-8",
    );
    const json = JSON.parse(content);
    if (typeof json.base_url === "string" && json.base_url) {
      return json.base_url;
    }
  } catch {
    // File missing or unreadable — fall through.
  }

  // Fallback: daemon.json (local daemon mode only, backward compat).
  try {
    const content = fs.readFileSync(
      path.join(djinnDir, "daemon.json"),
      "utf-8",
    );
    const json = JSON.parse(content);
    if (typeof json.port === "number") {
      return `http://127.0.0.1:${json.port}`;
    }
  } catch {
    // File missing or unreadable.
  }

  return null;
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/**
 * Push tokens to the server's credential vault via MCP `credential_set`.
 *
 * Best-effort: failures are logged but never propagated because the primary
 * token storage (local file) has already succeeded.
 */
export async function syncTokensToServer(
  accessToken: string,
  refreshToken: string,
  expiresAt: number,
  userLogin?: string,
): Promise<void> {
  const baseUrl = resolveServerBaseUrl();
  if (!baseUrl) {
    console.warn(
      "token_sync: server URL not available, skipping credential sync",
    );
    return;
  }

  const serverTokens: ServerGitHubAppTokens = {
    access_token: accessToken,
    refresh_token: refreshToken,
    expires_at: expiresAt,
    refresh_token_expires_at: null,
    user_login: userLogin ?? null,
  };

  let tokensJson: string;
  try {
    tokensJson = JSON.stringify(serverTokens);
  } catch (err) {
    console.error("token_sync: failed to serialize tokens:", err);
    return;
  }

  try {
    await doSync(baseUrl, tokensJson);
    console.log("token_sync: successfully synced GitHub tokens to server");
  } catch (err) {
    console.error("token_sync: failed to sync tokens to server:", err);
  }
}

// ---------------------------------------------------------------------------
// MCP handshake
// ---------------------------------------------------------------------------

/**
 * Perform the full MCP handshake and call `credential_set`.
 *
 * 1. POST /mcp — initialize
 * 2. Extract mcp-session-id header
 * 3. POST /mcp — notifications/initialized
 * 4. POST /mcp — tools/call credential_set
 * 5. Parse response (JSON or SSE)
 */
async function doSync(baseUrl: string, tokensJson: string): Promise<void> {
  const mcpUrl = `${baseUrl}/mcp`;

  // Step 1: Initialize MCP session
  const initPayload = {
    jsonrpc: "2.0",
    id: 1,
    method: "initialize",
    params: {
      protocolVersion: "2025-06-18",
      capabilities: {},
      clientInfo: {
        name: "djinn-desktop-token-sync",
        version: "0.1.0",
      },
    },
  };

  const initResp = await fetch(mcpUrl, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Accept: "application/json, text/event-stream",
    },
    body: JSON.stringify(initPayload),
  });

  if (!initResp.ok) {
    throw new Error(`MCP initialize returned status ${initResp.status}`);
  }

  const sessionId = initResp.headers.get("mcp-session-id");
  if (!sessionId) {
    throw new Error(
      "Missing mcp-session-id header on initialize response",
    );
  }

  // Consume the body so the connection is released.
  await initResp.text();

  // Step 2: Send notifications/initialized
  const notifyPayload = {
    jsonrpc: "2.0",
    method: "notifications/initialized",
    params: {},
  };

  const notifyResp = await fetch(mcpUrl, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Accept: "application/json, text/event-stream",
      "mcp-session-id": sessionId,
    },
    body: JSON.stringify(notifyPayload),
  });

  // Consume the body.
  await notifyResp.text();

  // Step 3: Call credential_set tool
  const toolPayload = {
    jsonrpc: "2.0",
    id: 2,
    method: "tools/call",
    params: {
      name: "credential_set",
      arguments: {
        provider_id: GITHUB_APP_PROVIDER_ID,
        key_name: GITHUB_APP_OAUTH_DB_KEY,
        api_key: tokensJson,
      },
    },
  };

  const toolResp = await fetch(mcpUrl, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Accept: "application/json, text/event-stream",
      "mcp-session-id": sessionId,
    },
    body: JSON.stringify(toolPayload),
  });

  if (!toolResp.ok) {
    throw new Error(`credential_set returned status ${toolResp.status}`);
  }

  const body = await toolResp.text();

  // Parse the response — may be plain JSON or SSE.
  const result = parseJsonrpcResult(body, 2);

  // Check for tool-level error in the result payload.
  if (
    typeof result === "object" &&
    result !== null &&
    "ok" in result &&
    (result as Record<string, unknown>).ok === false
  ) {
    const errMsg =
      (result as Record<string, unknown>).error ?? "unknown error";
    throw new Error(`credential_set returned ok=false: ${errMsg}`);
  }
}

// ---------------------------------------------------------------------------
// JSON-RPC response parsing
// ---------------------------------------------------------------------------

/**
 * Parse a JSON-RPC 2.0 response that may be either a single JSON object
 * or an SSE stream containing multiple events.
 */
export function parseJsonrpcResult(body: string, id: number): unknown {
  // Try plain JSON first.
  try {
    const single = JSON.parse(body);
    if (single.id === id) {
      if (single.error) {
        const msg = single.error.message ?? "unknown RPC error";
        throw new Error(`JSON-RPC error: ${msg}`);
      }
      return extractToolPayload(single);
    }
  } catch (err) {
    // If it was our own thrown error, re-throw it.
    if (err instanceof Error && err.message.startsWith("JSON-RPC error:")) {
      throw err;
    }
    // Otherwise fall through to SSE parsing.
  }

  // Parse as SSE stream.
  for (const line of body.split("\n")) {
    const data = line.startsWith("data: ") ? line.slice(6) : line;
    try {
      const event = JSON.parse(data);
      if (event.id === id) {
        if (event.error) {
          const msg = event.error.message ?? "unknown RPC error";
          throw new Error(`JSON-RPC error: ${msg}`);
        }
        return extractToolPayload(event);
      }
    } catch (err) {
      // If it was our own thrown error, re-throw it.
      if (
        err instanceof Error &&
        err.message.startsWith("JSON-RPC error:")
      ) {
        throw err;
      }
      // Otherwise skip non-JSON lines.
    }
  }

  throw new Error(`No JSON-RPC response with id=${id} found in body`);
}

/**
 * Extract the tool result payload from a JSON-RPC result envelope.
 *
 * Prefers `structuredContent`, falls back to parsing the first text
 * content item, then returns the raw result.
 */
export function extractToolPayload(envelope: Record<string, unknown>): unknown {
  const result = envelope.result as Record<string, unknown> | undefined;
  if (!result) {
    throw new Error("Missing 'result' in JSON-RPC response");
  }

  // Prefer structuredContent.
  if (result.structuredContent !== undefined) {
    return result.structuredContent;
  }

  // Fall back to parsing the first text content item.
  if (Array.isArray(result.content)) {
    for (const item of result.content) {
      if (
        typeof item === "object" &&
        item !== null &&
        typeof (item as Record<string, unknown>).text === "string"
      ) {
        try {
          return JSON.parse((item as Record<string, unknown>).text as string);
        } catch {
          // Not valid JSON — skip.
        }
      }
    }
  }

  return result;
}
