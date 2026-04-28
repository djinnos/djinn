/**
 * SymbolDetailPanel — right-rail "360° view" of the selected node.
 *
 * Opens whenever `selectionId` is set in the highlight store. Fetches
 * via `code_graph context` (PR C1) — the typed wrapper applies the
 * `include_content=false` default because the panel renders metadata
 * + neighbor lists, not a code snippet (D5 will pass `true` for chat
 * citations).
 *
 * Layout sections, top to bottom:
 *
 *   1. Header        — name, kind, file_path:start-end
 *   2. Method meta   — visibility / async / params / return type
 *   3. "Show blast radius" CTA (kicks off `code_graph impact` and
 *      writes the symbol uids back into the highlight store)
 *   4. Incoming bucketed by EdgeCategory
 *   5. Outgoing bucketed by EdgeCategory
 */

import { useEffect, useMemo, useState } from "react";
import { HugeiconsIcon } from "@hugeicons/react";
import {
  AlertCircleIcon,
  Cancel01Icon,
  CodeIcon,
  RefreshIcon,
  Wifi02Icon,
} from "@hugeicons/core-free-icons";

import {
  fetchContext,
  fetchImpact,
  parseFileGroups,
  parseSymbolContext,
  truncatePathLeft,
  type EdgeCategory,
  type RelatedSymbol,
  type SymbolContext,
} from "@/api/codeGraph";
import { useCodeGraphStore } from "@/stores/codeGraphStore";
import { cn } from "@/lib/utils";

interface SymbolDetailPanelProps {
  projectId: string;
  /**
   * Override for tests / Storybook — when provided, the component
   * skips the actual fetch and renders the supplied context. The
   * store-driven path uses `null` so production code goes through
   * the network layer.
   */
  injectedContext?: SymbolContext | null;
  /** Optional: surface fetch errors to a parent toast. */
  onFetchError?: (err: string) => void;
}

type FetchState =
  | { status: "idle" }
  | { status: "loading" }
  | { status: "ready"; context: SymbolContext }
  | { status: "error"; error: string };

const CATEGORY_LABELS: Record<EdgeCategory, string> = {
  calls: "Calls",
  references: "References",
  imports: "Imports",
  contains: "Contains",
  extends: "Extends",
  implements: "Implements",
  type_defines: "Type defines",
  defines: "Defines",
  reads: "Reads",
  writes: "Writes",
};

const CATEGORY_ORDER: EdgeCategory[] = [
  "calls",
  "references",
  "reads",
  "writes",
  "imports",
  "contains",
  "extends",
  "implements",
  "type_defines",
  "defines",
];

export function SymbolDetailPanel({
  projectId,
  injectedContext,
  onFetchError,
}: SymbolDetailPanelProps) {
  const selectionId = useCodeGraphStore((s) => s.selectionId);
  const setSelection = useCodeGraphStore((s) => s.setSelection);
  const setToolHighlight = useCodeGraphStore((s) => s.setToolHighlight);
  const clearToolHighlight = useCodeGraphStore((s) => s.clearToolHighlight);
  const setBlastRadiusFrontier = useCodeGraphStore(
    (s) => s.setBlastRadiusFrontier,
  );
  const clearBlastRadiusFrontier = useCodeGraphStore(
    (s) => s.clearBlastRadiusFrontier,
  );

  const [fetchState, setFetchState] = useState<FetchState>(
    injectedContext ? { status: "ready", context: injectedContext } : { status: "idle" },
  );
  const [impactBusy, setImpactBusy] = useState(false);

  // ── Fetch context whenever the selection changes ──────────────────────
  useEffect(() => {
    if (injectedContext) {
      setFetchState({ status: "ready", context: injectedContext });
      return;
    }
    if (!selectionId) {
      setFetchState({ status: "idle" });
      return;
    }
    let cancelled = false;
    setFetchState({ status: "loading" });
    (async () => {
      try {
        const raw = await fetchContext(projectId, {
          key: selectionId,
          include_content: false,
        });
        if (cancelled) return;
        const parsed = parseSymbolContext(raw);
        if (!parsed) {
          setFetchState({
            status: "error",
            error:
              "Symbol not found in the canonical graph (it may have moved or been removed).",
          });
          return;
        }
        setFetchState({ status: "ready", context: parsed });
      } catch (err) {
        if (cancelled) return;
        const msg = err instanceof Error ? err.message : String(err);
        setFetchState({ status: "error", error: msg });
        onFetchError?.(msg);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [projectId, selectionId, injectedContext, onFetchError]);

  const handleClose = () => {
    setSelection(null);
    clearToolHighlight();
    clearBlastRadiusFrontier();
  };

  const handleShowBlastRadius = async () => {
    if (!selectionId || impactBusy) return;
    setImpactBusy(true);
    try {
      // Default depth is fine for the visual highlight; the chat tool
      // can still call `impact max_depth=N` for a fuller view.
      const raw = await fetchImpact(projectId, selectionId, {
        group_by: "file",
      });
      const groups = parseFileGroups(raw);
      // Collect every sample symbol uid — enough to light up the
      // affected clusters without overwhelming the canvas. The
      // server already truncates per-file samples for us.
      const ids = new Set<string>();
      for (const g of groups) for (const k of g.sample_keys) ids.add(k);
      setToolHighlight(ids);
      setBlastRadiusFrontier(ids);
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      onFetchError?.(msg);
    } finally {
      setImpactBusy(false);
    }
  };

  if (!selectionId) {
    return null;
  }

  return (
    <aside
      data-testid="symbol-detail-panel"
      className="flex h-full w-[360px] shrink-0 flex-col border-l border-border/60 bg-background/60"
    >
      <header className="flex items-center justify-between border-b border-border/60 px-4 py-2.5">
        <span className="text-xs font-medium uppercase tracking-wide text-muted-foreground/70">
          Symbol detail
        </span>
        <button
          type="button"
          onClick={handleClose}
          className="rounded-md p-1 text-muted-foreground transition-colors hover:bg-accent/50 hover:text-foreground"
          aria-label="Close detail panel"
        >
          <HugeiconsIcon icon={Cancel01Icon} className="h-4 w-4" />
        </button>
      </header>
      <div className="min-h-0 flex-1 overflow-y-auto">
        <PanelBody
          state={fetchState}
          onShowBlastRadius={handleShowBlastRadius}
          impactBusy={impactBusy}
        />
      </div>
    </aside>
  );
}

interface PanelBodyProps {
  state: FetchState;
  onShowBlastRadius: () => void;
  impactBusy: boolean;
}

function PanelBody({ state, onShowBlastRadius, impactBusy }: PanelBodyProps) {
  if (state.status === "loading") {
    return (
      <div className="flex flex-col items-center justify-center gap-3 px-4 py-12 text-sm text-muted-foreground">
        <HugeiconsIcon
          icon={RefreshIcon}
          className="h-5 w-5 animate-spin [animation-duration:2s]"
        />
        <span>Loading symbol context…</span>
      </div>
    );
  }
  if (state.status === "error") {
    return (
      <div className="flex flex-col gap-2 px-4 py-6 text-sm">
        <div className="flex items-center gap-2 text-destructive">
          <HugeiconsIcon icon={AlertCircleIcon} className="h-4 w-4" />
          <span className="font-medium">Couldn&apos;t load context</span>
        </div>
        <p className="text-xs text-muted-foreground">{state.error}</p>
      </div>
    );
  }
  if (state.status === "idle") {
    return null;
  }

  const { symbol, incoming, outgoing } = state.context;
  return (
    <div className="flex flex-col gap-4 px-4 py-4">
      <SymbolHeader
        name={symbol.name}
        kind={symbol.kind}
        filePath={symbol.file_path}
        startLine={symbol.start_line}
        endLine={symbol.end_line}
      />
      {symbol.method_metadata && <MethodMetaBlock meta={symbol.method_metadata} />}
      <button
        type="button"
        onClick={onShowBlastRadius}
        disabled={impactBusy}
        className={cn(
          "flex w-full items-center justify-center gap-2 rounded-md border border-border/60 bg-background px-3 py-2 text-xs font-medium transition-colors",
          "hover:bg-accent/50 disabled:opacity-50",
        )}
      >
        <HugeiconsIcon icon={Wifi02Icon} className="h-4 w-4" />
        {impactBusy ? "Computing blast radius…" : "Show blast radius"}
      </button>
      <RelatedSection title="Incoming" buckets={incoming} />
      <RelatedSection title="Outgoing" buckets={outgoing} />
    </div>
  );
}

interface SymbolHeaderProps {
  name: string;
  kind: string;
  filePath: string;
  startLine: number;
  endLine: number;
}

function SymbolHeader({
  name,
  kind,
  filePath,
  startLine,
  endLine,
}: SymbolHeaderProps) {
  return (
    <div className="space-y-1">
      <div className="flex items-center gap-2">
        <HugeiconsIcon
          icon={CodeIcon}
          className="h-4 w-4 text-muted-foreground"
        />
        <h3 className="truncate text-sm font-semibold text-foreground" title={name}>
          {name || "(unnamed)"}
        </h3>
      </div>
      <div className="text-xs text-muted-foreground">
        <span className="rounded-sm bg-muted/40 px-1.5 py-0.5 font-mono text-[10px] uppercase tracking-wide">
          {kind || "symbol"}
        </span>
        {filePath && (
          <span className="ml-2 font-mono">
            {truncatePathLeft(filePath, 36)}:{startLine}
            {endLine > startLine ? `-${endLine}` : ""}
          </span>
        )}
      </div>
    </div>
  );
}

interface MethodMetaBlockProps {
  meta: NonNullable<SymbolContext["symbol"]["method_metadata"]>;
}

function MethodMetaBlock({ meta }: MethodMetaBlockProps) {
  const tags = useMemo(() => {
    const out: string[] = [];
    if (meta.visibility) out.push(meta.visibility);
    if (meta.is_async) out.push("async");
    return out;
  }, [meta.visibility, meta.is_async]);

  if (
    tags.length === 0 &&
    meta.params.length === 0 &&
    !meta.return_type &&
    meta.annotations.length === 0
  ) {
    return null;
  }

  return (
    <section className="space-y-2 rounded-md border border-border/40 bg-muted/20 p-3">
      <div className="flex flex-wrap gap-1">
        {tags.map((t) => (
          <span
            key={t}
            className="rounded-sm bg-muted px-1.5 py-0.5 text-[10px] font-medium uppercase tracking-wide text-muted-foreground"
          >
            {t}
          </span>
        ))}
      </div>
      {meta.params.length > 0 && (
        <div>
          <div className="text-[10px] font-medium uppercase tracking-wide text-muted-foreground/70">
            Parameters
          </div>
          <ul className="mt-1 space-y-0.5 text-xs font-mono text-foreground/90">
            {meta.params.map((p) => (
              <li key={p.name} className="truncate">
                <span className="text-foreground">{p.name}</span>
                {p.type_name && (
                  <span className="text-muted-foreground">: {p.type_name}</span>
                )}
                {p.default_value && (
                  <span className="text-muted-foreground/70"> = {p.default_value}</span>
                )}
              </li>
            ))}
          </ul>
        </div>
      )}
      {meta.return_type && (
        <div className="text-xs font-mono">
          <span className="text-[10px] font-medium uppercase tracking-wide text-muted-foreground/70">
            Returns
          </span>
          <div className="mt-0.5 truncate text-foreground/90">
            {meta.return_type}
          </div>
        </div>
      )}
      {meta.annotations.length > 0 && (
        <div className="text-xs">
          <div className="text-[10px] font-medium uppercase tracking-wide text-muted-foreground/70">
            Annotations
          </div>
          <div className="mt-0.5 flex flex-wrap gap-1 font-mono">
            {meta.annotations.map((a) => (
              <span
                key={a}
                className="rounded-sm bg-muted/60 px-1.5 py-0.5 text-[10px]"
              >
                {a}
              </span>
            ))}
          </div>
        </div>
      )}
    </section>
  );
}

interface RelatedSectionProps {
  title: string;
  buckets: Partial<Record<EdgeCategory, RelatedSymbol[]>>;
}

function RelatedSection({ title, buckets }: RelatedSectionProps) {
  const setSelection = useCodeGraphStore((s) => s.setSelection);

  const populated = CATEGORY_ORDER.filter((cat) => {
    const list = buckets[cat];
    return list && list.length > 0;
  });

  if (populated.length === 0) {
    return (
      <section className="space-y-1">
        <h4 className="text-[10px] font-medium uppercase tracking-wide text-muted-foreground/70">
          {title}
        </h4>
        <p className="text-xs italic text-muted-foreground/60">No edges.</p>
      </section>
    );
  }

  return (
    <section className="space-y-2">
      <h4 className="text-[10px] font-medium uppercase tracking-wide text-muted-foreground/70">
        {title}
      </h4>
      <div className="space-y-2">
        {populated.map((cat) => (
          <div key={cat} className="space-y-0.5">
            <div className="text-[10px] font-medium uppercase tracking-wider text-muted-foreground">
              {CATEGORY_LABELS[cat]}
              <span className="ml-1 text-muted-foreground/50">
                ({buckets[cat]!.length})
              </span>
            </div>
            <ul className="space-y-px">
              {buckets[cat]!.slice(0, 12).map((rel) => (
                <li key={rel.uid}>
                  <button
                    type="button"
                    onClick={() => setSelection(rel.uid)}
                    className="group block w-full truncate rounded-sm px-1.5 py-0.5 text-left text-xs transition-colors hover:bg-accent/40"
                    title={rel.uid}
                  >
                    <span className="font-mono text-foreground/90">
                      {rel.name || rel.uid}
                    </span>
                    {rel.file_path && (
                      <span className="ml-2 text-[10px] text-muted-foreground">
                        {truncatePathLeft(rel.file_path, 28)}
                      </span>
                    )}
                  </button>
                </li>
              ))}
              {buckets[cat]!.length > 12 && (
                <li className="px-1.5 text-[10px] italic text-muted-foreground/60">
                  + {buckets[cat]!.length - 12} more
                </li>
              )}
            </ul>
          </div>
        ))}
      </div>
    </section>
  );
}
