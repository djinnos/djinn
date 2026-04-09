import { getServerPort } from "@/electron/commands";

async function getBaseUrl(): Promise<string> {
  const port = await getServerPort();
  return `http://127.0.0.1:${port}`;
}

// ── MCP Servers ──────────────────────────────────────────────────────────────

export interface McpServer {
  name: string;
  url: string | null;
  command: string | null;
  args: string[];
  env: Record<string, string>;
}

export interface CreateMcpServerRequest {
  project_id: string;
  name: string;
  url?: string;
  command?: string;
  args?: string[];
  env?: Record<string, string>;
}

export interface UpdateMcpServerRequest {
  project_id: string;
  name: string;
  url?: string;
  command?: string;
  args?: string[];
  env?: Record<string, string>;
}

export async function fetchMcpServers(projectId: string): Promise<McpServer[]> {
  const baseUrl = await getBaseUrl();
  const res = await fetch(
    `${baseUrl}/project/mcp-servers?project_id=${encodeURIComponent(projectId)}`,
  );
  if (!res.ok) throw new Error(`Failed to fetch MCP servers: ${res.status}`);
  const data = (await res.json()) as { servers: McpServer[] };
  return data.servers;
}

export async function createMcpServer(req: CreateMcpServerRequest): Promise<McpServer> {
  const baseUrl = await getBaseUrl();
  const res = await fetch(`${baseUrl}/project/mcp-servers`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(req),
  });
  if (!res.ok) {
    const text = await res.text();
    throw new Error(text || `Failed to create MCP server: ${res.status}`);
  }
  return res.json() as Promise<McpServer>;
}

export async function updateMcpServer(req: UpdateMcpServerRequest): Promise<McpServer> {
  const baseUrl = await getBaseUrl();
  const res = await fetch(`${baseUrl}/project/mcp-servers/update`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(req),
  });
  if (!res.ok) {
    const text = await res.text();
    throw new Error(text || `Failed to update MCP server: ${res.status}`);
  }
  return res.json() as Promise<McpServer>;
}

export async function deleteMcpServer(projectId: string, name: string): Promise<void> {
  const baseUrl = await getBaseUrl();
  const res = await fetch(
    `${baseUrl}/project/mcp-servers/delete?project_id=${encodeURIComponent(projectId)}&name=${encodeURIComponent(name)}`,
    { method: "DELETE" },
  );
  if (!res.ok) {
    const text = await res.text();
    throw new Error(text || `Failed to delete MCP server: ${res.status}`);
  }
}

// ── MCP Default Assignments ──────────────────────────────────────────────────

export interface McpDefaults {
  agent_mcp_defaults: Record<string, string[]>;
  global_skills: string[];
}

export async function fetchMcpDefaults(projectId: string): Promise<McpDefaults> {
  const baseUrl = await getBaseUrl();
  const res = await fetch(
    `${baseUrl}/project/mcp-defaults?project_id=${encodeURIComponent(projectId)}`,
  );
  if (!res.ok) throw new Error(`Failed to fetch MCP defaults: ${res.status}`);
  return res.json() as Promise<McpDefaults>;
}

export async function saveMcpDefaults(
  projectId: string,
  defaults: McpDefaults,
): Promise<McpDefaults> {
  const baseUrl = await getBaseUrl();
  const res = await fetch(`${baseUrl}/project/mcp-defaults`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ project_id: projectId, ...defaults }),
  });
  if (!res.ok) {
    const text = await res.text();
    throw new Error(text || `Failed to save MCP defaults: ${res.status}`);
  }
  return res.json() as Promise<McpDefaults>;
}

// ── Skills ───────────────────────────────────────────────────────────────────

export interface Skill {
  name: string;
  description: string | null;
  content: string;
}

export interface CreateSkillRequest {
  project_id: string;
  name: string;
  description?: string;
  content: string;
}

export interface UpdateSkillRequest {
  project_id: string;
  name: string;
  description?: string;
  content: string;
}

export async function fetchSkills(projectId: string): Promise<Skill[]> {
  const baseUrl = await getBaseUrl();
  const res = await fetch(
    `${baseUrl}/project/skills?project_id=${encodeURIComponent(projectId)}`,
  );
  if (!res.ok) throw new Error(`Failed to fetch skills: ${res.status}`);
  const data = (await res.json()) as { skills: Skill[] };
  return data.skills;
}

export async function createSkill(req: CreateSkillRequest): Promise<Skill> {
  const baseUrl = await getBaseUrl();
  const res = await fetch(`${baseUrl}/project/skills`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(req),
  });
  if (!res.ok) {
    const text = await res.text();
    throw new Error(text || `Failed to create skill: ${res.status}`);
  }
  return res.json() as Promise<Skill>;
}

export async function updateSkill(req: UpdateSkillRequest): Promise<Skill> {
  const baseUrl = await getBaseUrl();
  const res = await fetch(`${baseUrl}/project/skills/update`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(req),
  });
  if (!res.ok) {
    const text = await res.text();
    throw new Error(text || `Failed to update skill: ${res.status}`);
  }
  return res.json() as Promise<Skill>;
}

export async function deleteSkill(projectId: string, name: string): Promise<void> {
  const baseUrl = await getBaseUrl();
  const res = await fetch(
    `${baseUrl}/project/skills/delete?project_id=${encodeURIComponent(projectId)}&name=${encodeURIComponent(name)}`,
    { method: "DELETE" },
  );
  if (!res.ok) {
    const text = await res.text();
    throw new Error(text || `Failed to delete skill: ${res.status}`);
  }
}
