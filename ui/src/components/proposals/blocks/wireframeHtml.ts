/**
 * Srcdoc builder for the sandboxed `Wireframe` block.
 *
 * Sibling to `sandboxedHtml.ts`: it reuses that module's `sanitizeBlockHtml`
 * (same DOMPurify config), the same restrictive CSP, and the same empty
 * `sandbox=""` — so a wireframe is "sandboxed HTML + wireframe design tokens".
 * On top of the sanitized fragment it injects a `<style>` block defining the
 * `--wf-*` design tokens, a clean (non-sketch) font, the `.wf-*` helper classes,
 * and bare-element auto-theming (h1/h2/h3/p/button/input/a/hr/…).
 *
 * WHY CONCRETE HEX: the frame is a `sandbox=""` opaque-origin iframe, so its CSS
 * CANNOT read the parent app's CSS custom properties (`hsl(var(--foreground))`
 * resolves to nothing inside the frame). Following the `DJINN_MERMAID_THEME`
 * precedent we therefore hardcode the djinn dark-theme tokens as sRGB hex, plus a
 * `prefers-color-scheme: light` fallback (mirroring `sandboxedHtml.ts`). The
 * deferred hand-drawn/Excalifont sketch path is intentionally dropped — a clean
 * system font stack is used.
 */

import { IFRAME_CSP, sanitizeBlockHtml } from "./sandboxedHtml";

/** The wireframe surface presets — fixed content widths (px) + min-height floors. */
export const WIREFRAME_SURFACES = {
  browser: { width: 900, minHeight: 200, radius: 14 },
  desktop: { width: 840, minHeight: 200, radius: 14 },
  mobile: { width: 300, minHeight: 360, radius: 30 },
  popover: { width: 360, minHeight: 120, radius: 16 },
  panel: { width: 420, minHeight: 200, radius: 16 },
} as const;

export type WireframeSurface = keyof typeof WIREFRAME_SURFACES;

export const DEFAULT_WIREFRAME_SURFACE: WireframeSurface = "desktop";

/** Normalize an arbitrary attribute string into a known surface (or default). */
export function resolveWireframeSurface(
  surface: string | undefined | null,
): WireframeSurface {
  const key = (surface ?? "").trim().toLowerCase();
  return key in WIREFRAME_SURFACES
    ? (key as WireframeSurface)
    : DEFAULT_WIREFRAME_SURFACE;
}

/**
 * The `--wf-*` design tokens, resolved to concrete djinn DARK-theme sRGB hex
 * (the iframe can't read parent CSS vars). A `prefers-color-scheme: light`
 * override flips them for light viewers.
 *
 * Values trace to the djinn theme in `globals.css` (oklch) converted to hex,
 * matching the `DJINN_MERMAID_THEME` palette where they overlap:
 *   --wf-ink     ≈ --foreground (near-white)
 *   --wf-muted   ≈ --muted-foreground
 *   --wf-line    ≈ --border (white @ ~10% over the dark surface)
 *   --wf-paper   ≈ --background
 *   --wf-card    ≈ --card (a hair above the page)
 *   --wf-accent  = --primary (djinn violet)
 *   --wf-warn    = --destructive
 */
const WF_TOKENS_DARK = `
  --wf-ink: #ededed;
  --wf-muted: #9f9fa9;
  --wf-line: #34343a;
  --wf-paper: #18181b;
  --wf-card: #1f1f22;
  --wf-accent: #8650f6;
  --wf-accent-fg: #fafafa;
  --wf-accent-soft: rgba(134, 80, 246, 0.14);
  --wf-warn: #e5484d;
  --wf-ok: #8650f6;
  --wf-radius: 9px;
`;

const WF_TOKENS_LIGHT = `
  --wf-ink: #1f1f1d;
  --wf-muted: #6b7280;
  --wf-line: #d4d4d8;
  --wf-paper: #ffffff;
  --wf-card: #f7f7f8;
  --wf-accent: #6d33e6;
  --wf-accent-fg: #ffffff;
  --wf-accent-soft: rgba(109, 51, 230, 0.10);
  --wf-warn: #dc2626;
  --wf-ok: #6d33e6;
  --wf-radius: 9px;
`;

/**
 * The full `<style>` body: tokens + clean font + `.wf-*` helper classes + bare
 * semantic-element auto-theming. Ported from agent-native `blocks.css`'s
 * `.plan-html-frame` rules, with the sketch font/rough overlay machinery
 * removed and scoped under `.wf-root` (the frame body wrapper).
 */
function wireframeStyle(): string {
  return `
:root {${WF_TOKENS_DARK}}
@media (prefers-color-scheme: light) { :root {${WF_TOKENS_LIGHT}} }

html, body { margin: 0; padding: 0; background: transparent; }
* { box-sizing: border-box; min-width: 0; }

.wf-root {
  width: 100%;
  min-height: 100%;
  background: var(--wf-paper);
  color: var(--wf-ink);
  font-family: ui-sans-serif, system-ui, -apple-system, "Segoe UI", Inter, sans-serif;
  font-size: 14px;
  line-height: 1.45;
  overflow-wrap: anywhere;
}

.wf-root h1 { font-size: 22px; font-weight: 700; line-height: 1.15; margin: 0 0 8px; }
.wf-root h2 { font-size: 17px; font-weight: 700; line-height: 1.2; margin: 0 0 6px; }
.wf-root h3 { font-size: 14px; font-weight: 700; margin: 0 0 4px; }
.wf-root p { margin: 0 0 8px; }
.wf-root small, .wf-root .wf-muted { color: var(--wf-muted); font-size: 12.5px; }
.wf-root a { color: var(--wf-accent); text-decoration: none; }
.wf-root hr { border: 0; border-top: 1.4px solid var(--wf-line); margin: 10px 0; }
.wf-root img, .wf-root svg { max-width: 100%; }

.wf-root .wf-icon {
  display: inline-block; width: 1em; height: 1em; flex: 0 0 auto;
  color: currentColor; vertical-align: -0.16em;
}
.wf-root .wf-icon-fallback {
  display: inline-flex; align-items: center; justify-content: center;
  border: 1.2px solid currentColor; border-radius: 999px;
  font-size: 0.72em; font-weight: 700; line-height: 1;
  width: 1.1em; height: 1.1em;
}

.wf-root button, .wf-root .wf-btn {
  display: inline-flex; align-items: center; justify-content: center; gap: 6px;
  font: inherit; font-weight: 700; color: var(--wf-ink);
  background: var(--wf-paper); border: 1.4px solid var(--wf-line);
  border-radius: var(--wf-radius); padding: 7px 14px; cursor: default;
}
.wf-root button.primary, .wf-root .wf-btn.primary, .wf-root [data-primary] {
  background: var(--wf-accent); border-color: var(--wf-accent); color: var(--wf-accent-fg);
}

.wf-root input, .wf-root textarea, .wf-root select {
  font: inherit; color: var(--wf-ink); background: var(--wf-card);
  border: 1.4px solid var(--wf-line); border-radius: var(--wf-radius);
  padding: 8px 10px; width: 100%;
}
.wf-root input[type="checkbox"], .wf-root input[type="radio"] {
  width: 16px; height: 16px; padding: 0; accent-color: var(--wf-accent); flex: 0 0 auto;
}

.wf-root .wf-card, .wf-root .wf-box {
  background: var(--wf-card); border: 1.4px solid var(--wf-line);
  border-radius: var(--wf-radius); padding: 12px;
}
.wf-root .wf-pill, .wf-root .wf-chip {
  display: inline-flex; align-items: center; gap: 5px;
  border: 1.4px solid var(--wf-line); border-radius: 999px;
  padding: 2px 10px; font-size: 12.5px;
}
.wf-root .wf-pill.accent, .wf-root .wf-chip.accent {
  border-color: var(--wf-accent); color: var(--wf-accent); background: var(--wf-accent-soft);
}
`;
}

/**
 * Build the full `srcdoc` for a sandboxed wireframe iframe. The HTML is
 * sanitized (DOMPurify, same config as the html block), wrapped in a `.wf-root`
 * body, and the `--wf-*` token `<style>` is injected ahead of it. The output is
 * intended ONLY for an `<iframe sandbox="" srcdoc={...}>`.
 *
 * The wrapper sets the fixed surface width so the frame keeps its footprint, with
 * the surface's `min-height` as a floor. Robust against malformed input —
 * `sanitizeBlockHtml` never throws and an empty/non-string body yields a calm
 * empty surface rather than an error.
 */
export function buildWireframeSrcDoc(
  html: string | undefined | null,
  surface: WireframeSurface | string | undefined | null,
): string {
  const preset = WIREFRAME_SURFACES[resolveWireframeSurface(surface as string)];
  const body = sanitizeBlockHtml(html);
  return (
    "<!doctype html><html><head>" +
    `<meta http-equiv="Content-Security-Policy" content="${IFRAME_CSP}">` +
    '<meta name="referrer" content="no-referrer">' +
    "<style>" +
    wireframeStyle() +
    `.wf-root{width:${preset.width}px;max-width:100%;min-height:${preset.minHeight}px;}` +
    "</style></head><body>" +
    `<div class="wf-root">${body}</div>` +
    "</body></html>"
  );
}
