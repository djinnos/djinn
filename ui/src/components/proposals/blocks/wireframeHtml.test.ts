import { describe, expect, it } from "vitest";

import {
  buildWireframeSrcDoc,
  DEFAULT_WIREFRAME_SURFACE,
  resolveWireframeSurface,
  WIREFRAME_SURFACES,
  type WireframeSurface,
} from "./wireframeHtml";

describe("resolveWireframeSurface", () => {
  it("passes through every known surface", () => {
    for (const key of Object.keys(WIREFRAME_SURFACES) as WireframeSurface[]) {
      expect(resolveWireframeSurface(key)).toBe(key);
    }
  });

  it("is case-insensitive and trims", () => {
    expect(resolveWireframeSurface(" Mobile ")).toBe("mobile");
  });

  it("falls back to the default for unknown/empty input", () => {
    expect(resolveWireframeSurface("nonsense")).toBe(DEFAULT_WIREFRAME_SURFACE);
    expect(resolveWireframeSurface("")).toBe(DEFAULT_WIREFRAME_SURFACE);
    expect(resolveWireframeSurface(undefined)).toBe(DEFAULT_WIREFRAME_SURFACE);
    expect(resolveWireframeSurface(null)).toBe(DEFAULT_WIREFRAME_SURFACE);
  });
});

describe("buildWireframeSrcDoc", () => {
  it("injects the --wf-* design tokens and a light-scheme fallback", () => {
    const doc = buildWireframeSrcDoc("<h1>Hi</h1>", "desktop");
    expect(doc).toContain("--wf-ink:");
    expect(doc).toContain("--wf-paper:");
    expect(doc).toContain("--wf-accent:");
    expect(doc).toContain("--wf-card:");
    expect(doc).toContain("--wf-line:");
    expect(doc).toContain("--wf-muted:");
    expect(doc).toContain("--wf-radius:");
    // concrete djinn dark hex (iframe can't read parent css vars)
    expect(doc).toContain("#8650f6");
    // light-theme fallback like sandboxedHtml.ts
    expect(doc).toContain("prefers-color-scheme: light");
  });

  it("embeds the helper classes and the restrictive CSP", () => {
    const doc = buildWireframeSrcDoc("<div>x</div>", "desktop");
    expect(doc).toContain(".wf-card");
    expect(doc).toContain(".wf-pill");
    expect(doc).toContain(".wf-muted");
    expect(doc).toContain("Content-Security-Policy");
    expect(doc).toContain("script-src 'none'");
  });

  it("maps each surface to its preset width and min-height", () => {
    for (const [key, preset] of Object.entries(WIREFRAME_SURFACES)) {
      const doc = buildWireframeSrcDoc("<p>x</p>", key);
      expect(doc).toContain(`width:${preset.width}px`);
      expect(doc).toContain(`min-height:${preset.minHeight}px`);
    }
  });

  it("wraps the body in a .wf-root and preserves benign content", () => {
    const doc = buildWireframeSrcDoc(
      '<div class="wf-card"><h1>Sign in</h1></div>',
      "browser",
    );
    expect(doc).toContain('<div class="wf-root">');
    expect(doc).toContain('<div class="wf-card">');
    expect(doc).toContain("Sign in");
  });

  it("strips scripts, event handlers, and javascript: URLs", () => {
    const doc = buildWireframeSrcDoc(
      '<b>keep</b><script>alert(1)</script>' +
        '<img src="x" onerror="alert(2)">' +
        '<a href="javascript:alert(3)">x</a>',
      "desktop",
    );
    expect(doc).toContain("<b>keep</b>");
    expect(doc).not.toContain("alert(1)");
    expect(doc).not.toContain("onerror");
    expect(doc).not.toContain("javascript:alert(3)");
  });

  it("does not throw on malformed/empty input and yields a calm empty surface", () => {
    expect(() => buildWireframeSrcDoc("<div><span>", "desktop")).not.toThrow();
    expect(() => buildWireframeSrcDoc("", "desktop")).not.toThrow();
    expect(() => buildWireframeSrcDoc(undefined, "desktop")).not.toThrow();
    expect(() => buildWireframeSrcDoc(null, "desktop")).not.toThrow();
    // unknown surface still produces a valid doc at the default footprint
    const doc = buildWireframeSrcDoc("<p>x</p>", "bogus");
    expect(doc).toContain(
      `width:${WIREFRAME_SURFACES[DEFAULT_WIREFRAME_SURFACE].width}px`,
    );
  });
});
