import { describe, expect, it } from "vitest";

import {
  type EnvironmentConfig,
  normalizeConfig,
  pruneOrphanLanguages,
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
