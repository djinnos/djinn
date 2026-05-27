import { callMcpTool } from "@/api/mcpClient";

export interface UserSettings {
  autoApprovePrs: boolean;
  /**
   * The user's ordered model id list (priority high → low). Full
   * `"provider/model"` ids, e.g. `"openai/gpt-5.5"`. `[]` when unset.
   */
  models: string[];
  /**
   * Per-user per-model concurrency caps, keyed by full `"provider/model"` id.
   * `{}` when unset; consumers default missing entries to 1.
   */
  maxSessions: Record<string, number>;
}

interface RawGet {
  ok?: boolean;
  user_id?: string | null;
  auto_approve_prs?: boolean;
  models?: string[] | null;
  max_sessions?: Record<string, number> | null;
  error?: string | null;
}

interface RawSet {
  ok?: boolean;
  applied?: boolean;
  auto_approve_prs?: boolean | null;
  models?: string[] | null;
  max_sessions?: Record<string, number> | null;
  error?: string | null;
}

function parseMaxSessions(raw: Record<string, number> | null | undefined): Record<string, number> {
  return raw && typeof raw === "object" ? raw : {};
}

export async function fetchUserSettings(): Promise<UserSettings> {
  const resp = (await callMcpTool("user_settings_get", {})) as RawGet;
  if (resp?.ok === false) {
    throw new Error(resp.error ?? "failed to load user settings");
  }
  return {
    autoApprovePrs: Boolean(resp?.auto_approve_prs),
    models: Array.isArray(resp?.models) ? resp.models : [],
    maxSessions: parseMaxSessions(resp?.max_sessions),
  };
}

export async function patchUserSettings(patch: {
  autoApprovePrs?: boolean;
  /** Full `"provider/model"` ids in priority order. Omit to keep current. */
  models?: string[];
  /** Per-model caps keyed by full `"provider/model"` id. Omit to keep current. */
  maxSessions?: Record<string, number>;
}): Promise<UserSettings> {
  const args: Record<string, unknown> = {};
  if (patch.autoApprovePrs !== undefined) {
    args.auto_approve_prs = patch.autoApprovePrs;
  }
  if (patch.models !== undefined) {
    args.models = patch.models;
  }
  if (patch.maxSessions !== undefined) {
    args.max_sessions = patch.maxSessions;
  }
  const resp = (await callMcpTool("user_settings_set", args)) as RawSet;
  if (resp?.ok === false) {
    throw new Error(resp.error ?? "failed to save user settings");
  }
  return {
    autoApprovePrs: Boolean(resp?.auto_approve_prs),
    models: Array.isArray(resp?.models) ? resp.models : [],
    maxSessions: parseMaxSessions(resp?.max_sessions),
  };
}
