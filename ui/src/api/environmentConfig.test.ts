import { describe, expect, it } from "vitest";

import {
  type EnvironmentConfig,
  DEFAULT_PRE_TASK_FAILURE_POLICY,
  DEFAULT_PRE_TASK_TIMEOUT,
  cargoFeaturesCsv,
  normalizeConfig,
  normalizePreTaskCommands,
  pruneOrphanLanguages,
  setCargoFeaturesCsv,
} from "@/api/environmentConfig";

function configWith(
  partial: Partial<EnvironmentConfig>,
): EnvironmentConfig {
  return { ...normalizeConfig({ schema_version: 1, source: "user-edited" }), ...partial };
}

describe("pruneOrphanLanguages", () => {
  it("drops a language no workspace pins (the svc-analytics orphan)", () => {
    const before = configWith({
      languages: { go: { default_version: "1.25.4" }, python: { default_version: "3.12" } },
      workspaces: [{ root: "", language: "go", version: "1.25.4" }],
    });
    const after = pruneOrphanLanguages(before);
    expect(after.languages).toEqual({ go: { default_version: "1.25.4" } });
  });

  it("keeps every language still pinned by a workspace", () => {
    const before = configWith({
      languages: { go: { default_version: "1.25.4" }, python: { default_version: "3.12" } },
      workspaces: [
        { root: "", language: "go", version: "1.25.4" },
        { root: "py/", language: "python", version: "3.12" },
      ],
    });
    expect(pruneOrphanLanguages(before).languages).toEqual(before.languages);
  });

  it("leaves languages untouched when there are no workspaces", () => {
    const before = configWith({
      languages: { python: { default_version: "3.12" } },
      workspaces: [],
    });
    // No workspaces → intent is ambiguous (detection may not have populated
    // workspaces yet), so we must not nuke the detected languages.
    expect(pruneOrphanLanguages(before)).toBe(before);
  });

  it("returns the same reference when nothing is orphaned (no needless re-render)", () => {
    const before = configWith({
      languages: { go: { default_version: "1.25.4" } },
      workspaces: [{ root: "", language: "go", version: "1.25.4" }],
    });
    expect(pruneOrphanLanguages(before)).toBe(before);
  });

  it("preserves unknown language keys (raw-JSON passthrough)", () => {
    const before = configWith({
      languages: {
        go: { default_version: "1.25.4" },
        // a key the form doesn't model — must survive
        zig: { default_version: "0.13" },
      } as EnvironmentConfig["languages"],
      workspaces: [{ root: "", language: "go", version: "1.25.4" }],
    });
    const after = pruneOrphanLanguages(before);
    expect((after.languages as Record<string, unknown>).zig).toEqual({ default_version: "0.13" });
  });
});

describe("normalizeConfig preserves form-untouched fields", () => {
  it("keeps cargo_cache_policy / agent_mcp_defaults / global_skills (no silent drop on save)", () => {
    const cfg = normalizeConfig({
      schema_version: 1,
      cargo_cache_policy: {
        mode: "explicit",
        policy: { features: ["postgres"], sccache: true, incremental: false },
      },
      agent_mcp_defaults: { worker: ["github"] },
      global_skills: ["verify"],
    });
    expect(cfg.cargo_cache_policy).toEqual({
      mode: "explicit",
      // legacy sccache/incremental keys round-trip untouched (server ignores them)
      policy: { features: ["postgres"], sccache: true, incremental: false },
    });
    expect(cfg.agent_mcp_defaults).toEqual({ worker: ["github"] });
    expect(cfg.global_skills).toEqual(["verify"]);
  });

  it("keeps ordered grouped final verification plans when saving another form field", () => {
    const plan = {
      version: 1,
      profile_id: "ci-default",
      profile_revision: 1,
      command_groups: [
        { name: "rust", commands: [{ check_id: "test", executable: "cargo", timeout_seconds: 300 }] },
        { name: "web", commands: [{ check_id: "web-test", executable: "pnpm", timeout_seconds: 300 }] },
      ],
      selection_rules: [
        { match: ["server/**"], command_groups: ["rust"] },
        { match: ["**"], command_groups: ["rust", "web"] },
      ],
    };
    const cfg = normalizeConfig({ schema_version: 1, lifecycle: { final_verification: plan } });
    expect(cfg.lifecycle.final_verification).toEqual(plan);
  });
});

describe("cargo features CSV helpers", () => {
  it("reads an explicit feature override as CSV", () => {
    const cfg = normalizeConfig({
      schema_version: 1,
      cargo_cache_policy: { mode: "explicit", policy: { features: ["ci", "postgres"] } },
    });
    expect(cargoFeaturesCsv(cfg)).toBe("ci, postgres");
  });

  it("reads auto-detected / absent policy as empty", () => {
    expect(cargoFeaturesCsv(normalizeConfig({ schema_version: 1 }))).toBe("");
    expect(
      cargoFeaturesCsv(
        normalizeConfig({ schema_version: 1, cargo_cache_policy: { mode: "auto-detected" } }),
      ),
    ).toBe("");
  });

  it("writes a CSV override into an explicit policy", () => {
    const cfg = normalizeConfig({ schema_version: 1 });
    const next = setCargoFeaturesCsv(cfg, " ci , postgres ,, ");
    expect(next.cargo_cache_policy).toEqual({
      mode: "explicit",
      policy: { features: ["ci", "postgres"] },
    });
  });

  it("clearing features drops cargo_cache_policy when no other override exists", () => {
    const cfg = normalizeConfig({
      schema_version: 1,
      cargo_cache_policy: { mode: "explicit", policy: { features: ["ci"] } },
    });
    const next = setCargoFeaturesCsv(cfg, "");
    expect(next.cargo_cache_policy).toBeUndefined();
  });

  it("clearing features keeps an explicit policy that carries other overrides", () => {
    const cfg = normalizeConfig({
      schema_version: 1,
      cargo_cache_policy: { mode: "explicit", policy: { features: ["ci"], all_features: true } },
    });
    const next = setCargoFeaturesCsv(cfg, "");
    expect(next.cargo_cache_policy).toEqual({
      mode: "explicit",
      policy: { all_features: true },
    });
  });
});

describe("normalizePreTaskCommands", () => {
  it("returns [] for non-array input", () => {
    expect(normalizePreTaskCommands(undefined)).toEqual([]);
    expect(normalizePreTaskCommands(null)).toEqual([]);
    expect(normalizePreTaskCommands("string")).toEqual([]);
    expect(normalizePreTaskCommands(42)).toEqual([]);
  });

  it("returns [] for an empty array", () => {
    expect(normalizePreTaskCommands([])).toEqual([]);
  });

  it("applies defaults for a well-formed PreTaskCommand object with missing optional fields", () => {
    const result = normalizePreTaskCommands([{ command: "pip install -e ." }]);
    expect(result).toEqual([
      {
        command: "pip install -e .",
        timeout_seconds: DEFAULT_PRE_TASK_TIMEOUT,
        failure_policy: DEFAULT_PRE_TASK_FAILURE_POLICY,
      },
    ]);
  });

  it("preserves explicit timeout_seconds and failure_policy", () => {
    const result = normalizePreTaskCommands([
      { command: "echo ok", timeout_seconds: 120, failure_policy: "best_effort" },
    ]);
    expect(result).toEqual([
      { command: "echo ok", timeout_seconds: 120, failure_policy: "best_effort" },
    ]);
  });

  it("preserves the optional name field", () => {
    const result = normalizePreTaskCommands([
      { command: "make setup", name: "setup-step" },
    ]);
    expect(result[0].name).toBe("setup-step");
    expect(result[0].command).toBe("make setup");
  });

  it("wraps a bare string into a PreTaskCommand with defaults", () => {
    const result = normalizePreTaskCommands(["echo hello"]);
    expect(result).toEqual([
      {
        command: "echo hello",
        timeout_seconds: DEFAULT_PRE_TASK_TIMEOUT,
        failure_policy: DEFAULT_PRE_TASK_FAILURE_POLICY,
      },
    ]);
  });

  it("wraps an argv array into a PreTaskCommand by joining with spaces", () => {
    const result = normalizePreTaskCommands([["pip", "install", "-e", "."]]);
    expect(result).toEqual([
      {
        command: "pip install -e .",
        timeout_seconds: DEFAULT_PRE_TASK_TIMEOUT,
        failure_policy: DEFAULT_PRE_TASK_FAILURE_POLICY,
      },
    ]);
  });

  it("normalizes a mix of objects, strings, and arrays", () => {
    const result = normalizePreTaskCommands([
      { command: "cargo build", timeout_seconds: 600 },
      "echo inline",
      ["make", "test"],
    ]);
    expect(result).toHaveLength(3);
    expect(result[0].timeout_seconds).toBe(600);
    expect(result[0].failure_policy).toBe("blocking");
    expect(result[1].command).toBe("echo inline");
    expect(result[2].command).toBe("make test");
  });
});

describe("normalizeConfig pre_task defaults", () => {
  it("defaults pre_task to [] when lifecycle is absent", () => {
    const cfg = normalizeConfig({ schema_version: 1 });
    expect(cfg.lifecycle.pre_task).toEqual([]);
  });

  it("defaults pre_task to [] when lifecycle.pre_task is absent", () => {
    const cfg = normalizeConfig({ schema_version: 1, lifecycle: { post_build: [] } });
    expect(cfg.lifecycle.pre_task).toEqual([]);
  });

  it("normalizes pre_task items through normalizePreTaskCommands", () => {
    const cfg = normalizeConfig({
      schema_version: 1,
      lifecycle: {
        pre_task: [{ command: "setup-db" }],
      },
    });
    expect(cfg.lifecycle.pre_task).toEqual([
      {
        command: "setup-db",
        timeout_seconds: DEFAULT_PRE_TASK_TIMEOUT,
        failure_policy: DEFAULT_PRE_TASK_FAILURE_POLICY,
      },
    ]);
  });

  it("preserves post_build and pre_anything as HookCommand[]", () => {
    const cfg = normalizeConfig({
      schema_version: 1,
      lifecycle: {
        post_build: ["apt-get install -y curl"],
        pre_anything: ["echo warming up"],
        pre_task: [{ command: "make prepare" }],
      },
    });
    expect(cfg.lifecycle.post_build).toEqual(["apt-get install -y curl"]);
    expect(cfg.lifecycle.pre_anything).toEqual(["echo warming up"]);
    expect(cfg.lifecycle.pre_task).toEqual([
      {
        command: "make prepare",
        timeout_seconds: DEFAULT_PRE_TASK_TIMEOUT,
        failure_policy: DEFAULT_PRE_TASK_FAILURE_POLICY,
      },
    ]);
  });
});
