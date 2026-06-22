import { describe, expect, it } from "vitest";

import {
  buildIframeSrcDoc,
  IFRAME_CSP,
  SANDBOX_VALUE,
  sanitizeBlockHtml,
} from "./sandboxedHtml";

describe("sanitizeBlockHtml", () => {
  it("strips <script> tags", () => {
    const out = sanitizeBlockHtml(
      '<div>ok</div><script>alert(1)</script>',
    );
    expect(out).toContain("<div>ok</div>");
    expect(out.toLowerCase()).not.toContain("<script");
    expect(out).not.toContain("alert(1)");
  });

  it("strips on*= event-handler attributes", () => {
    const out = sanitizeBlockHtml('<img src="x" onerror="alert(1)">');
    expect(out.toLowerCase()).not.toContain("onerror");
    expect(out).not.toContain("alert(1)");
  });

  it("strips javascript: URLs", () => {
    const out = sanitizeBlockHtml('<a href="javascript:alert(1)">x</a>');
    expect(out.toLowerCase()).not.toContain("javascript:");
  });

  it("strips data:text/html URLs", () => {
    const out = sanitizeBlockHtml(
      '<a href="data:text/html,<script>alert(1)</script>">x</a>',
    );
    expect(out.toLowerCase()).not.toContain("data:text/html");
    expect(out.toLowerCase()).not.toContain("<script");
  });

  it("preserves benign formatting markup", () => {
    const out = sanitizeBlockHtml(
      '<div style="padding:8px"><strong>Hi</strong> <em>there</em></div>',
    );
    expect(out).toContain("<strong>");
    expect(out).toContain("<em>");
    expect(out).toContain("Hi");
  });

  it("preserves inline SVG drawing markup", () => {
    const out = sanitizeBlockHtml(
      '<svg viewBox="0 0 10 10"><rect width="10" height="10" fill="red"/></svg>',
    );
    expect(out.toLowerCase()).toContain("<svg");
    expect(out.toLowerCase()).toContain("<rect");
  });

  it("strips <script> inside SVG (onload/script payloads)", () => {
    const out = sanitizeBlockHtml(
      '<svg><script>alert(1)</script><a xlink:href="javascript:alert(1)">x</a></svg>',
    );
    expect(out.toLowerCase()).not.toContain("<script");
    expect(out.toLowerCase()).not.toContain("javascript:");
  });

  it("removes <iframe> nested in the fragment", () => {
    const out = sanitizeBlockHtml('<div>x</div><iframe src="evil"></iframe>');
    expect(out.toLowerCase()).not.toContain("<iframe");
  });

  it("does not throw on malformed / unbalanced HTML", () => {
    expect(() => sanitizeBlockHtml("<div><span>unclosed")).not.toThrow();
    expect(() => sanitizeBlockHtml("</div></div>")).not.toThrow();
    expect(sanitizeBlockHtml("plain text")).toContain("plain text");
  });

  it("returns empty string for empty / nullish input", () => {
    expect(sanitizeBlockHtml("")).toBe("");
    expect(sanitizeBlockHtml(undefined)).toBe("");
    expect(sanitizeBlockHtml(null)).toBe("");
  });
});

describe("buildIframeSrcDoc", () => {
  it("embeds the restrictive CSP meta with script-src 'none'", () => {
    const doc = buildIframeSrcDoc("<div>ok</div>");
    expect(doc).toContain("Content-Security-Policy");
    expect(doc).toContain(IFRAME_CSP);
    expect(doc).toContain("script-src 'none'");
    expect(doc).toContain("default-src 'none'");
  });

  it("embeds the sanitized body (script stripped) not the raw input", () => {
    const doc = buildIframeSrcDoc("<b>keep</b><script>alert(1)</script>");
    expect(doc).toContain("<b>keep</b>");
    expect(doc).not.toContain("alert(1)");
    expect(doc.toLowerCase()).not.toContain("<script>alert");
  });

  it("produces a complete html document", () => {
    const doc = buildIframeSrcDoc("<p>hi</p>");
    expect(doc.startsWith("<!doctype html>")).toBe(true);
    expect(doc).toContain("<body>");
    expect(doc).toContain("</html>");
  });

  it("never throws on malformed input", () => {
    expect(() => buildIframeSrcDoc("<svg><foreignObject>")).not.toThrow();
    expect(() => buildIframeSrcDoc("")).not.toThrow();
  });
});

describe("sandbox constant", () => {
  it("is the empty (fully locked) sandbox value — no scripts, no same-origin", () => {
    expect(SANDBOX_VALUE).toBe("");
    expect(SANDBOX_VALUE).not.toContain("allow-scripts");
    expect(SANDBOX_VALUE).not.toContain("allow-same-origin");
  });
});
