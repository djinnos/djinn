import { callMcpTool } from "@/api/mcpClient";
import type {
  ProviderModelsConnectedOutputSchema,
  ProviderConnectedOutputSchema,
  ProviderCatalogOutputSchema,
} from "@/api/generated/mcp-tools.gen";

/**
 * Admin-side helpers for configuring another user on their behalf — any
 * human or non-human user an admin manages.
 *
 * Every call threads the target user's id through the admin-gated
 * `target_user_id` arg so the server reads/writes THAT user's per-user config
 * instead of the acting admin's. These helpers deliberately bypass the global
 * `useProviders`/`settingsStore` singletons (which are scoped to the current
 * user) — the UserConfigDialog component keeps the target user's state local
 * via react-query keyed on the target id. `target_user_id` is appended via the
 * generated input schemas' `[k: string]: any` index signature, so this stays
 * type-safe even before the generated MCP types regenerate to name the field
 * explicitly.
 */

export type CatalogProvider = ProviderCatalogOutputSchema.ProviderCatalogItem;
export type ConnectedProvider = ProviderConnectedOutputSchema.ProviderCatalogItem;
export type UserModel = ProviderModelsConnectedOutputSchema.ProviderModelOutput;

/** Full provider catalog (static; not target-scoped, but accepts it harmlessly). */
export async function fetchUserCatalog(
  targetUserId: string,
): Promise<CatalogProvider[]> {
  const response = await callMcpTool("provider_catalog", { target_user_id: targetUserId });
  return response.providers;
}

/** Providers the target user currently has live credentials for. */
export async function fetchUserConnectedProviders(
  targetUserId: string,
): Promise<ConnectedProvider[]> {
  const response = await callMcpTool("provider_connected", { target_user_id: targetUserId });
  return response.providers;
}

/** Tool-call-capable models on providers the target user has connected. */
export async function fetchUserConnectedModels(
  targetUserId: string,
): Promise<UserModel[]> {
  const response = await callMcpTool("provider_models_connected", {
    target_user_id: targetUserId,
  });
  const seen = new Set<string>();
  const models: UserModel[] = [];
  for (const model of response.models) {
    if (model.tool_call === false) continue;
    if (seen.has(model.id)) continue;
    seen.add(model.id);
    models.push(model);
  }
  return models;
}

/** The target user's ordered model selection plus per-model caps. */
export interface UserModelSelection {
  /** Full `"provider/model"` ids, high → low priority. */
  models: string[];
  /** Per-model concurrency caps keyed by full `"provider/model"` id. */
  maxSessions: Record<string, number>;
}

function parseMaxSessions(raw: unknown): Record<string, number> {
  return raw && typeof raw === "object" ? (raw as Record<string, number>) : {};
}

/** The target user's ordered model selection (high → low priority) + caps. */
export async function fetchUserModelSelection(
  targetUserId: string,
): Promise<UserModelSelection> {
  const response = await callMcpTool("user_settings_get", { target_user_id: targetUserId });
  if (response.ok === false) {
    throw new Error(response.error ?? "Failed to load user settings");
  }
  return {
    models: Array.isArray(response.models) ? response.models : [],
    maxSessions: parseMaxSessions(response.max_sessions),
  };
}

/** Persist the target user's ordered model selection and per-model caps. */
export async function saveUserModelSelection(
  targetUserId: string,
  models: string[],
  maxSessions: Record<string, number>,
): Promise<UserModelSelection> {
  const response = await callMcpTool("user_settings_set", {
    target_user_id: targetUserId,
    models,
    max_sessions: maxSessions,
  });
  if (!response.ok) {
    throw new Error(response.error ?? "Failed to save user models");
  }
  return {
    models: Array.isArray(response.models) ? response.models : models,
    maxSessions: parseMaxSessions(response.max_sessions),
  };
}

/** Store an API key credential owned by the target user. */
export async function setUserCredential(args: {
  targetUserId: string;
  providerId: string;
  keyName: string;
  apiKey: string;
}): Promise<void> {
  const response = await callMcpTool("credential_set", {
    target_user_id: args.targetUserId,
    provider_id: args.providerId,
    key_name: args.keyName,
    api_key: args.apiKey,
  });
  if (!response.ok || !response.success) {
    throw new Error(response.error ?? "Failed to store credential");
  }
}

export interface UserOAuthPending {
  userCode: string;
  verificationUri?: string;
  verificationUriComplete: string;
  expiresInSecs: number;
}

export type UserOAuthResult =
  | { kind: "connected" }
  | { kind: "pending"; pending: UserOAuthPending }
  | { kind: "error"; message: string };

/**
 * Kick off the device-code OAuth flow for the target user. On a pending
 * flow the server hands back a short `user_code` + `verification_uri_complete`
 * and spawns a background poller; the caller surfaces the code and awaits a
 * `credential.updated` SSE (or polls `provider_connected`) to confirm.
 */
export async function startUserOAuth(
  targetUserId: string,
  providerId: string,
): Promise<UserOAuthResult> {
  try {
    const result = await callMcpTool("provider_oauth_start", {
      target_user_id: targetUserId,
      provider_id: providerId,
    });
    if (!result.ok) {
      return { kind: "error", message: result.error ?? "OAuth flow failed" };
    }
    if (result.pending && result.user_code && result.verification_uri_complete) {
      return {
        kind: "pending",
        pending: {
          userCode: result.user_code,
          verificationUri: result.verification_uri ?? undefined,
          verificationUriComplete: result.verification_uri_complete,
          expiresInSecs:
            typeof result.expires_in === "number" ? result.expires_in : 900,
        },
      };
    }
    return { kind: "connected" };
  } catch (error) {
    return {
      kind: "error",
      message: error instanceof Error ? error.message : "OAuth flow failed",
    };
  }
}
