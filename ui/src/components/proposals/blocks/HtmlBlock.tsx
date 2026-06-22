import { BlockShell } from "./BlockShell";
import { SandboxedHtmlFrame } from "./SandboxedHtmlFrame";
import type { BlockProps } from "./types";

/**
 * Html — renders author/agent-supplied HTML SAFELY.
 *
 * The block CHILDREN are the HTML document/fragment (a string). It is rendered
 * inside a fully sandboxed iframe (`sandbox=""`, restrictive CSP, no scripts, no
 * same-origin) after a DOMPurify pass — see `SandboxedHtmlFrame` /
 * `sandboxedHtml.ts`. This is the safe replacement for the old unsanitized
 * raw-SVG `dangerouslySetInnerHTML` escape hatch.
 *
 * It is NOT a scripting surface: scripts, event handlers, and `javascript:` URLs
 * are stripped at three layers (server validation, DOMPurify, iframe sandbox).
 */
export function HtmlBlock({ children }: BlockProps) {
  const html = typeof children === "string" ? children.trim() : "";

  return (
    <BlockShell label="HTML" accent="text-sky-400">
      <SandboxedHtmlFrame html={html} title="Embedded HTML block" />
    </BlockShell>
  );
}
