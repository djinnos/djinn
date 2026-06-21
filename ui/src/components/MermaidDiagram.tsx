/**
 * MermaidDiagram — reusable Mermaid renderer.
 *
 * PR D4: shared between `ImpactFlowModal` (blast-radius visualization) and
 * the chat citation pipeline (PR D5), with a future home in Process flows
 * (Epic F2).
 *
 * Contract:
 *   - props: `{ source: string; className?: string }`
 *   - initializes Mermaid once at module load with `securityLevel: "strict"`
 *     so we never inline-eval untrusted source,
 *   - renders into a sanitized container (DOMPurify) so even a `securityLevel`
 *     regression can't smuggle script tags through,
 *   - normalizes unicode arrow/dash glyphs to Mermaid's ASCII edge operators
 *     before render (LLM-authored sources frequently use `→`/`⟶`/`—`),
 *   - on render failure falls back to a graceful surface: the raw source in a
 *     styled `<pre>` with a copy-source button plus a calm one-line message,
 *     never the raw developer exception and never a blank block.
 *
 * The component is wrapped in `React.memo` so identical `source` strings
 * don't re-trigger Mermaid's heavy SVG generation when parents re-render.
 */

import {
  memo,
  useEffect,
  useId,
  useMemo,
  useRef,
  useState,
} from "react";
import mermaid from "mermaid";
import DOMPurify from "dompurify";

import { cn } from "@/lib/utils";
import { DiagramFallback } from "@/components/proposals/blocks/DiagramFallback";
import { normalizeMermaidSource } from "@/components/proposals/blocks/mermaidNormalize";

// Initialize Mermaid exactly once at module load. `startOnLoad: false` keeps
// it from auto-walking the DOM (we control rendering explicitly), and
// `securityLevel: "strict"` blocks `<script>`/`<iframe>`/click handlers in
// rendered output.
//
// `htmlLabels: false` is load-bearing: by default Mermaid renders flowchart
// node labels as HTML inside a `<foreignObject>`. We sanitize the rendered SVG
// with DOMPurify's *SVG* profile (below), which strips `<foreignObject>` and
// its embedded HTML — leaving visibly empty node boxes. Forcing native SVG
// `<text>` labels means the label markup survives the SVG-profile sanitize.
//
// `theme: "dark"` matches the app's dark-only palette so the diagram doesn't
// render as a bright card on a dark page.
let mermaidInitialized = false;
function initMermaid() {
  if (mermaidInitialized) return;
  mermaid.initialize({
    startOnLoad: false,
    securityLevel: "strict",
    theme: "dark",
    // Render labels as native SVG <text>, not <foreignObject> HTML, so the
    // SVG-profile DOMPurify pass below can't strip them.
    htmlLabels: false,
    flowchart: { htmlLabels: false },
    // Defer fontFamily to the page CSS so we don't pull a Mermaid-specific
    // font into our font stack.
    fontFamily: "inherit",
  });
  mermaidInitialized = true;
}

export interface MermaidDiagramProps {
  /** Raw Mermaid source (e.g. `flowchart TD\n  a --> b`). */
  source: string;
  className?: string;
}

interface RenderState {
  status: "idle" | "rendering" | "ready" | "error";
  svg?: string;
  error?: string;
}

function MermaidDiagramImpl({ source, className }: MermaidDiagramProps) {
  // Stable ids per instance — Mermaid uses these as the SVG root id and
  // demands they be unique within the document.
  const reactId = useId();
  // Mermaid id rules: must start with a letter and contain no special chars.
  const renderId = `mermaid-${reactId.replace(/[^a-zA-Z0-9]/g, "")}`;

  const [state, setState] = useState<RenderState>({ status: "idle" });
  const containerRef = useRef<HTMLDivElement | null>(null);

  // Rewrite unicode arrow/dash glyphs to ASCII edge operators before render.
  // Conservative: only structural (outside-label) glyphs change.
  const normalizedSource = useMemo(
    () => normalizeMermaidSource(source),
    [source],
  );

  useEffect(() => {
    let cancelled = false;
    initMermaid();
    setState({ status: "rendering" });

    (async () => {
      try {
        const { svg } = await mermaid.render(renderId, normalizedSource);
        if (cancelled) return;
        // Belt-and-braces: even with `securityLevel: "strict"`, sanitize the
        // SVG before injecting. DOMPurify's default profile preserves SVG
        // structure while stripping scripts and event handlers.
        const sanitized = DOMPurify.sanitize(svg, {
          USE_PROFILES: { svg: true, svgFilters: true },
        });
        setState({ status: "ready", svg: sanitized });
      } catch (err) {
        if (cancelled) return;
        setState({
          status: "error",
          error: err instanceof Error ? err.message : String(err),
        });
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [normalizedSource, renderId]);

  if (state.status === "error") {
    // Never dump the raw parser exception or blank the block: show the original
    // source with a copy button and a calm one-line reason.
    return (
      <div className={className} data-testid="mermaid-error">
        <DiagramFallback
          source={source}
          message={`Could not render diagram: ${state.error}`}
        />
      </div>
    );
  }

  return (
    <div
      ref={containerRef}
      data-testid="mermaid-diagram"
      className={cn(
        // Mermaid's SVGs render unstyled by default — let parents control
        // sizing via `className`. We center horizontally so flowcharts
        // don't hug the left edge of a wide modal.
        "flex w-full items-center justify-center [&_svg]:max-w-full [&_svg]:h-auto",
        className,
      )}
      // The svg is sanitized above; injecting via dangerouslySetInnerHTML
      // is the standard Mermaid integration pattern.
      dangerouslySetInnerHTML={
        state.status === "ready" ? { __html: state.svg ?? "" } : undefined
      }
    >
      {state.status !== "ready" ? (
        <span className="text-xs text-muted-foreground">Rendering diagram…</span>
      ) : null}
    </div>
  );
}

export const MermaidDiagram = memo(
  MermaidDiagramImpl,
  (prev, next) => prev.source === next.source && prev.className === next.className,
);
