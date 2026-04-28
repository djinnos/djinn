/**
 * CitationLink — clickable anchor for a `[[file:...]]` / `[[symbol:...]]`
 * citation surfaced inside a chat message (PR D5).
 *
 * On click it:
 *   1. Resolves the citation (file form is local, symbol form hits
 *      `code_graph search`).
 *   2. For an unambiguous resolution, writes to `codeGraphStore` and
 *      navigates to `/code-graph` so the canvas pulses the matching
 *      Sigma node (the pulse animation is owned by D3's reducer).
 *   3. For an ambiguous symbol citation, renders an inline candidate
 *      popover and lets the user pick one.
 *
 * The renderer preserves the model's original text inside the anchor
 * (`[[file:src/foo.rs:42-58]]`) so the chat reads naturally even when
 * the user can't / won't click. We tone the look down to a subtle
 * underlined chip with an icon hint.
 */

import { useEffect, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";

import {
  resolveCitation,
  type CitationResolution,
} from "@/lib/citationResolver";
import type { ParsedCitation } from "@/lib/citationParser";
import { useCodeGraphStore } from "@/stores/codeGraphStore";
import { useSelectedProjectId } from "@/stores/useProjectStore";
import { cn } from "@/lib/utils";
import type { SearchHit } from "@/components/pulse/pulseTypes";

export interface CitationLinkProps {
  citation: ParsedCitation;
  /**
   * Override the destination route. Defaults to `/code-graph`. Tests
   * use this so we don't need a full router setup.
   */
  destination?: string;
}

/**
 * Visible label inside the anchor — mirrors the model output without
 * the surrounding `[[ ]]` so the prose flows. Falls back to the raw
 * token for any shape we don't recognize.
 */
function citationLabel(citation: ParsedCitation): string {
  if (citation.kind === "file") {
    if (citation.startLine === null) return citation.path;
    if (citation.endLine === null || citation.endLine === citation.startLine) {
      return `${citation.path}:${citation.startLine}`;
    }
    return `${citation.path}:${citation.startLine}-${citation.endLine}`;
  }
  return `${citation.symbolKind}:${citation.name}`;
}

export function CitationLink({
  citation,
  destination = "/code-graph",
}: CitationLinkProps) {
  const navigate = useNavigate();
  const setCitations = useCodeGraphStore((s) => s.setCitations);
  const projectId = useSelectedProjectId();

  const [resolving, setResolving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [popover, setPopover] = useState<SearchHit[] | null>(null);
  const popoverRef = useRef<HTMLDivElement | null>(null);

  // Close the popover on outside click — quick approximation of the
  // base-ui dropdown without pulling in another primitive.
  useEffect(() => {
    if (!popover) return;
    const handler = (e: MouseEvent) => {
      if (popoverRef.current && !popoverRef.current.contains(e.target as Node)) {
        setPopover(null);
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [popover]);

  const pin = (nodeId: string) => {
    setCitations([nodeId]);
    setPopover(null);
    if (typeof window !== "undefined" && window.location.pathname === destination) {
      // Already on the canvas — store change is enough to repaint.
      return;
    }
    navigate(destination);
  };

  const handleClick = async (event: React.MouseEvent) => {
    event.preventDefault();
    event.stopPropagation();
    if (resolving) return;
    setError(null);
    setResolving(true);

    try {
      // File citations don't need a project to resolve, but symbol
      // citations do. Surface a friendly error rather than silently
      // doing nothing.
      if (citation.kind === "symbol" && !projectId) {
        setError("Pick a project first.");
        return;
      }

      const resolution: CitationResolution = await resolveCitation(
        citation,
        projectId ?? "",
      );

      switch (resolution.status) {
        case "direct":
          pin(resolution.nodeId);
          return;
        case "ambiguous":
          setPopover(resolution.hits);
          return;
        case "missing":
          setError("No matching symbol in the code graph.");
          return;
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : "Resolution failed.");
    } finally {
      setResolving(false);
    }
  };

  return (
    <span className="relative inline-block">
      <button
        type="button"
        data-cite-kind={citation.kind}
        data-cite-raw={citation.raw}
        onClick={handleClick}
        title={citation.raw}
        aria-label={`Open ${citationLabel(citation)} in code graph`}
        className={cn(
          "code-ref inline-flex items-baseline gap-0.5 rounded-sm px-1 py-px font-mono text-[0.85em] leading-none",
          "border border-blue-400/30 bg-blue-400/10 text-blue-300 transition-colors",
          "hover:border-blue-400/60 hover:bg-blue-400/20 hover:text-blue-200",
          "focus:outline-none focus:ring-2 focus:ring-blue-400/50",
          resolving && "opacity-60",
        )}
      >
        <span aria-hidden className="text-blue-400/70">{citation.kind === "file" ? "F" : "S"}</span>
        <span className="break-all">{citationLabel(citation)}</span>
      </button>
      {error && (
        <span
          role="status"
          className="ml-1 text-xs text-amber-400/80"
          onClick={() => setError(null)}
        >
          {error}
        </span>
      )}
      {popover && (
        <div
          ref={popoverRef}
          role="dialog"
          aria-label="Citation candidates"
          className={cn(
            "absolute left-0 top-full z-50 mt-1 w-80 max-w-[80vw] rounded-md",
            "border border-border bg-popover p-1 shadow-lg",
          )}
        >
          <div className="px-2 py-1.5 text-[11px] uppercase tracking-wide text-muted-foreground">
            Pick a match
          </div>
          <ul className="flex max-h-64 flex-col gap-px overflow-y-auto">
            {popover.map((hit) => (
              <li key={hit.key}>
                <button
                  type="button"
                  onClick={() => pin(hit.key)}
                  className="flex w-full flex-col rounded px-2 py-1.5 text-left text-xs hover:bg-accent"
                >
                  <span className="truncate font-mono text-foreground">
                    {hit.display_name || hit.key}
                  </span>
                  <span className="truncate text-[10px] text-muted-foreground">
                    {hit.kind}
                    {hit.file ? ` · ${hit.file}` : ""}
                    {Number.isFinite(hit.score) ? ` · ${hit.score.toFixed(2)}` : ""}
                  </span>
                </button>
              </li>
            ))}
          </ul>
        </div>
      )}
    </span>
  );
}

// Re-exported so tests / sibling components can render the same
// label without re-implementing the formatting logic.
export const __testing = { citationLabel };
