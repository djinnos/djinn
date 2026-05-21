import { callMcpTool } from "@/api/mcpClient";

export interface UserSettings {
  autoApprovePrs: boolean;
}

interface RawGet {
  ok?: boolean;
  user_id?: string | null;
  auto_approve_prs?: boolean;
  error?: string | null;
}

interface RawSet {
  ok?: boolean;
  applied?: boolean;
  auto_approve_prs?: boolean | null;
  error?: string | null;
}

export async function fetchUserSettings(): Promise<UserSettings> {
  const resp = (await callMcpTool("user_settings_get", {})) as RawGet;
  if (resp?.ok === false) {
    throw new Error(resp.error ?? "failed to load user settings");
  }
  return {
    autoApprovePrs: Boolean(resp?.auto_approve_prs),
  };
}

export async function patchUserSettings(patch: {
  autoApprovePrs?: boolean;
}): Promise<UserSettings> {
  const args: Record<string, unknown> = {};
  if (patch.autoApprovePrs !== undefined) {
    args.auto_approve_prs = patch.autoApprovePrs;
  }
  const resp = (await callMcpTool("user_settings_set", args)) as RawSet;
  if (resp?.ok === false) {
    throw new Error(resp.error ?? "failed to save user settings");
  }
  return {
    autoApprovePrs: Boolean(resp?.auto_approve_prs),
  };
}
