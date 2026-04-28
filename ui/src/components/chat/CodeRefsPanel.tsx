/**
 * CodeRefsPanel — right-rail strip in `/chat` that lists every
 * citation surfaced by the assistant in the current session.
 *
 * Wave-D5 scope: read the active session's messages from the chat
 * store, run them through the citation parser, and render a flat
 * deduplicated list. Each entry is clickable — clicking pins the ref
 * the same way the inline `<CitationLink>` does (delegates to it via
 * shared resolver).
 *
 * Future polish (D6+): group refs by file, surface line ranges,
 * stream the snippet preview from the canonical workspace.
 */

import { useMemo } from "react";

import { useChatStore } from "@/stores/chatStore";
import { extractCitations, type ParsedCitation } from "@/lib/citationParser";
import { CitationLink } from "./CitationLink";

interface CodeRefsPanelProps {
  /**
   * Optional fixed-width class so the chat caller can decide layout.
   * Defaults to a 14rem rail that mirrors the chat session list.
   */
  className?: string;
}

interface DedupedCitation {
  citation: ParsedCitation;
  /** Number of times the same anchor key appears in the message thread. */
  count: number;
}

function citationKey(c: ParsedCitation): string {
  return c.kind === "file"
    ? `file:${c.path}:${c.startLine ?? ""}-${c.endLine ?? ""}`
    : `symbol:${c.symbolKind}:${c.name}`;
}

export function CodeRefsPanel({ className }: CodeRefsPanelProps) {
  const activeSessionId = useChatStore((s) => s.activeSessionId);
  const messages = useChatStore((s) =>
    activeSessionId ? s.messagesBySession[activeSessionId] ?? [] : [],
  );
  const streaming = useChatStore((s) =>
    activeSessionId ? s.streamingBySession[activeSessionId] ?? "" : "",
  );

  const refs = useMemo<DedupedCitation[]>(() => {
    const counts = new Map<string, DedupedCitation>();
    const visit = (text: string) => {
      for (const c of extractCitations(text)) {
        const k = citationKey(c);
        const existing = counts.get(k);
        if (existing) {
          existing.count += 1;
        } else {
          counts.set(k, { citation: c, count: 1 });
        }
      }
    };

    for (const m of messages) {
      if (m.role === "assistant") visit(m.content);
    }
    if (streaming) visit(streaming);

    return Array.from(counts.values());
  }, [messages, streaming]);

  if (refs.length === 0) {
    return (
      <aside
        className={
          className ??
          "hidden w-56 shrink-0 border-l border-border/60 bg-background/30 px-3 py-4 text-xs text-muted-foreground lg:block"
        }
        aria-label="Code references"
      >
        <h3 className="mb-2 text-[10px] font-semibold uppercase tracking-wider text-muted-foreground/80">
          Code refs
        </h3>
        <p className="text-muted-foreground/60">
          Cited code symbols will appear here once the assistant references
          them.
        </p>
      </aside>
    );
  }

  return (
    <aside
      className={
        className ??
        "hidden w-56 shrink-0 border-l border-border/60 bg-background/30 px-3 py-4 lg:block"
      }
      aria-label="Code references"
    >
      <h3 className="mb-2 text-[10px] font-semibold uppercase tracking-wider text-muted-foreground/80">
        Code refs · {refs.length}
      </h3>
      <ul className="flex flex-col gap-1.5">
        {refs.map((entry) => (
          <li key={citationKey(entry.citation)}>
            <CitationLink citation={entry.citation} />
            {entry.count > 1 && (
              <span className="ml-1 text-[10px] text-muted-foreground/70">
                ×{entry.count}
              </span>
            )}
          </li>
        ))}
      </ul>
    </aside>
  );
}
