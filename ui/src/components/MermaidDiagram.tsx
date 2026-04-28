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
 *   - falls back to a `<pre>` block with the raw source on render error.
 *
 * The component is wrapped in `React.memo` so identical `source` strings
 * don't re-trigger Mermaid's heavy SVG generation when parents re-render.
 */

import {
  memo,
  useEffect,
  useId,
  useRef,
  useState,
} from "react";
import mermaid from "mermaid";
import DOMPurify from "dompurify";

import { cn } from "@/lib/utils";

// Initialize Mermaid exactly once at module load. `startOnLoad: false` keeps
// it from auto-walking the DOM (we control rendering explicitly), and
// `securityLevel: "strict"` blocks `<script>`/`<iframe>`/click handlers in
// rendered output. The default `theme` follows our app's light/dark token
// scheme well enough for now.
let mermaidInitialized = false;
function initMermaid() {
  if (mermaidInitialized) return;
  mermaid.initialize({
    startOnLoad: false,
    securityLevel: "strict",
    theme: "default",
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

  useEffect(() => {
    let cancelled = false;
    initMermaid();
    setState({ status: "rendering" });

    (async () => {
      try {
        const { svg } = await mermaid.render(renderId, source);
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
  }, [source, renderId]);

  if (state.status === "error") {
    return (
      <div
        className={cn(
          "rounded-md border border-destructive/30 bg-destructive/5 p-3 text-xs",
          className,
        )}
        data-testid="mermaid-error"
      >
        <p className="mb-2 font-medium text-destructive">
          Mermaid render failed: {state.error}
        </p>
        <pre className="overflow-auto whitespace-pre-wrap font-mono text-[11px] text-muted-foreground">
          {source}
        </pre>
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
