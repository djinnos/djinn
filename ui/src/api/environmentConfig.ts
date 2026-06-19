/**
 * `EnvironmentConfig` — per-project runtime configuration.
 *
 * Hand-written mirror of `djinn_stack::environment::EnvironmentConfig`
 * (server/crates/djinn-stack/src/environment.rs). The MCP get/set tools'
 * `config` payload is typed as `unknown` / `{[k: string]: any}` in the
 * generated mcp-tools.gen.ts, so this module narrows that into a
 * structural type the editor page can bind against.
 */
import { callMcpTool } from "@/api/mcpClient";

export const SCHEMA_VERSION = 1;

export type ConfigSource = "auto-detected" | "user-edited";

export interface RustLanguage {
  default_toolchain: string;
}

export interface NodeLanguage {
  default_version: string;
  default_package_manager?: string | null;
}

export interface SimpleLanguage {
  default_version: string;
}

export type PythonLanguage = SimpleLanguage;
export type GoLanguage = SimpleLanguage;
export type JavaLanguage = SimpleLanguage;
export type RubyLanguage = SimpleLanguage;
export type DotnetLanguage = SimpleLanguage;
export type ClangLanguage = SimpleLanguage;

export interface Languages {
  rust?: RustLanguage;
  node?: NodeLanguage;
  python?: PythonLanguage;
  go?: GoLanguage;
  java?: JavaLanguage;
  ruby?: RubyLanguage;
  dotnet?: DotnetLanguage;
  clang?: ClangLanguage;
}

export type LanguageKey = keyof Languages;

export const LANGUAGE_KEYS: LanguageKey[] = [
  "rust",
  "node",
  "python",
  "go",
  "java",
  "ruby",
  "dotnet",
  "clang",
];

export interface Workspace {
  root: string;
  language: string;
  toolchain?: string | null;
  version?: string | null;
  package_manager?: string | null;
}

/**
 * A lifecycle / setup command.
 *
 * Mirrors `djinn_stack::environment::HookCommand`'s `#[serde(untagged)]`
 * union: a shell string, argv array, or named parallel map.
 */
export type HookCommand =
  | string
  | string[]
  | { [name: string]: HookCommand };

export interface LifecycleHooks {
  post_build: HookCommand[];
  pre_anything: HookCommand[];
  pre_task: HookCommand[];
}

export interface EnvironmentConfig {
  schema_version: number;
  source: ConfigSource;
  languages: Languages;
  workspaces: Workspace[];
  system_packages: string[];
  env: Record<string, string>;
  lifecycle: LifecycleHooks;
}

/**
 * Normalize a raw JSON blob from the server into a fully-populated
 * `EnvironmentConfig`. Applies the same defaults `EnvironmentConfig::empty()`
 * does on the Rust side so the form bindings never have to branch on
 * `undefined` for required nested shapes.
 *
 * Tolerates the pre-cleanup field name `pre_warm` by routing it into
 * `pre_anything` — matches the serde `alias` on the Rust side.
 */
export function normalizeConfig(
  raw: unknown | null | undefined,
): EnvironmentConfig {
  const obj = (raw && typeof raw === "object" ? raw : {}) as Record<string, unknown>;
  const lifecycle = (obj.lifecycle ?? {}) as Record<string, unknown>;
  const env = (obj.env ?? {}) as Record<string, string>;
  const systemPackages = Array.isArray(obj.system_packages)
    ? (obj.system_packages as string[])
    : [];
  const preAnything = Array.isArray(lifecycle.pre_anything)
    ? (lifecycle.pre_anything as HookCommand[])
    : Array.isArray(lifecycle.pre_warm)
      ? (lifecycle.pre_warm as HookCommand[])
      : [];
  return {
    schema_version:
      typeof obj.schema_version === "number" ? obj.schema_version : SCHEMA_VERSION,
    source: (obj.source === "user-edited" ? "user-edited" : "auto-detected") as ConfigSource,
    languages: (obj.languages ?? {}) as Languages,
    workspaces: Array.isArray(obj.workspaces) ? (obj.workspaces as Workspace[]) : [],
    system_packages: systemPackages,
    env: { ...env },
    lifecycle: {
      post_build: Array.isArray(lifecycle.post_build)
        ? (lifecycle.post_build as HookCommand[])
        : [],
      pre_anything: preAnything,
      pre_task: Array.isArray(lifecycle.pre_task)
        ? (lifecycle.pre_task as HookCommand[])
        : [],
    },
  };
}

/**
 * Drop every `languages` entry not pinned by at least one workspace.
 *
 * The Form models languages purely through workspaces: picking a language
 * in a workspace auto-enables it (see `ensureLanguageEnabled` in
 * EnvironmentConfigForm). The inverse has to hold too — the image-builder
 * emits an install block for ANY non-null `languages.X`
 * (`emit_python_block` & friends in dockerfile.rs), independent of the
 * workspace list. Without this reconcile, removing a workspace leaves an
 * orphaned language that keeps installing into the image with no Form
 * control to clear it (you could only fix it from the Raw JSON tab).
 *
 * Guard: with NO workspaces we can't infer intent (and a fresh detection
 * may populate `languages` before `workspaces`), so leave `languages`
 * untouched. Unknown keys pass through. This runs on form mutations +
 * save only — the Raw JSON editor is the escape hatch for the rare
 * language-without-a-workspace case (e.g. installed only for a hook).
 *
 * Returns the same object reference when nothing changed.
 */
export function pruneOrphanLanguages(config: EnvironmentConfig): EnvironmentConfig {
  if (config.workspaces.length === 0) return config;
  const used = new Set(config.workspaces.map((w) => w.language));
  const next: Languages = {};
  let changed = false;
  for (const [key, value] of Object.entries(config.languages)) {
    if ((LANGUAGE_KEYS as string[]).includes(key) && !used.has(key)) {
      changed = true;
      continue;
    }
    (next as Record<string, unknown>)[key] = value;
  }
  return changed ? { ...config, languages: next } : config;
}

/**
 * Fetch the current environment_config for a project.
 *
 * Returns `null` on a fresh row that the boot reseed hook hasn't touched
 * yet (schema_version is `0` in that case — the Rust
 * `column_default_parses_to_empty_with_schema_version_zero` test is the
 * canonical reference).
 */
export async function fetchEnvironmentConfig(projectId: string): Promise<{
  config: EnvironmentConfig;
  seeded: boolean;
  selectedImageId: string | null;
  selectedImageName: string | null;
}> {
  const response = await callMcpTool("project_environment_config_get", {
    project: projectId,
  });
  if (response.status !== "ok") {
    throw new Error(response.error ?? "Failed to load environment config");
  }
  const raw = (response.config ?? {}) as Record<string, unknown>;
  const seeded = typeof raw.schema_version === "number" && raw.schema_version >= 1;
  return {
    config: normalizeConfig(raw),
    seeded,
    selectedImageId: response.selected_image_id ?? null,
    selectedImageName: response.selected_image_name ?? null,
  };
}

export interface SaveResult {
  ok: boolean;
  error?: string;
}

/**
 * Persist a validated EnvironmentConfig. The server re-validates server-side
 * (shell-injection guards, workspace uniqueness, etc) before writing.
 */
export async function saveEnvironmentConfig(
  projectId: string,
  config: EnvironmentConfig,
): Promise<SaveResult> {
  const response = await callMcpTool("project_environment_config_set", {
    project: projectId,
    config: config as unknown as Record<string, unknown>,
  });
  if (response.status !== "ok") {
    return { ok: false, error: response.error ?? "save failed" };
  }
  return { ok: true };
}

export interface ResetResult {
  ok: boolean;
  error?: string;
  config?: EnvironmentConfig;
}

/**
 * Discard the current `environment_config` and regenerate it from the
 * project's detected stack. Server-side: mirrors the boot reseed hook
 * but runs on demand. Fails if the project's stack column is still
 * empty (detection hasn't run yet).
 */
export async function resetEnvironmentConfig(projectId: string): Promise<ResetResult> {
  const response = await callMcpTool("project_environment_config_reset", {
    project: projectId,
  });
  if (response.status !== "ok") {
    return { ok: false, error: response.error ?? "reset failed" };
  }
  return {
    ok: true,
    config: normalizeConfig(response.config ?? null),
  };
}
