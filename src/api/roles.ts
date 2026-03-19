import { getServerPort } from "@/tauri/commands";

async function getBaseUrl(): Promise<string> {
  const port = await getServerPort();
  return `http://127.0.0.1:${port}`;
}

export type BaseRole = "worker" | "task_reviewer" | "pm" | "groomer";

export interface Role {
  id: string;
  name: string;
  base_role: BaseRole;
  description: string;
  system_prompt_extensions: string[];
  is_default: boolean;
  learned_prompt: string | null;
}

export interface LearnedPromptAmendment {
  id: string;
  proposed_text: string;
  action: "keep" | "discard";
  metrics_before: Record<string, number>;
  metrics_after: Record<string, number>;
  created_at: string;
}

export interface LearnedPromptHistory {
  learned_prompt: string | null;
  amendments: LearnedPromptAmendment[];
}

export interface CreateRoleRequest {
  name: string;
  base_role: BaseRole;
  description: string;
  system_prompt_extensions: string[];
}

export interface UpdateRoleRequest {
  name?: string;
  description?: string;
  system_prompt_extensions?: string[];
}

export async function fetchRoles(): Promise<Role[]> {
  const baseUrl = await getBaseUrl();
  const response = await fetch(`${baseUrl}/roles`);
  if (!response.ok) {
    throw new Error(`Failed to fetch roles: ${response.status}`);
  }
  const data = (await response.json()) as { roles: Role[] };
  return data.roles;
}

export async function createRole(request: CreateRoleRequest): Promise<Role> {
  const baseUrl = await getBaseUrl();
  const response = await fetch(`${baseUrl}/roles`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(request),
  });
  if (!response.ok) {
    const text = await response.text();
    throw new Error(`Failed to create role: ${text || response.status}`);
  }
  return response.json() as Promise<Role>;
}

export async function updateRole(id: string, request: UpdateRoleRequest): Promise<Role> {
  const baseUrl = await getBaseUrl();
  const response = await fetch(`${baseUrl}/roles/${id}`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(request),
  });
  if (!response.ok) {
    const text = await response.text();
    throw new Error(`Failed to update role: ${text || response.status}`);
  }
  return response.json() as Promise<Role>;
}

export async function deleteRole(id: string): Promise<void> {
  const baseUrl = await getBaseUrl();
  const response = await fetch(`${baseUrl}/roles/${id}`, {
    method: "DELETE",
  });
  if (!response.ok) {
    const text = await response.text();
    throw new Error(`Failed to delete role: ${text || response.status}`);
  }
}

// ── Metrics ───────────────────────────────────────────────────────────────────

export interface RoleMetricPoint {
  /** ISO date string, e.g. "2026-03-01" */
  date: string;
  success_rate: number;
}

export interface RoleMetrics {
  role_id: string;
  role_name: string;
  base_role: BaseRole;
  is_default: boolean;
  task_count: number;
  success_rate: number | null;
  avg_token_usage: number | null;
  avg_time_to_complete_seconds: number | null;
  verification_pass_rate: number | null;
  reopen_rate: number | null;
  /** trend: positive = improving, negative = declining, null = not enough data */
  success_rate_trend: number | null;
  history: RoleMetricPoint[];
}

export interface RoleMetricsResponse {
  metrics: RoleMetrics[];
  generated_at: string;
}

export async function fetchRoleMetrics(): Promise<RoleMetricsResponse> {
  const baseUrl = await getBaseUrl();
  const response = await fetch(`${baseUrl}/roles/metrics`);
  if (!response.ok) {
    throw new Error(`Failed to fetch role metrics: ${response.status}`);
  }
  return response.json() as Promise<RoleMetricsResponse>;
}

export async function fetchLearnedPromptHistory(id: string): Promise<LearnedPromptHistory> {
  const baseUrl = await getBaseUrl();
  const response = await fetch(`${baseUrl}/roles/${id}/learned-prompt/history`);
  if (!response.ok) {
    throw new Error(`Failed to fetch learned prompt history: ${response.status}`);
  }
  return response.json() as Promise<LearnedPromptHistory>;
}

export async function clearLearnedPrompt(id: string): Promise<void> {
  const baseUrl = await getBaseUrl();
  const response = await fetch(`${baseUrl}/roles/${id}/learned-prompt`, {
    method: "DELETE",
  });
  if (!response.ok) {
    const text = await response.text();
    throw new Error(`Failed to clear learned prompt: ${text || response.status}`);
  }
}
