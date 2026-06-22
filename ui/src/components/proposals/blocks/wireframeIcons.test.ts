import { describe, expect, it } from "vitest";

import {
  renderWireframeIconHtml,
  WIREFRAME_ICON_NAMES,
} from "./wireframeIcons";

describe("renderWireframeIconHtml", () => {
  it("replaces a data-icon marker with an inline svg carrying the .wf-icon class", () => {
    const out = renderWireframeIconHtml('<span data-icon="mail"></span>');
    expect(out).toContain("<svg");
    expect(out).toContain('class="wf-icon"');
    expect(out).toContain('data-icon="mail"');
    expect(out).toContain("<path");
    // The marker element itself is gone.
    expect(out).not.toContain("data-icon=\"mail\"></span>");
  });

  it("supports both <span> and self-closing <i> markers", () => {
    expect(renderWireframeIconHtml('<i data-icon="search"></i>')).toContain(
      'data-icon="search"',
    );
    expect(renderWireframeIconHtml('<span data-icon="x" />')).toContain(
      'data-icon="x"',
    );
  });

  it("resolves aliases and separator/case variants to a canonical icon", () => {
    // alias email -> mail
    expect(renderWireframeIconHtml('<span data-icon="email"></span>')).toContain(
      'data-icon="mail"',
    );
    // alias close -> x
    expect(renderWireframeIconHtml('<span data-icon="close"></span>')).toContain(
      'data-icon="x"',
    );
    // separator/case variant chevron-down -> chevronDown
    expect(
      renderWireframeIconHtml('<span data-icon="chevron-down"></span>'),
    ).toContain('data-icon="chevronDown"');
    // `icon`-prefixed
    expect(
      renderWireframeIconHtml('<span data-icon="iconSettings"></span>'),
    ).toContain('data-icon="settings"');
  });

  it("carries an accessible label from aria-label or title; otherwise aria-hidden", () => {
    const labelled = renderWireframeIconHtml(
      '<span data-icon="mail" aria-label="Email"></span>',
    );
    expect(labelled).toContain('role="img"');
    expect(labelled).toContain('aria-label="Email"');

    const bare = renderWireframeIconHtml('<span data-icon="mail"></span>');
    expect(bare).toContain('aria-hidden="true"');
    expect(bare).not.toContain('role="img"');
  });

  it("drops an unknown icon to a safe ? fallback chip (no raw injection)", () => {
    const out = renderWireframeIconHtml(
      '<span data-icon="totally-not-an-icon"></span>',
    );
    expect(out).toContain("wf-icon-fallback");
    expect(out).toContain(">?<");
    // The unknown name is escaped into a data attribute, never an SVG path.
    expect(out).not.toContain("<path");
    expect(out).toContain('data-icon-name="totally-not-an-icon"');
  });

  it("escapes a malicious unknown icon name (no markup injection)", () => {
    const out = renderWireframeIconHtml(
      '<span data-icon="&quot;&gt;<script>alert(1)</script>"></span>',
    );
    // Whatever survives, the fallback must not contain a live script tag.
    expect(out).not.toContain("<script>alert(1)</script>");
  });

  it("leaves non-marker content untouched and never throws", () => {
    const html = '<div class="wf-card"><h1>Sign in</h1><p>hello</p></div>';
    expect(renderWireframeIconHtml(html)).toBe(html);
    expect(renderWireframeIconHtml("")).toBe("");
    expect(renderWireframeIconHtml(undefined)).toBe("");
    expect(renderWireframeIconHtml(null)).toBe("");
  });

  it("replaces multiple markers in one pass", () => {
    const out = renderWireframeIconHtml(
      '<span data-icon="search"></span> q <i data-icon="user"></i>',
    );
    expect(out.match(/<svg/g) ?? []).toHaveLength(2);
    expect(out).toContain('data-icon="search"');
    expect(out).toContain('data-icon="user"');
  });

  it("exports a non-trivial curated icon set", () => {
    expect(WIREFRAME_ICON_NAMES.length).toBeGreaterThanOrEqual(15);
    expect(WIREFRAME_ICON_NAMES).toContain("mail");
    expect(WIREFRAME_ICON_NAMES).toContain("search");
    expect(WIREFRAME_ICON_NAMES).toContain("x");
  });
});
