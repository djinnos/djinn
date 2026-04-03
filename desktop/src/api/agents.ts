import { getServerPort } from "@/electron/commands";

async function getBaseUrl(): Promise<string> {
  const port = await getServerPort();
  return `http://127.0.0.1:${port}`;
}

export type BaseRole = "worker" | "reviewer" | "lead" | "planner";

export interface Agent {
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

export interface CreateAgentRequest {
  project_id: string;
  name: string;
  base_role: BaseRole;
  description: string;
  system_prompt_extensions: string[];
}

export interface UpdateAgentRequest {
  name?: string;
  description?: string;
  system_prompt_extensions?: string[];
}

export async function fetchAgents(projectId?: string): Promise<Agent[]> {
  const baseUrl = await getBaseUrl();
  const url = projectId
    ? `${baseUrl}/agents?project_id=${encodeURIComponent(projectId)}`
    : `${baseUrl}/agents`;
  const response = await fetch(url);
  if (!response.ok) {
    throw new Error(`Failed to fetch agents: ${response.status}`);
  }
  const data = (await response.json()) as { agents: Agent[] };
  return data.agents;
}

export async function createAgent(request: CreateAgentRequest): Promise<Agent> {
  const baseUrl = await getBaseUrl();
  const response = await fetch(`${baseUrl}/agents`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(request),
  });
  if (!response.ok) {
    const text = await response.text();
    throw new Error(`Failed to create agent: ${text || response.status}`);
  }
  return response.json() as Promise<Agent>;
}

export async function updateAgent(id: string, request: UpdateAgentRequest): Promise<Agent> {
  const baseUrl = await getBaseUrl();
  const response = await fetch(`${baseUrl}/agents/${id}`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(request),
  });
  if (!response.ok) {
    const text = await response.text();
    throw new Error(`Failed to update agent: ${text || response.status}`);
  }
  return response.json() as Promise<Agent>;
}

export async function deleteAgent(id: string): Promise<void> {
  const baseUrl = await getBaseUrl();
  const response = await fetch(`${baseUrl}/agents/${id}`, {
    method: "DELETE",
  });
  if (!response.ok) {
    const text = await response.text();
    throw new Error(`Failed to delete agent: ${text || response.status}`);
  }
}

// ── Metrics ───────────────────────────────────────────────────────────────────

export interface AgentMetricPoint {
  /** ISO date string, e.g. "2026-03-01" */
  date: string;
  success_rate: number;
}

export interface AgentMetrics {
  agent_id: string;
  agent_name: string;
  base_role: BaseRole;
  is_default: boolean;
  task_count: number;
  success_rate: number | null;
  avg_token_usage: number | null;
  avg_tokens_in: number | null;
  avg_tokens_out: number | null;
  avg_time_to_complete_seconds: number | null;
  verification_pass_rate: number | null;
  reopen_rate: number | null;
  /** trend: positive = improving, negative = declining, null = not enough data */
  success_rate_trend: number | null;
  history: AgentMetricPoint[];
}

export interface AgentMetricsResponse {
  metrics: AgentMetrics[];
  generated_at: string;
}

export async function fetchAgentMetrics(projectId: string): Promise<AgentMetricsResponse> {
  const baseUrl = await getBaseUrl();
  const response = await fetch(`${baseUrl}/agents/metrics?project_id=${encodeURIComponent(projectId)}`);
  if (!response.ok) {
    throw new Error(`Failed to fetch agent metrics: ${response.status}`);
  }
  return response.json() as Promise<AgentMetricsResponse>;
}

export async function fetchLearnedPromptHistory(id: string): Promise<LearnedPromptHistory> {
  const baseUrl = await getBaseUrl();
  const response = await fetch(`${baseUrl}/agents/${id}/learned-prompt/history`);
  if (!response.ok) {
    throw new Error(`Failed to fetch learned prompt history: ${response.status}`);
  }
  return response.json() as Promise<LearnedPromptHistory>;
}

export async function clearLearnedPrompt(id: string): Promise<void> {
  const baseUrl = await getBaseUrl();
  const response = await fetch(`${baseUrl}/agents/${id}/learned-prompt`, {
    method: "DELETE",
  });
  if (!response.ok) {
    const text = await response.text();
    throw new Error(`Failed to clear learned prompt: ${text || response.status}`);
  }
}
