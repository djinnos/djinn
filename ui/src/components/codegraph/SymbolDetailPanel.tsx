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
 *   3. Focus status and direction controls for the unified DOI model
 *   4. Dependencies (outgoing, excluding containment) bucketed by EdgeCategory
 *   5. Dependents/Impact (incoming, excluding containment) bucketed by EdgeCategory
 */

import { useEffect, useMemo, useState } from "react";
import { HugeiconsIcon } from "@hugeicons/react";
import {
  AlertCircleIcon,
  Cancel01Icon,
  CodeIcon,
  RefreshIcon,
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
import {
  useCodeGraphStore,
  type FocusDirection,
} from "@/stores/codeGraphStore";
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
  const focusAnchorId = useCodeGraphStore((s) => s.focusAnchorId);
  const focusDirection = useCodeGraphStore((s) => s.focusDirection);
  const setFocusAnchor = useCodeGraphStore((s) => s.setFocusAnchor);
  const setFocusDirection = useCodeGraphStore((s) => s.setFocusDirection);
  const setDoiImpact = useCodeGraphStore((s) => s.setDoiImpact);
  const clearDoiImpact = useCodeGraphStore((s) => s.clearDoiImpact);

  const [fetchState, setFetchState] = useState<FetchState>(
    injectedContext ? { status: "ready", context: injectedContext } : { status: "idle" },
  );
  const [impactState, setImpactState] = useState<{
    status: "idle" | "loading" | "ready" | "error";
    count?: number;
  }>({ status: "idle" });

  // ── Fetch context whenever the selection changes ──────────────────────
  useEffect(() => {
    if (injectedContext) {
      // eslint-disable-next-line react-hooks/set-state-in-effect -- async fetch state machine: seed/reset the context state before (or in place of) the network read.
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

  const activeAnchorId = focusAnchorId ?? selectionId;

  useEffect(() => {
    if (injectedContext) {
      clearDoiImpact();
      // eslint-disable-next-line react-hooks/set-state-in-effect -- async impact-fetch state machine: reset before (or in place of) the network read.
      setImpactState({ status: "idle" });
      return;
    }
    if (
      !activeAnchorId ||
      (focusDirection !== "dependents" && focusDirection !== "both")
    ) {
      clearDoiImpact();
      setImpactState({ status: "idle" });
      return;
    }

    let cancelled = false;
    setImpactState({ status: "loading" });
    (async () => {
      try {
        const raw = await fetchImpact(projectId, activeAnchorId, {
          direction: "dependents",
          group_by: "file",
        });
        const groups = parseFileGroups(raw);
        const ids = new Set<string>([activeAnchorId]);
        for (const group of groups) {
          for (const key of group.sample_keys) ids.add(key);
        }
        if (cancelled) return;
        setDoiImpact(ids);
        setImpactState({ status: "ready", count: Math.max(0, ids.size - 1) });
      } catch (err) {
        if (cancelled) return;
        clearDoiImpact();
        setImpactState({ status: "error" });
        onFetchError?.(err instanceof Error ? err.message : String(err));
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [
    activeAnchorId,
    clearDoiImpact,
    focusDirection,
    injectedContext,
    onFetchError,
    projectId,
    setDoiImpact,
  ]);

  const handleClose = () => {
    setSelection(null);
  };

  if (!selectionId) {
    return null;
  }

  return (
    <aside
      data-testid="symbol-detail-panel"
      className="flex h-full w-[360px] shrink-0 flex-col border-l border-[#2d2d3d] bg-[#16161f]/85 backdrop-blur"
    >
      <header className="flex items-center justify-between border-b border-[#2d2d3d] px-4 py-2.5">
        <span className="text-xs font-medium uppercase tracking-wide text-zinc-500">
          Symbol detail
        </span>
        <button
          type="button"
          onClick={handleClose}
          className="rounded-md p-1 text-zinc-400 transition-colors hover:bg-zinc-800/60 hover:text-zinc-100"
          aria-label="Close detail panel"
        >
          <HugeiconsIcon icon={Cancel01Icon} className="h-4 w-4" />
        </button>
      </header>
      <div className="min-h-0 flex-1 overflow-y-auto">
        <PanelBody
          state={fetchState}
          selectionId={selectionId}
          focusAnchorId={focusAnchorId}
          focusDirection={focusDirection}
          impactState={impactState}
          onSetFocusAnchor={setFocusAnchor}
          onSetFocusDirection={setFocusDirection}
        />
      </div>
    </aside>
  );
}

interface PanelBodyProps {
  state: FetchState;
  selectionId: string;
  focusAnchorId: string | null;
  focusDirection: FocusDirection;
  impactState: {
    status: "idle" | "loading" | "ready" | "error";
    count?: number;
  };
  onSetFocusAnchor: (id: string | null) => void;
  onSetFocusDirection: (direction: FocusDirection) => void;
}

function PanelBody({
  state,
  selectionId,
  focusAnchorId,
  focusDirection,
  impactState,
  onSetFocusAnchor,
  onSetFocusDirection,
}: PanelBodyProps) {
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
      <FocusStatus
        selectionId={selectionId}
        focusAnchorId={focusAnchorId}
        direction={focusDirection}
        impactState={impactState}
        onSetFocusAnchor={onSetFocusAnchor}
        onSetFocusDirection={onSetFocusDirection}
      />
      <RelatedSection
        title="Dependents/Impact"
        buckets={withoutContainment(incoming)}
      />
      <RelatedSection title="Dependencies" buckets={withoutContainment(outgoing)} />
    </div>
  );
}

function withoutContainment(
  buckets: Partial<Record<EdgeCategory, RelatedSymbol[]>>,
): Partial<Record<EdgeCategory, RelatedSymbol[]>> {
  const filtered = { ...buckets };
  delete filtered.contains;
  return filtered;
}

interface FocusStatusProps {
  selectionId: string;
  focusAnchorId: string | null;
  direction: FocusDirection;
  impactState: {
    status: "idle" | "loading" | "ready" | "error";
    count?: number;
  };
  onSetFocusAnchor: (id: string | null) => void;
  onSetFocusDirection: (direction: FocusDirection) => void;
}

function FocusStatus({
  selectionId,
  focusAnchorId,
  direction,
  impactState,
  onSetFocusAnchor,
  onSetFocusDirection,
}: FocusStatusProps) {
  const anchoredHere = focusAnchorId === selectionId;
  const activeAnchor = focusAnchorId ?? selectionId;
  const impactText = (() => {
    if (direction === "dependencies") return "Using local dependency edges.";
    if (impactState.status === "loading") return "Loading impact samples…";
    if (impactState.status === "error") return "Impact samples unavailable.";
    if (impactState.status === "ready") {
      return `${impactState.count ?? 0} sampled dependent${impactState.count === 1 ? "" : "s"} merged into DOI focus.`;
    }
    return "Impact samples load when Dependents/Impact is active.";
  })();

  return (
    <section className="space-y-2 rounded-md border border-border/40 bg-muted/20 p-3">
      <div className="flex items-center justify-between gap-3">
        <div>
          <div className="text-[10px] font-medium uppercase tracking-wide text-muted-foreground/70">
            DOI focus
          </div>
          <p className="mt-1 text-xs text-muted-foreground">
            Anchor: {focusAnchorId ? "pinned" : "selected symbol"} · {impactText}
          </p>
        </div>
        <button
          type="button"
          onClick={() => onSetFocusAnchor(anchoredHere ? null : selectionId)}
          className="rounded-md border border-border/60 px-2 py-1 text-[11px] font-medium text-muted-foreground transition-colors hover:bg-accent/50 hover:text-foreground"
        >
          {anchoredHere ? "Unpin" : focusAnchorId ? "Pin here" : "Pin focus"}
        </button>
      </div>
      <div className="flex rounded-md border border-border/60 bg-background/60 p-0.5">
        {FOCUS_DIRECTION_OPTIONS.map(({ id, label, title }) => (
          <button
            key={id}
            type="button"
            onClick={() => onSetFocusDirection(id)}
            title={title}
            className={cn(
              "flex-1 rounded px-2 py-1 text-[11px] font-medium transition-colors",
              direction === id
                ? "bg-accent text-accent-foreground"
                : "text-muted-foreground hover:text-foreground",
            )}
          >
            {label}
          </button>
        ))}
      </div>
      <p className="text-[10px] text-muted-foreground/70">
        Active anchor: {truncatePathLeft(activeAnchor, 42)}
      </p>
    </section>
  );
}

const FOCUS_DIRECTION_OPTIONS: Array<{
  id: FocusDirection;
  label: string;
  title: string;
}> = [
  {
    id: "dependencies",
    label: "Dependencies",
    title: "Downstream: what this symbol uses",
  },
  {
    id: "dependents",
    label: "Dependents/Impact",
    title: "Upstream: what uses this symbol",
  },
  { id: "both", label: "Both", title: "Show both dependency directions" },
];

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
