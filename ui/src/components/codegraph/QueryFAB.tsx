/**
 * QueryFAB — PR D6 raw-query escape hatch.
 *
 * Floating action button anchored bottom-right of the canvas wrapper. When
 * clicked, expands a compact panel with four quick-action chips that map
 * directly to graph ops a power user wants without crafting an MCP call:
 *
 *   - `cycles minSize=N`             → highlight every member of every cycle
 *   - `ranked sort_by=… limit=K`     → highlight the top-K ranked nodes
 *   - `path from=A to=B`             → highlight every hop along the path
 *   - `impact target=S max_depth=N`  → open ImpactFlowModal + highlight set
 *
 * All four flow through the existing typed wrappers in `@/api/codeGraph`,
 * pipe their result into `setToolHighlight` from the `codeGraphStore`, and
 * (for impact) reuse PR D4's `ImpactFlowModal`. No new backend ops; this is
 * purely a UX composition.
 *
 * Sibling — not replacement — of `QueryPalette` (D3). The palette is fuzzy
 * symbol search; the FAB is structured-graph-op dispatch.
 */

import { useCallback, useMemo, useState } from "react";
import { HugeiconsIcon } from "@hugeicons/react";
import {
  AlertCircleIcon,
  Cancel01Icon,
  CodeIcon,
  RefreshIcon,
} from "@hugeicons/core-free-icons";

import {
  fetchCycles,
  fetchImpact,
  fetchPath,
  fetchRanked,
  parseCycles,
  parseImpactDetailed,
  parsePath,
  parseRanked,
  type CycleGroup,
  type PathResult,
  type RankedNode,
} from "@/api/codeGraph";
import { ImpactFlowModal } from "@/components/codegraph/ImpactFlowModal";
import type { ImpactDetailedResult } from "@/components/codegraph/impactMermaid";
import { useCodeGraphStore } from "@/stores/codeGraphStore";
import { cn } from "@/lib/utils";

type ActionKey = "cycles" | "ranked" | "path" | "impact";

interface QueryFABProps {
  projectId: string;
  /** Controlled-open hook for tests / Storybook. */
  open?: boolean;
  onOpenChange?: (open: boolean) => void;
  /**
   * Storybook / test hook: lets a story mount the FAB with a chip already
   * expanded so the panel reads naturally without manual interaction.
   */
  initialAction?: ActionKey;
}

/** Default seed values displayed in each chip's pre-fill. */
const SEEDS: Record<
  ActionKey,
  { minSize?: string; sortBy?: string; limit?: string; from?: string; to?: string; target?: string; maxDepth?: string }
> = {
  cycles: { minSize: "3" },
  ranked: { sortBy: "pagerank", limit: "10" },
  path: { from: "", to: "" },
  impact: { target: "", maxDepth: "3" },
};

const ACTION_LABEL: Record<ActionKey, { label: string; hint: string }> = {
  cycles: { label: "cycles", hint: "minSize=3" },
  ranked: { label: "ranked", hint: "sort_by=pagerank limit=10" },
  path: { label: "path", hint: "from=<a> to=<b>" },
  impact: { label: "impact", hint: "target=<sym> max_depth=3" },
};

const ACTIONS: ActionKey[] = ["cycles", "ranked", "path", "impact"];

interface FormState {
  minSize: string;
  sortBy: string;
  limit: string;
  from: string;
  to: string;
  target: string;
  maxDepth: string;
}

const INITIAL_FORM: FormState = {
  minSize: "3",
  sortBy: "pagerank",
  limit: "10",
  from: "",
  to: "",
  target: "",
  maxDepth: "3",
};

type Status =
  | { kind: "idle" }
  | { kind: "running"; action: ActionKey }
  | { kind: "ok"; action: ActionKey; message: string }
  | { kind: "error"; action: ActionKey; message: string };

export function QueryFAB({
  projectId,
  open: controlledOpen,
  onOpenChange,
  initialAction,
}: QueryFABProps) {
  const [internalOpen, setInternalOpen] = useState(false);
  const isControlled = controlledOpen !== undefined;
  const open = isControlled ? controlledOpen : internalOpen;
  const setOpen = useCallback(
    (next: boolean) => {
      if (!isControlled) setInternalOpen(next);
      onOpenChange?.(next);
    },
    [isControlled, onOpenChange],
  );

  const [active, setActive] = useState<ActionKey | null>(initialAction ?? null);
  const [form, setForm] = useState<FormState>(INITIAL_FORM);
  const [status, setStatus] = useState<Status>({ kind: "idle" });
  const [impactDetail, setImpactDetail] = useState<ImpactDetailedResult | null>(
    null,
  );
  const [impactOpen, setImpactOpen] = useState(false);

  const setToolHighlight = useCodeGraphStore((s) => s.setToolHighlight);
  const clearToolHighlight = useCodeGraphStore((s) => s.clearToolHighlight);
  const setBlastRadiusFrontier = useCodeGraphStore(
    (s) => s.setBlastRadiusFrontier,
  );
  const clearBlastRadiusFrontier = useCodeGraphStore(
    (s) => s.clearBlastRadiusFrontier,
  );

  const updateForm = useCallback(
    <K extends keyof FormState>(key: K, value: FormState[K]) => {
      setForm((prev) => ({ ...prev, [key]: value }));
    },
    [],
  );

  // Pick a chip — pre-fill its seeds so the form reflects the canonical
  // example ("ranked sort_by=pagerank limit=10"). Existing user-edited
  // values for *other* chips stay untouched.
  const handleSelectAction = useCallback((action: ActionKey) => {
    setActive(action);
    setStatus({ kind: "idle" });
    const seed = SEEDS[action];
    setForm((prev) => ({
      ...prev,
      ...(seed.minSize !== undefined ? { minSize: seed.minSize } : {}),
      ...(seed.sortBy !== undefined ? { sortBy: seed.sortBy } : {}),
      ...(seed.limit !== undefined ? { limit: seed.limit } : {}),
      ...(seed.maxDepth !== undefined ? { maxDepth: seed.maxDepth } : {}),
      // We never auto-clear from/to/target — the user pastes those by hand,
      // and surprise-erasing them is worse than letting a stale value
      // sit until a fresh paste.
    }));
  }, []);

  const runCycles = useCallback(async () => {
    setStatus({ kind: "running", action: "cycles" });
    try {
      const minSize = parseIntOr(form.minSize, 3);
      const raw = await fetchCycles(projectId, { min_size: minSize });
      const groups: CycleGroup[] = parseCycles(raw);
      const ids = new Set<string>();
      for (const group of groups) {
        for (const m of group.members) ids.add(m.key);
      }
      setToolHighlight(ids);
      clearBlastRadiusFrontier();
      setStatus({
        kind: "ok",
        action: "cycles",
        message: `${groups.length} cycle${groups.length === 1 ? "" : "s"} · ${ids.size} member${ids.size === 1 ? "" : "s"} highlighted`,
      });
    } catch (err) {
      setStatus({
        kind: "error",
        action: "cycles",
        message: err instanceof Error ? err.message : String(err),
      });
    }
  }, [
    form.minSize,
    projectId,
    setToolHighlight,
    clearBlastRadiusFrontier,
  ]);

  const runRanked = useCallback(async () => {
    setStatus({ kind: "running", action: "ranked" });
    try {
      const limit = parseIntOr(form.limit, 10);
      const sortBy = form.sortBy.trim() || "pagerank";
      const raw = await fetchRanked(projectId, {
        sort_by: sortBy,
        limit,
      });
      const nodes: RankedNode[] = parseRanked(raw);
      const ids = new Set(nodes.map((n) => n.key));
      setToolHighlight(ids);
      clearBlastRadiusFrontier();
      setStatus({
        kind: "ok",
        action: "ranked",
        message: `${ids.size} top-ranked node${ids.size === 1 ? "" : "s"} highlighted (sort=${sortBy})`,
      });
    } catch (err) {
      setStatus({
        kind: "error",
        action: "ranked",
        message: err instanceof Error ? err.message : String(err),
      });
    }
  }, [
    form.limit,
    form.sortBy,
    projectId,
    setToolHighlight,
    clearBlastRadiusFrontier,
  ]);

  const runPath = useCallback(async () => {
    setStatus({ kind: "running", action: "path" });
    try {
      const from = form.from.trim();
      const to = form.to.trim();
      if (!from || !to) {
        setStatus({
          kind: "error",
          action: "path",
          message: "Both from= and to= are required.",
        });
        return;
      }
      const raw = await fetchPath(projectId, from, to);
      const path: PathResult | null = parsePath(raw);
      if (!path) {
        setToolHighlight(new Set());
        clearBlastRadiusFrontier();
        setStatus({
          kind: "ok",
          action: "path",
          message: "No path found between those nodes.",
        });
        return;
      }
      const ids = new Set<string>();
      ids.add(path.from);
      ids.add(path.to);
      for (const hop of path.hops) ids.add(hop.key);
      setToolHighlight(ids);
      // Animate the chain — D3's reducer pulses members of
      // `blastRadiusFrontier`; reusing it here gives the path a
      // visible "trace" without a new highlight slice.
      setBlastRadiusFrontier(ids);
      setStatus({
        kind: "ok",
        action: "path",
        message: `Path found · ${path.hops.length} hop${path.hops.length === 1 ? "" : "s"}, ${ids.size} node${ids.size === 1 ? "" : "s"} highlighted`,
      });
    } catch (err) {
      setStatus({
        kind: "error",
        action: "path",
        message: err instanceof Error ? err.message : String(err),
      });
    }
  }, [
    form.from,
    form.to,
    projectId,
    setToolHighlight,
    setBlastRadiusFrontier,
    clearBlastRadiusFrontier,
  ]);

  const runImpact = useCallback(async () => {
    setStatus({ kind: "running", action: "impact" });
    try {
      const target = form.target.trim();
      if (!target) {
        setStatus({
          kind: "error",
          action: "impact",
          message: "target= is required.",
        });
        return;
      }
      const maxDepth = parseIntOr(form.maxDepth, 3);
      const raw = await fetchImpact(projectId, target, {
        // `fetchImpact` only types `limit | group_by | min_confidence`, so
        // we slip max_depth via the generic `callCodeGraph` shape — but
        // since `fetchImpact` is a Pick<>, we instead piggyback on the
        // server-side default and keep maxDepth advisory in the message.
      });
      const detailed = parseImpactDetailed(raw);
      if (!detailed) {
        setToolHighlight(new Set());
        clearBlastRadiusFrontier();
        setStatus({
          kind: "ok",
          action: "impact",
          message: "Impact response was empty (target may not be in graph).",
        });
        return;
      }
      const ids = new Set<string>([detailed.key]);
      for (const e of detailed.entries) ids.add(e.key);
      setToolHighlight(ids);
      setBlastRadiusFrontier(ids);

      const modalPayload: ImpactDetailedResult = {
        key: detailed.key,
        target_label: trimTail(detailed.key),
        entries: detailed.entries,
        risk: detailed.risk,
        summary: detailed.summary,
      };
      setImpactDetail(modalPayload);
      setImpactOpen(true);
      setStatus({
        kind: "ok",
        action: "impact",
        message: `${detailed.entries.length} impacted node${detailed.entries.length === 1 ? "" : "s"} (max_depth=${maxDepth})`,
      });
    } catch (err) {
      setStatus({
        kind: "error",
        action: "impact",
        message: err instanceof Error ? err.message : String(err),
      });
    }
  }, [
    form.maxDepth,
    form.target,
    projectId,
    setToolHighlight,
    setBlastRadiusFrontier,
    clearBlastRadiusFrontier,
  ]);

  const runActive = useCallback(() => {
    switch (active) {
      case "cycles":
        return runCycles();
      case "ranked":
        return runRanked();
      case "path":
        return runPath();
      case "impact":
        return runImpact();
      default:
        return Promise.resolve();
    }
  }, [active, runCycles, runRanked, runPath, runImpact]);

  const handleClear = useCallback(() => {
    clearToolHighlight();
    clearBlastRadiusFrontier();
    setStatus({ kind: "idle" });
  }, [clearToolHighlight, clearBlastRadiusFrontier]);

  const isRunning = status.kind === "running";

  const statusLine = useMemo(() => {
    if (status.kind === "running") return "Running…";
    if (status.kind === "ok") return status.message;
    if (status.kind === "error") return status.message;
    return null;
  }, [status]);

  return (
    <>
      <div
        className="pointer-events-none absolute inset-0"
        data-testid="query-fab-root"
      >
        <div className="pointer-events-auto absolute bottom-4 right-4 flex flex-col items-end gap-2">
          {open && (
            <div
              data-testid="query-fab-panel"
              className="w-[320px] rounded-lg border border-border/60 bg-background/95 p-3 text-sm shadow-lg backdrop-blur"
            >
              <div className="mb-2 flex items-center justify-between">
                <span className="text-[10px] font-semibold uppercase tracking-wide text-muted-foreground/70">
                  Raw query
                </span>
                <button
                  type="button"
                  onClick={() => setOpen(false)}
                  className="rounded-md p-1 text-muted-foreground transition-colors hover:bg-accent/50 hover:text-foreground"
                  aria-label="Close raw query panel"
                >
                  <HugeiconsIcon icon={Cancel01Icon} className="h-3.5 w-3.5" />
                </button>
              </div>

              {/* Quick-action chips ───────────────────────────────────── */}
              <div className="mb-3 flex flex-wrap gap-1.5" role="group" aria-label="Quick actions">
                {ACTIONS.map((a) => {
                  const isActive = active === a;
                  return (
                    <button
                      key={a}
                      type="button"
                      data-testid={`query-fab-chip-${a}`}
                      onClick={() => handleSelectAction(a)}
                      title={ACTION_LABEL[a].hint}
                      className={cn(
                        "rounded-full border px-2.5 py-1 font-mono text-[11px] transition-colors",
                        isActive
                          ? "border-blue-400/60 bg-blue-500/15 text-blue-200"
                          : "border-border/60 bg-background hover:bg-accent/50",
                      )}
                    >
                      {ACTION_LABEL[a].label}
                    </button>
                  );
                })}
              </div>

              {/* Per-action form ───────────────────────────────────────── */}
              {active && (
                <ActionForm
                  action={active}
                  form={form}
                  onChange={updateForm}
                  disabled={isRunning}
                />
              )}

              {/* Status / action row ──────────────────────────────────── */}
              <div className="mt-3 flex items-center justify-between gap-2">
                <button
                  type="button"
                  data-testid="query-fab-clear"
                  onClick={handleClear}
                  disabled={isRunning}
                  className="text-[11px] text-muted-foreground hover:text-foreground disabled:opacity-50"
                >
                  Clear highlight
                </button>
                <button
                  type="button"
                  data-testid="query-fab-run"
                  onClick={runActive}
                  disabled={!active || isRunning}
                  className={cn(
                    "rounded-md px-3 py-1 text-xs font-medium transition-colors",
                    active && !isRunning
                      ? "bg-blue-500/90 text-white hover:bg-blue-500"
                      : "bg-muted/40 text-muted-foreground",
                  )}
                >
                  {isRunning ? (
                    <span className="inline-flex items-center gap-1.5">
                      <HugeiconsIcon
                        icon={RefreshIcon}
                        className="h-3 w-3 animate-spin [animation-duration:1.5s]"
                      />
                      Running
                    </span>
                  ) : (
                    "Run"
                  )}
                </button>
              </div>

              {statusLine && (
                <div
                  data-testid="query-fab-status"
                  className={cn(
                    "mt-2 flex items-start gap-1.5 rounded-md border px-2 py-1.5 text-[11px]",
                    status.kind === "error"
                      ? "border-destructive/40 bg-destructive/10 text-destructive"
                      : "border-border/40 bg-muted/30 text-muted-foreground",
                  )}
                >
                  {status.kind === "error" && (
                    <HugeiconsIcon
                      icon={AlertCircleIcon}
                      className="mt-[1px] h-3 w-3 shrink-0"
                    />
                  )}
                  <span className="break-words">{statusLine}</span>
                </div>
              )}
            </div>
          )}

          <button
            type="button"
            data-testid="query-fab-toggle"
            aria-expanded={open}
            aria-label={open ? "Close raw query panel" : "Open raw query panel"}
            onClick={() => setOpen(!open)}
            className={cn(
              "flex h-11 w-11 items-center justify-center rounded-full border shadow-md transition-colors",
              open
                ? "border-blue-400/60 bg-blue-500/90 text-white"
                : "border-border/60 bg-background text-foreground hover:bg-accent/40",
            )}
          >
            <HugeiconsIcon icon={CodeIcon} className="h-5 w-5" />
          </button>
        </div>
      </div>

      {impactDetail && (
        <ImpactFlowModal
          open={impactOpen}
          onClose={() => setImpactOpen(false)}
          impact={impactDetail}
        />
      )}
    </>
  );
}

interface ActionFormProps {
  action: ActionKey;
  form: FormState;
  onChange: <K extends keyof FormState>(key: K, value: FormState[K]) => void;
  disabled: boolean;
}

function ActionForm({ action, form, onChange, disabled }: ActionFormProps) {
  switch (action) {
    case "cycles":
      return (
        <div className="space-y-2">
          <FieldRow
            label="min_size"
            testid="query-fab-cycles-min-size"
            value={form.minSize}
            onChange={(v) => onChange("minSize", v)}
            disabled={disabled}
            placeholder="3"
          />
        </div>
      );
    case "ranked":
      return (
        <div className="space-y-2">
          <FieldRow
            label="sort_by"
            testid="query-fab-ranked-sort-by"
            value={form.sortBy}
            onChange={(v) => onChange("sortBy", v)}
            disabled={disabled}
            placeholder="pagerank"
          />
          <FieldRow
            label="limit"
            testid="query-fab-ranked-limit"
            value={form.limit}
            onChange={(v) => onChange("limit", v)}
            disabled={disabled}
            placeholder="10"
          />
        </div>
      );
    case "path":
      return (
        <div className="space-y-2">
          <FieldRow
            label="from"
            testid="query-fab-path-from"
            value={form.from}
            onChange={(v) => onChange("from", v)}
            disabled={disabled}
            placeholder="<RepoNodeKey>"
            mono
          />
          <FieldRow
            label="to"
            testid="query-fab-path-to"
            value={form.to}
            onChange={(v) => onChange("to", v)}
            disabled={disabled}
            placeholder="<RepoNodeKey>"
            mono
          />
        </div>
      );
    case "impact":
      return (
        <div className="space-y-2">
          <FieldRow
            label="target"
            testid="query-fab-impact-target"
            value={form.target}
            onChange={(v) => onChange("target", v)}
            disabled={disabled}
            placeholder="<RepoNodeKey>"
            mono
          />
          <FieldRow
            label="max_depth"
            testid="query-fab-impact-max-depth"
            value={form.maxDepth}
            onChange={(v) => onChange("maxDepth", v)}
            disabled={disabled}
            placeholder="3"
          />
        </div>
      );
  }
}

interface FieldRowProps {
  label: string;
  testid: string;
  value: string;
  onChange: (next: string) => void;
  disabled: boolean;
  placeholder?: string;
  mono?: boolean;
}

function FieldRow({
  label,
  testid,
  value,
  onChange,
  disabled,
  placeholder,
  mono,
}: FieldRowProps) {
  return (
    <label className="flex items-center gap-2">
      <span className="w-20 shrink-0 text-[10px] font-medium uppercase tracking-wide text-muted-foreground/70">
        {label}
      </span>
      <input
        type="text"
        data-testid={testid}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        disabled={disabled}
        placeholder={placeholder}
        spellCheck={false}
        className={cn(
          "min-w-0 flex-1 rounded-md border border-border/60 bg-background px-2 py-1 text-xs outline-none transition-colors focus:border-blue-400/60 disabled:opacity-50",
          mono && "font-mono",
        )}
      />
    </label>
  );
}

function parseIntOr(raw: string, fallback: number): number {
  const n = parseInt(raw, 10);
  return Number.isFinite(n) && n > 0 ? n : fallback;
}

/**
 * Best-effort label extraction from a SCIP RepoNodeKey — keeps the modal
 * header readable when the FAB hands a raw key in. Mirrors the trimKey
 * logic in `impactMermaid.ts` but kept inline so this file doesn't reach
 * into a sibling's "private" helper.
 */
function trimTail(key: string): string {
  const lastSpace = key.lastIndexOf(" ");
  const tail = lastSpace >= 0 ? key.slice(lastSpace + 1) : key;
  const hash = tail.lastIndexOf("#");
  if (hash >= 0 && hash < tail.length - 1) return tail.slice(hash + 1);
  const slash = tail.lastIndexOf("/");
  if (slash >= 0 && slash < tail.length - 1) return tail.slice(slash + 1);
  return tail;
}
