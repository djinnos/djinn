import { describe, expect, it } from "vitest";

import {
  CODE_LANGUAGE_OPTIONS,
  DEFAULT_CODE_MAX_LINES,
  computeCollapse,
  inferCodeLanguageFromFilename,
  normalizeCodeLanguage,
  resolveCodeLanguage,
  splitCodeFilename,
} from "./code";

describe("splitCodeFilename", () => {
  it("splits directory and basename", () => {
    expect(splitCodeFilename("src/server/auth.ts")).toEqual({
      directory: "src/server",
      basename: "auth.ts",
    });
  });

  it("treats a bare filename as having no directory", () => {
    expect(splitCodeFilename("auth.ts")).toEqual({
      directory: null,
      basename: "auth.ts",
    });
  });

  it("trims and tolerates empty / undefined", () => {
    expect(splitCodeFilename("  ")).toEqual({ directory: null, basename: "" });
    expect(splitCodeFilename(undefined)).toEqual({
      directory: null,
      basename: "",
    });
  });

  it("ignores leading and trailing slashes", () => {
    expect(splitCodeFilename("/a/b/c.rs")).toEqual({
      directory: "a/b",
      basename: "c.rs",
    });
  });
});

describe("inferCodeLanguageFromFilename", () => {
  it("infers from common extensions", () => {
    expect(inferCodeLanguageFromFilename("a/b/main.rs")).toBe("rust");
    expect(inferCodeLanguageFromFilename("auth.ts")).toBe("typescript");
    expect(inferCodeLanguageFromFilename("view.tsx")).toBe("tsx");
    expect(inferCodeLanguageFromFilename("config.yml")).toBe("yaml");
    expect(inferCodeLanguageFromFilename("script.py")).toBe("python");
  });

  it("recognizes extension-less special filenames", () => {
    expect(inferCodeLanguageFromFilename("Dockerfile")).toBe("bash");
    expect(inferCodeLanguageFromFilename("path/to/Makefile")).toBe("makefile");
  });

  it("returns null for unknown / missing", () => {
    expect(inferCodeLanguageFromFilename("notes")).toBeNull();
    expect(inferCodeLanguageFromFilename("data.weirdext")).toBeNull();
    expect(inferCodeLanguageFromFilename(undefined)).toBeNull();
    expect(inferCodeLanguageFromFilename("")).toBeNull();
  });
});

describe("normalizeCodeLanguage", () => {
  it("lowercases and strips the language- prefix", () => {
    expect(normalizeCodeLanguage("TypeScript")).toBe("typescript");
    expect(normalizeCodeLanguage("language-rust")).toBe("rust");
    expect(normalizeCodeLanguage("  Go  ")).toBe("go");
  });

  it("returns null for empty / undefined", () => {
    expect(normalizeCodeLanguage("")).toBeNull();
    expect(normalizeCodeLanguage("   ")).toBeNull();
    expect(normalizeCodeLanguage(undefined)).toBeNull();
  });
});

describe("resolveCodeLanguage", () => {
  it("prefers an explicit lang over the filename", () => {
    expect(resolveCodeLanguage("bash", "main.rs")).toBe("bash");
  });

  it("falls back to filename inference when lang is missing", () => {
    expect(resolveCodeLanguage(undefined, "main.rs")).toBe("rust");
    expect(resolveCodeLanguage("", "auth.ts")).toBe("typescript");
  });

  it("returns null when nothing resolves", () => {
    expect(resolveCodeLanguage(undefined, "notes")).toBeNull();
    expect(resolveCodeLanguage(undefined, undefined)).toBeNull();
  });
});

describe("computeCollapse", () => {
  const lines = (n: number) =>
    Array.from({ length: n }, (_, i) => `line ${i}`).join("\n");

  it("does not collapse short snippets under the default cap", () => {
    const state = computeCollapse(lines(10), false);
    expect(state.collapsible).toBe(false);
    expect(state.collapsed).toBe(false);
    expect(state.hiddenLines).toBe(0);
    expect(state.lineCount).toBe(10);
  });

  it("collapses snippets over the default cap", () => {
    const state = computeCollapse(lines(DEFAULT_CODE_MAX_LINES + 5), false);
    expect(state.collapsible).toBe(true);
    expect(state.collapsed).toBe(true);
    expect(state.hiddenLines).toBe(5);
  });

  it("expands when expanded=true", () => {
    const state = computeCollapse(lines(DEFAULT_CODE_MAX_LINES + 5), true);
    expect(state.collapsible).toBe(true);
    expect(state.collapsed).toBe(false);
  });

  it("honors an explicit maxLines cap", () => {
    const state = computeCollapse(lines(20), false, 5);
    expect(state.collapsible).toBe(true);
    expect(state.hiddenLines).toBe(15);
  });

  it("never collapses when maxLines is 0", () => {
    const state = computeCollapse(lines(1000), false, 0);
    expect(state.collapsible).toBe(false);
    expect(state.collapsed).toBe(false);
    expect(state.hiddenLines).toBe(0);
  });

  it("reports zero lines for an empty snippet", () => {
    expect(computeCollapse("", false).lineCount).toBe(0);
  });
});

describe("CODE_LANGUAGE_OPTIONS", () => {
  it("starts with the Auto sentinel", () => {
    expect(CODE_LANGUAGE_OPTIONS[0]).toEqual({ value: "", label: "Auto" });
  });

  it("has unique values", () => {
    const values = CODE_LANGUAGE_OPTIONS.map((o) => o.value);
    expect(new Set(values).size).toBe(values.length);
  });
});
