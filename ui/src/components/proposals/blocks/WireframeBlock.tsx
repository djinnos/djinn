import { useMemo } from "react";

import { ZoomPanSurface } from "@/components/ZoomPanSurface";
import { SandboxedHtmlFrame } from "./SandboxedHtmlFrame";
import {
  buildWireframeSrcDoc,
  resolveWireframeSurface,
  WIREFRAME_SURFACES,
} from "./wireframeHtml";
import { renderWireframeIconHtml } from "./wireframeIcons";
import type { BlockProps } from "./types";

/**
 * Wireframe — a low-fi HTML mockup of ONE screen, rendered SAFELY.
 *
 * The block CHILDREN are a self-contained HTML fragment of one screen. They are
 * run through the inline-icon transform (`renderWireframeIconHtml`, the
 * `data-icon` marker → SVG pass), then sanitized + wrapped in the `--wf-*` design
 * token srcdoc (`buildWireframeSrcDoc`) and painted in a fully sandboxed iframe
 * (`SandboxedHtmlFrame`, `sandbox=""`, restrictive CSP). The framed surface keeps
 * its `surface` preset width and sits inside a `ZoomPanSurface` so the reviewer
 * can zoom/pan/fullscreen a dense mockup.
 *
 * It is NOT a scripting surface — scripts, event handlers, and
 * `javascript:` URLs are stripped at three layers (server validation, DOMPurify,
 * iframe sandbox).
 *
 * Graceful: empty/invalid children render a calm fallback frame, never throw.
 */
export function WireframeBlock({ children, attributes }: BlockProps) {
  const rawHtml = typeof children === "string" ? children.trim() : "";
  const surface = resolveWireframeSurface(attributes?.surface);
  const preset = WIREFRAME_SURFACES[surface];

  // Icons run BEFORE sanitize (markers → inline SVG, which the SVG-profile
  // sanitizer keeps); the result is then sanitized + framed by the srcdoc
  // builder. Memoized so re-renders don't re-run the string transform.
  const srcDoc = useMemo(
    () => buildWireframeSrcDoc(renderWireframeIconHtml(rawHtml), surface),
    [rawHtml, surface],
  );

  // De-chromed: no outer "WIREFRAME" grey card. The mockup's own device frame
  // (browser / mobile / panel chrome from the srcdoc) IS the intended container,
  // so the surface sits straight in the document.
  return (
    <ZoomPanSurface
      allowFullscreen
      framedContent
      testIdPrefix="wireframe"
      fullscreenTitle="Wireframe"
    >
      <SandboxedHtmlFrame
        html={rawHtml}
        srcDoc={srcDoc}
        title="Wireframe mockup"
        // Fixed surface-preset width (centered); the surface min-height is the
        // floor so a short screen still reads as that surface. The iframe is
        // opaque-origin so it can't self-size — a max-height caps very tall
        // mockups and the inner srcdoc scrolls.
        className="mx-auto block max-h-[60vh] rounded-md border bg-background"
        style={{
          width: preset.width,
          maxWidth: "100%",
          height: preset.minHeight,
        }}
      />
    </ZoomPanSurface>
  );
}

// Re-export the surface preset map + type so callers/tests can read footprints
// without reaching into the html builder module.
export { WIREFRAME_SURFACES } from "./wireframeHtml";
export type { WireframeSurface } from "./wireframeHtml";
