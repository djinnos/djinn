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
export type Distro = "debian" | "alpine";

export interface BaseImage {
  distro: Distro;
  variant: string;
}

export interface RustLanguage {
  default_toolchain: string;
  components?: string[];
  targets?: string[];
}

export interface NodeLanguage {
  default_version: string;
  default_package_manager?: string | null;
  scip_indexer?: string | null;
}

export interface SimpleLanguage {
  default_version: string;
  scip_indexer?: string | null;
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
  slug: string;
  root: string;
  language: string;
  toolchain?: string | null;
  version?: string | null;
  package_manager?: string | null;
}

export interface SystemPackages {
  apt: string[];
  apk: string[];
}

/**
 * A lifecycle / verification / setup command.
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
  pre_warm: HookCommand[];
  pre_task: HookCommand[];
}

export interface VerificationRule {
  match_pattern: string;
  commands: string[];
}

export interface Verification {
  setup: HookCommand[];
  rules: VerificationRule[];
}

export interface EnvironmentConfig {
  schema_version: number;
  source: ConfigSource;
  base: BaseImage;
  languages: Languages;
  workspaces: Workspace[];
  system_packages: SystemPackages;
  env: Record<string, string>;
  lifecycle: LifecycleHooks;
  verification: Verification;
}

/**
 * Normalize a raw JSON blob from the server into a fully-populated
 * `EnvironmentConfig`. Applies the same defaults `EnvironmentConfig::empty()`
 * does on the Rust side so the form bindings never have to branch on
 * `undefined` for required nested shapes.
 */
export function normalizeConfig(
  raw: unknown | null | undefined,
): EnvironmentConfig {
  const obj = (raw && typeof raw === "object" ? raw : {}) as Record<string, unknown>;
  const base = (obj.base ?? {}) as Partial<BaseImage>;
  const system = (obj.system_packages ?? {}) as Partial<SystemPackages>;
  const lifecycle = (obj.lifecycle ?? {}) as Partial<LifecycleHooks>;
  const verification = (obj.verification ?? {}) as Partial<Verification>;
  const env = (obj.env ?? {}) as Record<string, string>;
  return {
    schema_version:
      typeof obj.schema_version === "number" ? obj.schema_version : SCHEMA_VERSION,
    source: (obj.source === "user-edited" ? "user-edited" : "auto-detected") as ConfigSource,
    base: {
      distro: (base.distro === "alpine" ? "alpine" : "debian") as Distro,
      variant: typeof base.variant === "string" && base.variant.length > 0
        ? base.variant
        : "bookworm-slim",
    },
    languages: (obj.languages ?? {}) as Languages,
    workspaces: Array.isArray(obj.workspaces) ? (obj.workspaces as Workspace[]) : [],
    system_packages: {
      apt: Array.isArray(system.apt) ? system.apt : [],
      apk: Array.isArray(system.apk) ? system.apk : [],
    },
    env: { ...env },
    lifecycle: {
      post_build: Array.isArray(lifecycle.post_build) ? lifecycle.post_build : [],
      pre_warm: Array.isArray(lifecycle.pre_warm) ? lifecycle.pre_warm : [],
      pre_task: Array.isArray(lifecycle.pre_task) ? lifecycle.pre_task : [],
    },
    verification: {
      setup: Array.isArray(verification.setup) ? verification.setup : [],
      rules: Array.isArray(verification.rules) ? verification.rules : [],
    },
  };
}

/**
 * Fetch the current environment_config for a project.
 *
 * Returns `null` on a fresh row that the boot reseed hook hasn't touched
 * yet (schema_version is `0` in that case — the Rust
 * `column_default_parses_to_empty_with_schema_version_zero` test is the
 * canonical reference).
 */
export async function fetchEnvironmentConfig(
  projectId: string,
): Promise<{ config: EnvironmentConfig; seeded: boolean }> {
  const response = await callMcpTool("project_environment_config_get", {
    project: projectId,
  });
  if (response.status !== "ok") {
    throw new Error(response.error ?? "Failed to load environment config");
  }
  const raw = (response.config ?? {}) as Record<string, unknown>;
  const seeded = typeof raw.schema_version === "number" && raw.schema_version >= 1;
  return { config: normalizeConfig(raw), seeded };
}

export interface SaveResult {
  ok: boolean;
  error?: string;
}

/**
 * Persist a validated EnvironmentConfig. The server re-validates server-side
 * (shell-injection guards, workspace-slug dedup, etc) before writing.
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
