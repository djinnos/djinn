import { useState } from "react";
import { Copy01Icon, Tick02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import { cn } from "@/lib/utils";

import type { BlockProps } from "./types";

/**
 * Wireframe — a low-fi mockup of ONE screen, drawn as ASCII / Unicode
 * box-drawing art and rendered as hand-drawn monospace text.
 *
 * The block CHILDREN are the drawing: boxes with `┌─┐│└┘├┤┬┴┼` (or ASCII
 * `.-'|+`), labels inside the boxes, `[x]`/`[ ]` checkboxes, etc. The model
 * declares layout intent SPATIALLY — which it does reliably — and we render that
 * grid verbatim, so there is zero intent-vs-result gap (no HTML/CSS to get wrong,
 * no diagram engine re-interpreting the characters). It is painted in the
 * self-hosted "Monaspace Radon" handwriting monospace at a muted grey so it reads
 * as a deliberate pencil-draft sketch rather than stark output.
 *
 * De-chromed (no card): the drawing IS the figure. A copy button appears on
 * hover. Empty children render a calm placeholder; nothing ever blanks.
 */
export function WireframeBlock({ children }: BlockProps) {
  const ascii = normalizeAscii(typeof children === "string" ? children : "");
  const [copied, setCopied] = useState(false);

  if (!ascii) {
    return <AsciiPre ascii="(empty wireframe)" muted />;
  }

  const copy = () => {
    if (typeof navigator === "undefined" || !navigator.clipboard) return;
    navigator.clipboard
      .writeText(ascii)
      .then(() => {
        setCopied(true);
        setTimeout(() => setCopied(false), 1200);
      })
      .catch(() => {
        /* clipboard unavailable — ignore */
      });
  };

  return (
    <figure className="group relative my-1">
      <AsciiPre ascii={ascii} />
      <button
        type="button"
        onClick={copy}
        title="Copy wireframe"
        aria-label={copied ? "Copied" : "Copy wireframe"}
        className="absolute right-1 top-1 flex items-center gap-1 rounded border bg-background/80 px-1.5 py-0.5 text-[11px] text-muted-foreground opacity-0 backdrop-blur transition-opacity hover:bg-muted/70 hover:text-foreground focus-visible:opacity-100 group-hover:opacity-100"
      >
        <HugeiconsIcon
          icon={copied ? Tick02Icon : Copy01Icon}
          size={12}
          className={copied ? "text-emerald-400" : undefined}
        />
        {copied ? "Copied" : "Copy"}
      </button>
    </figure>
  );
}

/**
 * The wireframe surface: the drawing as monospace `<pre>` in Monaspace Radon at a
 * muted grey. `whitespace-pre` keeps every authored cell; `overflow-x-auto` lets
 * a wide screen scroll rather than wrap (wrapping would shear the boxes).
 */
function AsciiPre({ ascii, muted }: { ascii: string; muted?: boolean }) {
  return (
    <pre
      data-testid="wireframe-ascii"
      style={{
        fontFamily: '"Monaspace Radon", ui-monospace, monospace',
        lineHeight: 1.1,
        // Monaspace's contextual "texture healing" retunes advance widths, which
        // shears the rigid grid that box-drawing relies on. Force strict
        // monospace so `│`/`─` corners tile into continuous borders.
        fontFeatureSettings: '"calt" 0, "liga" 0',
        fontVariantLigatures: "none",
      }}
      className={cn(
        "overflow-x-auto whitespace-pre text-[13px] text-zinc-300",
        muted && "italic text-muted-foreground/60",
      )}
    >
      {ascii}
    </pre>
  );
}

/**
 * Tidy the authored drawing without disturbing its grid: drop leading/trailing
 * blank lines, then strip the COMMON leading indentation (so a drawing indented
 * under an MDX tag isn't pushed off to the right) while preserving the relative
 * spacing that forms the boxes.
 */
function normalizeAscii(raw: string): string {
  const lines = raw.replace(/\r\n?/g, "\n").replace(/\s+$/, "").split("\n");
  while (lines.length > 0 && lines[0]!.trim() === "") lines.shift();
  if (lines.length === 0) return "";

  let common = Infinity;
  for (const line of lines) {
    if (line.trim() === "") continue;
    const indent = line.length - line.trimStart().length;
    if (indent < common) common = indent;
  }
  if (!Number.isFinite(common) || common === 0) return lines.join("\n");
  return lines.map((line) => line.slice(common)).join("\n");
}
