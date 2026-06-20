import { describe, expect, it } from "vitest";

import {
  type EnvironmentConfig,
  cargoFeaturesCsv,
  normalizeConfig,
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
