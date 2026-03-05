import { getServerPort } from "@/tauri/commands";

async function getBaseUrl(): Promise<string> {
  const port = await getServerPort();
  return `http://127.0.0.1:${port}`;
}

export async function checkServerHealth(): Promise<{ status: "ok" }> {
  const baseUrl = await getBaseUrl();
  const response = await fetch(`${baseUrl}/health`);
  if (!response.ok) {
    throw new Error(`Health check failed: ${response.status}`);
  }
  return response.json();
}

export interface Provider {
  id: string;
  name: string;
  description: string;
  requires_api_key: boolean;
}

export async function fetchProviderCatalog(): Promise<Provider[]> {
  const baseUrl = await getBaseUrl();
  const response = await fetch(`${baseUrl}/providers/catalog`);
  if (!response.ok) {
    throw new Error(`Failed to fetch provider catalog: ${response.status}`);
  }
  return response.json();
}

export async function validateProviderApiKey(
  providerId: string,
  apiKey: string
): Promise<{ valid: boolean; error?: string }> {
  const baseUrl = await getBaseUrl();
  const response = await fetch(`${baseUrl}/providers/validate`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
    },
    body: JSON.stringify({
      provider_id: providerId,
      api_key: apiKey,
    }),
  });
  
  if (!response.ok) {
    const error = await response.text();
    return { valid: false, error: error || `Validation failed: ${response.status}` };
  }
  
  return response.json();
}

export async function saveProviderCredentials(
  providerId: string,
  apiKey: string
): Promise<void> {
  const baseUrl = await getBaseUrl();
  const response = await fetch(`${baseUrl}/credentials/set`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
    },
    body: JSON.stringify({
      provider_id: providerId,
      api_key: apiKey,
    }),
  });
  
  if (!response.ok) {
    throw new Error(`Failed to save credentials: ${response.status}`);
  }
}

// Project-related types and API functions

export interface Project {
  id: string;
  name: string;
  path: string;
  created_at: string;
  updated_at: string;
}

export interface ProjectListResponse {
  projects: Project[];
}

export async function fetchProjects(): Promise<Project[]> {
  const baseUrl = await getBaseUrl();
  const response = await fetch(`${baseUrl}/projects`);
  if (!response.ok) {
    throw new Error(`Failed to fetch projects: ${response.status}`);
  }
  const data: ProjectListResponse = await response.json();
  return data.projects;
}

export async function addProject(path: string): Promise<Project> {
  const baseUrl = await getBaseUrl();
  const response = await fetch(`${baseUrl}/projects/add`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
    },
    body: JSON.stringify({ path }),
  });
  
  if (!response.ok) {
    const error = await response.text();
    throw new Error(`Failed to add project: ${error || response.status}`);
  }
  
  return response.json();
}

// Provider configuration check

export interface ProviderConfigStatus {
  configured: boolean;
  providers: string[];
}

export async function fetchProviderConfigStatus(): Promise<ProviderConfigStatus> {
  const baseUrl = await getBaseUrl();
  const response = await fetch(`${baseUrl}/providers/configured`);
  if (!response.ok) {
    throw new Error(`Failed to fetch provider config status: ${response.status}`);
  }
  return response.json();
}
