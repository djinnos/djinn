import { useMemo } from "react";

import { buildIframeSrcDoc, SANDBOX_VALUE } from "./sandboxedHtml";

export interface SandboxedHtmlFrameProps {
  /** Untrusted author/agent HTML (or SVG). Sanitized + framed before render. */
  html: string;
  /** Accessible title for the iframe. */
  title?: string;
  /** Extra classes for the iframe element. */
  className?: string;
  /**
   * Pre-built `srcdoc` document. When supplied it is used verbatim INSTEAD of
   * `buildIframeSrcDoc(html)`, letting a caller inject a specialized document
   * (e.g. the `Wireframe` block's `--wf-*` token srcdoc) while keeping this the
   * single sandboxed-iframe component. The caller is responsible for sanitizing
   * the body inside its builder — the `Wireframe` srcdoc reuses the same
   * `sanitizeBlockHtml` + CSP + `sandbox=""` security as the default path.
   */
  srcDoc?: string;
  /** Inline styles for the iframe (e.g. a fixed surface-preset width/height). */
  style?: React.CSSProperties;
}

/**
 * Render untrusted HTML inside a fully locked-down sandboxed iframe.
 *
 * Security (defense-in-depth, see `sandboxedHtml.ts`):
 *   - the HTML is DOMPurify-sanitized before it ever reaches the document,
 *   - it is embedded in a `srcdoc` carrying a restrictive CSP `<meta>`,
 *   - the iframe `sandbox=""` disables scripts AND same-origin — framed content
 *     can only paint static formatted HTML/SVG; it cannot run code, navigate the
 *     top window, submit forms, or reach the parent DOM.
 *
 * Because `sandbox=""` makes the framed document an opaque origin, the parent
 * CANNOT read its layout height to auto-size (any such read throws a
 * cross-origin error, and we must not add `allow-same-origin`+`allow-scripts`).
 * We therefore use a sensible fixed viewport with internal scrolling — a bounded
 * surface that never lets the framed content drive host layout.
 */
export function SandboxedHtmlFrame({
  html,
  title = "Embedded HTML",
  className,
  srcDoc: srcDocOverride,
  style,
}: SandboxedHtmlFrameProps) {
  const builtSrcDoc = useMemo(() => buildIframeSrcDoc(html), [html]);
  const srcDoc = srcDocOverride ?? builtSrcDoc;

  return (
    <iframe
      title={title}
      srcDoc={srcDoc}
      sandbox={SANDBOX_VALUE}
      referrerPolicy="no-referrer"
      loading="lazy"
      style={style}
      className={
        className ??
        "h-80 max-h-[32rem] w-full rounded-md border bg-background"
      }
    />
  );
}
