/**
 * Image-catalog MCP tool wrappers.
 *
 * The catalog (`images` table) holds reusable, content-addressed
 * `EnvironmentConfig` blobs that a project can reference instead of
 * authoring its own per-project config. The five underlying MCP tools
 * (`image_list`, `image_create`, `image_update`, `image_delete`,
 * `project_set_image`) are type-checked against the generated
 * `mcp-tools.gen.ts`.
 *
 * The image `config` payload is typed as `unknown` / `{[k: string]: any}`
 * in the generated bindings, so we narrow it to the same structural
 * `EnvironmentConfig` the per-project editor binds against.
 */
import { callMcpTool } from "@/api/mcpClient";
import { type EnvironmentConfig, normalizeConfig } from "@/api/environmentConfig";

/** Build status of an image's own config: none | building | ready | failed. */
export type ImageBuildStatus = "none" | "building" | "ready" | "failed" | string;

export interface CatalogImage {
  id: string;
  name: string;
  description?: string;
  status: ImageBuildStatus;
  config: EnvironmentConfig;
  /** Service-preset ids injected as native sidecars into every Pod that runs
   *  this image (reachable on 127.0.0.1 with the connection string in the
   *  preset's env var, e.g. TEST_POSTGRES_URL). */
  servicePresets: string[];
}

/** A fixed backing-service preset from the catalog (users do not create these). */
export interface ServicePreset {
  id: string;
  name: string;
  serviceType: string;
  image: string;
  connEnvVar: string;
}

export interface MutationResult {
  ok: boolean;
  error?: string;
  id?: string;
}

/**
 * Per-language available toolchain versions, fetched live from upstream
 * (cached, with a static fallback) by the `toolchain_versions` MCP tool.
 */
export interface ToolchainVersions {
  [language: string]: string[];
}

/**
 * Fetch the curated/live version lists for each toolchain. Throws on a
 * non-ok status so the caller (react-query) can fall back to a static list.
 */
export async function listToolchainVersions(): Promise<ToolchainVersions> {
  const response = await callMcpTool("toolchain_versions", {});
  if (response.status !== "ok") {
    throw new Error(response.error ?? "Failed to load toolchain versions");
  }
  return (response.versions ?? {}) as ToolchainVersions;
}

/**
 * Strip image-config fields the catalog backend rejects before sending:
 *  - `null`/`undefined` `languages.*` entries (a disabled language); the
 *    image-builder emits an install block for ANY present key, so an
 *    explicit null would still try to install.
 *  - any `verification` key — the field was removed from `EnvironmentConfig`,
 *    and the server 400s on the unknown field.
 *
 * Returns a shallow copy; never mutates the caller's config.
 */
export function sanitizeImageConfig(config: EnvironmentConfig): EnvironmentConfig {
  const { ...rest } = config as EnvironmentConfig & { verification?: unknown };
  delete (rest as { verification?: unknown }).verification;
  const languages: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(rest.languages ?? {})) {
    if (value != null) languages[key] = value;
  }
  return { ...rest, languages: languages as EnvironmentConfig["languages"] };
}

/** List every registered catalog image. */
export async function listImages(): Promise<CatalogImage[]> {
  const response = await callMcpTool("image_list", {});
  if (response.status !== "ok") {
    throw new Error(response.error ?? "Failed to load images");
  }
  return (response.images ?? []).map((img) => ({
    id: img.id,
    name: img.name,
    description: img.description,
    status: img.status,
    config: normalizeConfig(img.config),
    servicePresets: img.service_presets ?? [],
  }));
}

/** List the fixed catalog of backing-service presets (Postgres/Redis/RabbitMQ). */
export async function listServicePresets(): Promise<ServicePreset[]> {
  const response = await callMcpTool("service_preset_list", {});
  if (response.status !== "ok") {
    throw new Error(response.error ?? "Failed to load service presets");
  }
  return (response.presets ?? []).map((preset) => ({
    id: preset.id,
    name: preset.name,
    serviceType: preset.service_type,
    image: preset.image,
    connEnvVar: preset.conn_env_var,
  }));
}

/** Replace the full set of backing services injected into this image's Pods. */
export async function setImageServices(
  id: string,
  presetIds: string[],
): Promise<MutationResult> {
  const response = await callMcpTool("image_set_services", {
    id,
    preset_ids: presetIds,
  });
  if (response.status !== "ok") {
    return { ok: false, error: response.error ?? "set image services failed" };
  }
  return { ok: true, id: response.id };
}

/** Register a new catalog image. */
export async function createImage(input: {
  name: string;
  description?: string;
  config: EnvironmentConfig;
}): Promise<MutationResult> {
  const response = await callMcpTool("image_create", {
    name: input.name,
    description: input.description,
    config: sanitizeImageConfig(input.config) as unknown as Record<string, unknown>,
  });
  if (response.status !== "ok") {
    return { ok: false, error: response.error ?? "create failed" };
  }
  return { ok: true, id: response.id };
}

/** Update an existing catalog image in place. */
export async function updateImage(input: {
  id: string;
  name: string;
  description?: string;
  config: EnvironmentConfig;
}): Promise<MutationResult> {
  const response = await callMcpTool("image_update", {
    id: input.id,
    name: input.name,
    description: input.description,
    config: sanitizeImageConfig(input.config) as unknown as Record<string, unknown>,
  });
  if (response.status !== "ok") {
    return { ok: false, error: response.error ?? "update failed" };
  }
  return { ok: true, id: response.id };
}

/** Delete a catalog image. Fails server-side if a project still uses it. */
export async function deleteImage(id: string): Promise<MutationResult> {
  const response = await callMcpTool("image_delete", { id });
  if (response.status !== "ok") {
    return { ok: false, error: response.error ?? "delete failed" };
  }
  return { ok: true };
}

/**
 * Assign a catalog image to a project (or clear the assignment when
 * `imageId` is null/undefined).
 */
export async function setProjectImage(
  project: string,
  imageId: string | null,
): Promise<MutationResult> {
  const response = await callMcpTool("project_set_image", {
    project,
    image_id: imageId ?? undefined,
  });
  if (response.status !== "ok") {
    return { ok: false, error: response.error ?? "set image failed" };
  }
  return { ok: true };
}
