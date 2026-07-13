import { callMcpTool } from "@/api/mcpClient";
import { type ModelLanes, parseLanes } from "@/api/userSettings";
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

/**
 * Sentinel `targetId` meaning "the signed-in caller themselves". The server's
 * `target_user_id` arg is admin-only (see `resolve_effective_user`), so a
 * non-admin configuring their OWN settings must omit it entirely. Components
 * pass this sentinel; the helpers below translate it to an empty arg set while
 * still keying react-query caches distinctly from any admin-targeted user.
 */
export const SELF_TARGET = "__self__";

/** Build the `{ target_user_id }` arg, omitting it for the self sentinel. */
function targetArgs(targetId: string): { target_user_id?: string } {
  return targetId === SELF_TARGET ? {} : { target_user_id: targetId };
}

/** Full provider catalog (static; not target-scoped, but accepts it harmlessly). */
export async function fetchUserCatalog(
  targetUserId: string,
): Promise<CatalogProvider[]> {
  const response = await callMcpTool("provider_catalog", targetArgs(targetUserId));
  return response.providers;
}

/** Providers the target user currently has live credentials for. */
export async function fetchUserConnectedProviders(
  targetUserId: string,
): Promise<ConnectedProvider[]> {
  const response = await callMcpTool("provider_connected", targetArgs(targetUserId));
  return response.providers;
}

/** Tool-call-capable models on providers the target user has connected. */
export async function fetchUserConnectedModels(
  targetUserId: string,
): Promise<UserModel[]> {
  const response = await callMcpTool("provider_models_connected", targetArgs(targetUserId));
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

/** The target user's per-role model lanes plus per-model caps. */
export interface UserModelSelection {
  /** Per-role lanes (`plan`/`implement`/`review`), each high → low priority. */
  lanes: ModelLanes;
  /** Per-model concurrency caps keyed by full `"provider/model"` id. */
  maxSessions: Record<string, number>;
  /**
   * Cross-model ("Thorough") review toggle. When true (the default), the
   * reviewer prefers a model id different from the implementer's.
   */
  diverseReview: boolean;
  /**
   * Cross-model ("Diverse") refinement toggle. When true (the default),
   * proposal-refinement roles prefer a different model from the primary
   * task model. Falls back to same-model when alternatives are unavailable.
   */
  diverseRefinement: boolean;
}

function parseMaxSessions(raw: unknown): Record<string, number> {
  return raw && typeof raw === "object" ? (raw as Record<string, number>) : {};
}

/** The target user's per-role model lanes + caps. */
export async function fetchUserModelSelection(
  targetUserId: string,
): Promise<UserModelSelection> {
  const response = await callMcpTool("user_settings_get", targetArgs(targetUserId));
  if (response.ok === false) {
    throw new Error(response.error ?? "Failed to load user settings");
  }
  return {
    lanes: parseLanes(response.lanes),
    maxSessions: parseMaxSessions(response.max_sessions),
    diverseReview: response.diverse_review !== false,
    diverseRefinement: response.diverse_refinement !== false,
  };
}

/**
 * Persist the target user's per-role model lanes, per-model caps, and the
 * cross-model review/refinement toggles. `diverseReview` and
 * `diverseRefinement` are optional — omit to leave the server value untouched
 * (e.g. a lanes-only save).
 */
export async function saveUserModelSelection(
  targetUserId: string,
  lanes: ModelLanes,
  maxSessions: Record<string, number>,
  diverseReview?: boolean,
  diverseRefinement?: boolean,
): Promise<UserModelSelection> {
  const response = await callMcpTool("user_settings_set", {
    ...targetArgs(targetUserId),
    lanes,
    max_sessions: maxSessions,
    ...(diverseReview === undefined ? {} : { diverse_review: diverseReview }),
    ...(diverseRefinement === undefined ? {} : { diverse_refinement: diverseRefinement }),
  });
  if (!response.ok) {
    throw new Error(response.error ?? "Failed to save user models");
  }
  return {
    lanes: parseLanes(response.lanes),
    maxSessions: parseMaxSessions(response.max_sessions),
    diverseReview: response.diverse_review !== false,
    diverseRefinement: response.diverse_refinement !== false,
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
    ...targetArgs(args.targetUserId),
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
      ...targetArgs(targetUserId),
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
