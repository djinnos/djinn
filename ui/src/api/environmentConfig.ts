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

/**
 * Failure policy for a pre-task command.  Mirrors
 * `djinn_stack::environment::PreTaskFailurePolicy`.
 *
 * * `blocking` (default) — the task run fails if the command fails.
 * * `best_effort` — failures are logged but do not abort the task run.
 */
export type PreTaskFailurePolicy = "blocking" | "best_effort";

export const DEFAULT_PRE_TASK_TIMEOUT = 300;
export const DEFAULT_PRE_TASK_FAILURE_POLICY: PreTaskFailurePolicy = "blocking";

/**
 * A named pre-task command declared in the project environment config.
 *
 * Pre-task commands run in the task-run Pod before the supervisor starts.
 * Each command carries an optional name (auto-generated as `pre_task_N`
 * when omitted), a shell command string, a timeout, and a failure policy.
 *
 * Mirrors `djinn_stack::environment::PreTaskCommand`.
 */
export interface PreTaskCommand {
  /** Optional display/identity name. Auto-generated as `pre_task_N` when omitted. */
  name?: string;
  /** Shell command passed to `/bin/sh -c`. */
  command: string;
  /** Maximum wall-clock seconds the command may run. Default 300 (5 min). */
  timeout_seconds?: number;
  /** What to do when the command exits non-zero. */
  failure_policy?: PreTaskFailurePolicy;
}

/** A command in the canonical final-verification declaration. */
export interface FinalVerificationCommand {
  check_id: string;
  executable: string;
  argv?: string[];
  working_directory?: string;
  environment_names?: string[];
  timeout_seconds: number;
  descriptor_revision?: number;
}

/** Ordered named command subset for path-selected final verification. */
export interface FinalVerificationCommandGroup {
  name: string;
  commands: FinalVerificationCommand[];
}

/** Ordered path matcher selecting named final-verification command groups. */
export interface FinalVerificationSelectionRule {
  match: string[];
  command_groups: string[];
}

/**
 * Mirrors `djinn_stack::environment::FinalVerificationPlan`. The structured
 * form does not edit this plan, but it must retain legacy and grouped plans
 * when another form field is saved.
 */
export interface FinalVerificationPlan {
  version?: number;
  profile_id?: string;
  profile_revision?: number;
  commands?: FinalVerificationCommand[];
  command_groups?: FinalVerificationCommandGroup[];
  selection_rules?: FinalVerificationSelectionRule[];
  required_checks?: string[];
  input_manifest?: Record<string, unknown>;
  read_only_external_inputs?: Array<Record<string, string>>;
  output_only_globs?: string[];
  hermeticity?: Record<string, boolean>;
}

export interface LifecycleHooks {
  post_build: HookCommand[];
  pre_anything: HookCommand[];
  pre_task: PreTaskCommand[];
  /** Preserved verbatim because selection rules are edited through Raw JSON. */
  final_verification?: FinalVerificationPlan;
}

/**
 * Explicit Cargo target-cache policy override. Mirrors
 * `djinn_stack::environment::CargoCachePolicyOverride`. The dead
 * `sccache`/`incremental` knobs were removed once the platform began forcing
 * `CARGO_INCREMENTAL=1` + `RUSTC_WRAPPER=""` on every build pod (PR #874); any
 * such keys still stored on old rows pass through here untouched (the type is
 * intentionally open-ended) and are ignored by the server on read.
 */
export interface CargoCachePolicyOverride {
  workspace?: boolean;
  features?: string[];
  all_features?: boolean;
  warm_commands?: unknown[];
  [key: string]: unknown;
}

/**
 * Per-project Cargo cache policy. Mirrors the tagged
 * `djinn_stack::environment::CargoCachePolicy` enum: `{ mode: "auto-detected" }`
 * (detection-driven, the default) or `{ mode: "explicit", policy: {...} }`.
 */
export type CargoCachePolicy =
  | { mode: "auto-detected" }
  | { mode: "explicit"; policy: CargoCachePolicyOverride };

export interface EnvironmentConfig {
  schema_version: number;
  source: ConfigSource;
  languages: Languages;
  workspaces: Workspace[];
  system_packages: string[];
  env: Record<string, string>;
  lifecycle: LifecycleHooks;
  /**
   * Project-level Cargo cache policy. The structured form only edits the
   * `features` list; the rest of an explicit policy (and any unknown keys)
   * round-trips untouched so the form never silently drops it.
   */
  cargo_cache_policy?: CargoCachePolicy | null;
  /** Preserved verbatim so the structured form never drops them on save. */
  agent_mcp_defaults?: Record<string, string[]>;
  /** Preserved verbatim so the structured form never drops them on save. */
  global_skills?: string[];
}

/**
 * Normalize a raw `pre_task` value into `PreTaskCommand[]`, applying the
 * same serde defaults the Rust `PreTaskCommand` struct carries:
 * `timeout_seconds` → 300, `failure_policy` → "blocking".
 *
 * Tolerates non-object items (legacy `HookCommand` shapes) by wrapping
 * bare strings/arrays as `{ command }`.
 */
export function normalizePreTaskCommands(raw: unknown): PreTaskCommand[] {
  if (!Array.isArray(raw)) return [];
  return raw.map((item) => {
    // Bare string: wrap as a minimal PreTaskCommand.
    if (typeof item === "string") {
      return {
        command: item,
        timeout_seconds: DEFAULT_PRE_TASK_TIMEOUT,
        failure_policy: DEFAULT_PRE_TASK_FAILURE_POLICY,
      };
    }
    // Array (argv form): join into a shell string.
    if (Array.isArray(item)) {
      return {
        command: item.join(" "),
        timeout_seconds: DEFAULT_PRE_TASK_TIMEOUT,
        failure_policy: DEFAULT_PRE_TASK_FAILURE_POLICY,
      };
    }
    // Object: apply defaults for missing fields.
    if (item && typeof item === "object") {
      const obj = item as Record<string, unknown>;
      return {
        ...(obj as unknown as PreTaskCommand),
        timeout_seconds:
          typeof obj.timeout_seconds === "number"
            ? obj.timeout_seconds
            : DEFAULT_PRE_TASK_TIMEOUT,
        failure_policy:
          obj.failure_policy === "best_effort"
            ? "best_effort"
            : DEFAULT_PRE_TASK_FAILURE_POLICY,
      };
    }
    // Fallback: treat as an empty command (server will reject it).
    return {
      command: "",
      timeout_seconds: DEFAULT_PRE_TASK_TIMEOUT,
      failure_policy: DEFAULT_PRE_TASK_FAILURE_POLICY,
    };
  });
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
  const normalized: EnvironmentConfig = {
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
      pre_task: normalizePreTaskCommands(lifecycle.pre_task),
      final_verification:
        lifecycle.final_verification && typeof lifecycle.final_verification === "object"
          ? (lifecycle.final_verification as FinalVerificationPlan)
          : undefined,
    },
  };

  // Preserve fields the structured form doesn't fully edit but must NOT drop
  // on save (the form sends the whole config back). `cargo_cache_policy` is
  // partly edited (the features list); `agent_mcp_defaults` / `global_skills`
  // round-trip verbatim.
  if (obj.cargo_cache_policy != null && typeof obj.cargo_cache_policy === "object") {
    normalized.cargo_cache_policy = obj.cargo_cache_policy as CargoCachePolicy;
  }
  if (obj.agent_mcp_defaults != null && typeof obj.agent_mcp_defaults === "object") {
    normalized.agent_mcp_defaults = obj.agent_mcp_defaults as Record<string, string[]>;
  }
  if (Array.isArray(obj.global_skills)) {
    normalized.global_skills = obj.global_skills as string[];
  }

  return normalized;
}

/**
 * Read the Cargo feature override out of a config as a CSV string for the
 * editor. Empty string means "no explicit feature override" (auto-detect).
 */
export function cargoFeaturesCsv(config: EnvironmentConfig): string {
  const policy = config.cargo_cache_policy;
  if (policy && policy.mode === "explicit" && Array.isArray(policy.policy.features)) {
    return policy.policy.features.join(", ");
  }
  return "";
}

/**
 * Write a CSV Cargo feature override back into a config.
 *
 * Empty (after trimming) clears the override: if the rest of an explicit
 * policy is also empty we drop `cargo_cache_policy` entirely (back to
 * auto-detect); otherwise we keep the explicit policy with an empty feature
 * list. Any other explicit-policy fields (and unknown keys) are preserved.
 */
export function setCargoFeaturesCsv(
  config: EnvironmentConfig,
  csv: string,
): EnvironmentConfig {
  const features = csv
    .split(",")
    .map((f) => f.trim())
    .filter(Boolean);

  const existing =
    config.cargo_cache_policy && config.cargo_cache_policy.mode === "explicit"
      ? config.cargo_cache_policy.policy
      : {};

  if (features.length === 0) {
    const { features: _dropped, ...rest } = existing;
    const hasOtherOverrides = Object.keys(rest).length > 0;
    if (!hasOtherOverrides) {
      const next = { ...config };
      delete next.cargo_cache_policy;
      return next;
    }
    return { ...config, cargo_cache_policy: { mode: "explicit", policy: rest } };
  }

  return {
    ...config,
    cargo_cache_policy: { mode: "explicit", policy: { ...existing, features } },
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
