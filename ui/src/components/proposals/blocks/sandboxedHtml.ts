import DOMPurify, { type Config } from "dompurify";

/**
 * Security primitives for the sandboxed `html` block (and the `Diagram`
 * `type="svg"` path, which reuses this surface).
 *
 * Author/agent-supplied HTML is UNTRUSTED. We never inline it into the live page
 * DOM. Instead we apply defense-in-depth:
 *
 *  1. DOMPurify the markup (strip `<script>`, `on*=` handlers, `javascript:`
 *     URLs) — see {@link sanitizeBlockHtml}.
 *  2. Embed the sanitized markup in a fully locked-down `<iframe srcdoc>` with a
 *     restrictive CSP `<meta>` and an empty `sandbox` attribute (no scripts, no
 *     same-origin) — see {@link buildIframeSrcDoc}.
 *
 * The server-side validation layer (Rust `validation.rs`) is a third backstop
 * that rejects the most dangerous active markup before it is ever stored.
 *
 * Keeping these pure (no React) makes them unit-testable in isolation.
 */

/**
 * The minimal iframe `sandbox` value used for the html/svg render surface.
 *
 * Deliberately the EMPTY STRING: this disables scripts, same-origin access,
 * forms, top-navigation, popups, and pointer-lock — the iframe can only paint
 * static formatted HTML/SVG. `sandbox=""` is the most restrictive possible
 * value; never widen this to include `allow-scripts` + `allow-same-origin`
 * together (that combination lets framed content escape the sandbox).
 */
export const SANDBOX_VALUE = "";

/**
 * Content-Security-Policy embedded as a `<meta http-equiv>` inside the srcdoc
 * document. A second belt on top of the empty sandbox: even if the sandbox were
 * ever loosened, this blocks script execution, framing, form submission, and
 * remote connections while still allowing static formatted HTML/SVG to render
 * (inline styles + http(s)/data images).
 */
export const IFRAME_CSP =
  "default-src 'none'; " +
  "img-src data: https:; " +
  "style-src 'unsafe-inline'; " +
  "font-src data: https:; " +
  "base-uri 'none'; " +
  "form-action 'none'; " +
  "frame-src 'none'; " +
  "object-src 'none'; " +
  "script-src 'none'";

/**
 * DOMPurify configuration shared by the html and svg surfaces. Allows static
 * formatted HTML plus the full SVG drawing profile, and FORBIDS the active /
 * structural elements that could execute or escape. `dompurify` already strips
 * `on*=` handlers and `javascript:`/`data:text/html` URLs by default; the
 * explicit FORBID list is belt-and-suspenders.
 */
const SANITIZE_CONFIG: Config = {
  USE_PROFILES: { html: true, svg: true, svgFilters: true },
  FORBID_TAGS: [
    "script",
    "iframe",
    "object",
    "embed",
    "base",
    "form",
    "meta",
    "link",
    "noscript",
    "frame",
    "frameset",
    "applet",
  ],
  FORBID_ATTR: ["srcdoc"],
  // Never keep `javascript:`/`vbscript:`/`data:text/html` URLs.
  ALLOW_UNKNOWN_PROTOCOLS: false,
};

/**
 * Sanitize untrusted author/agent HTML (or SVG). Strips scripts, event-handler
 * attributes, and dangerous URLs while preserving benign formatting and SVG
 * drawing markup. Returns a string of sanitized HTML.
 *
 * Robust against malformed input — DOMPurify parses leniently and never throws;
 * a non-string or empty input yields an empty string.
 */
export function sanitizeBlockHtml(html: string | undefined | null): string {
  if (typeof html !== "string" || html.length === 0) return "";
  // Without `RETURN_TRUSTED_TYPE`, sanitize returns a plain string; coerce to be
  // explicit (the typed overload can widen to `string | TrustedHTML`).
  return String(DOMPurify.sanitize(html, SANITIZE_CONFIG));
}

/**
 * Build the full `srcdoc` document string for the sandboxed iframe. Sanitizes
 * the body, embeds the restrictive CSP meta, and applies a small reset so the
 * fragment renders legibly (transparent background so the host card shows
 * through; theme-aware ink via `prefers-color-scheme`).
 *
 * The output is intended ONLY for an `<iframe sandbox="" srcdoc={...}>`.
 */
export function buildIframeSrcDoc(html: string | undefined | null): string {
  const body = sanitizeBlockHtml(html);
  return (
    "<!doctype html><html><head>" +
    `<meta http-equiv="Content-Security-Policy" content="${IFRAME_CSP}">` +
    "<meta name=\"referrer\" content=\"no-referrer\">" +
    "<style>" +
    "html,body{margin:0;padding:0;background:transparent;}" +
    "body{font-family:Inter,system-ui,-apple-system,sans-serif;color:#1f1f1d;" +
    "font-size:14px;line-height:1.5;overflow-wrap:anywhere;}" +
    "*{box-sizing:border-box;}" +
    "img,svg,video{max-width:100%;height:auto;}" +
    "a{color:#3b82f6;}" +
    "@media(prefers-color-scheme:dark){body{color:#e8e8e6;}}" +
    "</style></head><body>" +
    body +
    "</body></html>"
  );
}
