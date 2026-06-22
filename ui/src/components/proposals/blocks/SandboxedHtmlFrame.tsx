import { useMemo } from "react";

import { buildIframeSrcDoc, SANDBOX_VALUE } from "./sandboxedHtml";

export interface SandboxedHtmlFrameProps {
  /** Untrusted author/agent HTML (or SVG). Sanitized + framed before render. */
  html: string;
  /** Accessible title for the iframe. */
  title?: string;
  /** Extra classes for the iframe element. */
  className?: string;
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
}: SandboxedHtmlFrameProps) {
  const srcDoc = useMemo(() => buildIframeSrcDoc(html), [html]);

  return (
    <iframe
      title={title}
      srcDoc={srcDoc}
      sandbox={SANDBOX_VALUE}
      referrerPolicy="no-referrer"
      loading="lazy"
      className={
        className ??
        "h-80 max-h-[32rem] w-full rounded-md border bg-background"
      }
    />
  );
}
