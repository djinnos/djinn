import { callMcpTool } from "@/api/mcpClient";
import { getServerPort } from "@/tauri/commands";

async function getBaseUrl(): Promise<string> {
  const port = await getServerPort();
  return `http://127.0.0.1:${port}`;
}

function providerDescription(provider: ProviderCatalogItem): string {
  const tags: string[] = [];
  if (provider.is_openai_compatible) tags.push("openai-compatible");
  if (provider.oauth_supported) tags.push("oauth");
  const suffix = tags.length > 0 ? ` (${tags.join(", ")})` : "";
  return `${provider.npm}${suffix}`;
}

function fallbackKeyName(providerId: string): string {
  const normalized = providerId.replace(/[^a-zA-Z0-9]/g, "_").toUpperCase();
  return `${normalized}_API_KEY`;
}

async function listProviderCatalogRaw(): Promise<ProviderCatalogItem[]> {
  const response = await callMcpTool<ProviderCatalogResponse>("provider_catalog");
  return response.providers;
}

async function resolveKeyName(providerId: string): Promise<string> {
  const providers = await listProviderCatalogRaw();
  const match = providers.find((provider) => provider.id === providerId);
  return match?.env_vars[0] ?? fallbackKeyName(providerId);
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

export interface ProviderCredential {
  provider_id: string;
  configured: boolean;
  valid: boolean;
  api_key_masked?: string;
}

interface ProviderCatalogResponse {
  providers: ProviderCatalogItem[];
  total: number;
}

interface ProviderCatalogItem {
  id: string;
  name: string;
  npm: string;
  env_vars: string[];
  is_openai_compatible: boolean;
  oauth_supported: boolean;
  base_url: string;
}

interface ProviderValidateResponse {
  ok: boolean;
  error?: string;
}

interface CredentialSetResponse {
  success: boolean;
  error?: string;
}

interface ProjectInfo {
  id: string;
  name: string;
  path: string;
}

interface ProjectListMcpResponse {
  projects: ProjectInfo[];
}

interface ProjectAddMcpResponse {
  project: ProjectInfo;
}

interface ProviderConnectedResponse {
  providers: Array<{ id: string }>;
  total: number;
}

interface CredentialListResponse {
  credentials: Array<{
    provider_id: string;
    key_name: string;
  }>;
}

interface CredentialDeleteResponse {
  success: boolean;
  deleted: boolean;
  error?: string;
}

export async function fetchProviderCatalog(): Promise<Provider[]> {
  const providers = await listProviderCatalogRaw();
  return providers.map((provider) => ({
    id: provider.id,
    name: provider.name,
    description: providerDescription(provider),
    requires_api_key: provider.env_vars.length > 0,
  }));
}

export async function validateProviderApiKey(
  providerId: string,
  apiKey: string
): Promise<{ valid: boolean; error?: string }> {
  try {
    const providers = await listProviderCatalogRaw();
    const provider = providers.find((entry) => entry.id === providerId);
    if (!provider) {
      return { valid: false, error: `Unknown provider: ${providerId}` };
    }

    const result = await callMcpTool<ProviderValidateResponse>("provider_validate", {
      provider_id: providerId,
      base_url: provider.base_url,
      api_key: apiKey,
    });

    if (!result.ok) {
      return { valid: false, error: result.error ?? "Validation failed" };
    }

    return { valid: true };
  } catch (error) {
    return {
      valid: false,
      error: error instanceof Error ? error.message : "Validation failed",
    };
  }
}

export async function saveProviderCredentials(
  providerId: string,
  apiKey: string
): Promise<void> {
  const keyName = await resolveKeyName(providerId);
  const response = await callMcpTool<CredentialSetResponse>("credential_set", {
    provider_id: providerId,
    key_name: keyName,
    api_key: apiKey,
  });

  if (!response.success) {
    throw new Error(response.error ?? "Failed to save credentials");
  }
}

// Project-related types and API functions

export interface Project {
  id: string;
  name: string;
  path: string;
  created_at?: string;
  updated_at?: string;
}

export async function fetchProjects(): Promise<Project[]> {
  const data = await callMcpTool<ProjectListMcpResponse>("project_list");
  return data.projects;
}

export async function addProject(path: string): Promise<Project> {
  const segments = path.split(/[\\/]/).filter(Boolean);
  const inferredName = segments[segments.length - 1] ?? "project";
  const response = await callMcpTool<ProjectAddMcpResponse>("project_add", {
    name: inferredName,
    path,
  });

  return response.project;
}

// Provider configuration check

export interface ProviderConfigStatus {
  configured: boolean;
  providers: string[];
}

export async function fetchProviderConfigStatus(): Promise<ProviderConfigStatus> {
  const response = await callMcpTool<ProviderConnectedResponse>("provider_connected");
  const providers = response.providers.map((provider) => provider.id);
  return {
    configured: providers.length > 0,
    providers,
  };
}


export async function fetchCredentialList(): Promise<ProviderCredential[]> {
  const [credentials, connected] = await Promise.all([
    callMcpTool<CredentialListResponse>("credential_list"),
    callMcpTool<ProviderConnectedResponse>("provider_connected"),
  ]);

  const connectedProviders = new Set(connected.providers.map((provider) => provider.id));

  const byProvider = new Map<string, ProviderCredential>();
  for (const credential of credentials.credentials) {
    byProvider.set(credential.provider_id, {
      provider_id: credential.provider_id,
      configured: true,
      valid: connectedProviders.has(credential.provider_id),
    });
  }

  for (const providerId of connectedProviders) {
    if (!byProvider.has(providerId)) {
      byProvider.set(providerId, {
        provider_id: providerId,
        configured: true,
        valid: true,
      });
    }
  }

  return Array.from(byProvider.values());
}


export async function deleteProviderCredentials(providerId: string): Promise<void> {
  const credentials = await callMcpTool<CredentialListResponse>("credential_list");
  const keys = credentials.credentials
    .filter((credential) => credential.provider_id === providerId)
    .map((credential) => credential.key_name);

  if (keys.length === 0) {
    return;
  }

  const results = await Promise.all(
    keys.map((keyName) =>
      callMcpTool<CredentialDeleteResponse>("credential_delete", {
        key_name: keyName,
      })
    )
  );

  const failed = results.find((result) => !result.success || !result.deleted);
  if (failed) {
    throw new Error(failed.error ?? "Failed to delete credentials");
  }
}
