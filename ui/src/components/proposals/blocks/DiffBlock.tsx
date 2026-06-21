import { useCallback, useEffect, useMemo, useState } from "react";
import {
  ArrowRight01Icon,
  LayoutTwoColumnIcon,
  LeftToRightListBulletIcon,
  MoreHorizontalIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import { cn } from "@/lib/utils";

import { BlockShell } from "./BlockShell";
import type { BlockProps } from "./types";
import {
  diffStats,
  isDiffLike,
  pairSplitRows,
  parseUnifiedDiff,
  segmentRows,
  splitFilename,
  type DiffMode,
  type DiffRow,
  type DiffRowKind,
  type SplitRow,
} from "./diff";

/**
 * Diff — a GitHub-style before/after diff renderer for proposal specs.
 *
 * The block's children are a git-style unified diff (optional `diff --git`/
 * `---`/`+++`/`@@` headers; body lines prefixed `+`/`-`/` `). The parser
 * reconstructs old + new line numbers, then this component renders either a
 * UNIFIED view (one column, dual line-number gutters + sign gutter) or a SPLIT
 * view (old | new columns with half-empty cells for pure adds/removes). The
 * toggle choice persists in localStorage and syncs across mounted diffs.
 *
 * Long runs of unchanged context collapse into an expandable "Show N unchanged
 * lines" row (keeping ~3 lines of edge context). A `+N −N` stat sits in the
 * header next to the filename (basename / dir split when `filename` is set).
 *
 * Robust + graceful: if the children text is not diff-like (no `+`/`-` lines and
 * no `@@` hunk), it falls back to a plain code `<pre>` so an oddly-authored block
 * never blanks or crashes. The code surface is forced LTR.
 */

const DIFF_MODE_STORAGE_KEY = "djinn:proposal-diff-view-mode";
const DIFF_MODE_STORAGE_EVENT = "djinn:proposal-diff-view-mode-change";

const ROW_BG: Record<DiffRowKind, string> = {
  added: "bg-emerald-500/10",
  removed: "bg-red-500/10",
  context: "",
};

const GUTTER_BG: Record<DiffRowKind, string> = {
  added: "bg-emerald-500/15",
  removed: "bg-red-500/15",
  context: "bg-muted/40",
};

const SIGN_COLOR: Record<DiffRowKind, string> = {
  added: "text-emerald-300",
  removed: "text-red-300",
  context: "text-muted-foreground",
};

const SIGN: Record<DiffRowKind, string> = {
  added: "+",
  removed: "−",
  context: " ",
};

const LINE_NO_CLASS =
  "w-[3.25rem] shrink-0 select-none px-2 py-0 text-right font-mono text-[11px] leading-5 text-muted-foreground/70 tabular-nums";

const CODE_CLASS =
  "block min-w-max flex-1 whitespace-pre px-2 py-0 font-mono text-xs leading-5 text-foreground";

function isDiffMode(value: unknown): value is DiffMode {
  return value === "unified" || value === "split";
}

function readStoredMode(): DiffMode | null {
  if (typeof window === "undefined") return null;
  try {
    const value = window.localStorage?.getItem(DIFF_MODE_STORAGE_KEY);
    return isDiffMode(value) ? value : null;
  } catch {
    return null;
  }
}

/**
 * Persisted, cross-diff-synced view-mode preference. Defaults to "unified";
 * writing the mode mirrors it to localStorage and broadcasts a same-tab event so
 * every mounted diff updates in lock-step (the native `storage` event only fires
 * across tabs).
 */
function usePreferredDiffMode() {
  // Seed straight from storage so the first paint already reflects the saved
  // preference (this is a client-only render path) — no setState-in-effect.
  const [mode, setMode] = useState<DiffMode>(() => readStoredMode() ?? "unified");

  useEffect(() => {
    if (typeof window === "undefined") return;
    const onStorage = (event: StorageEvent) => {
      if (event.key !== DIFF_MODE_STORAGE_KEY) return;
      if (isDiffMode(event.newValue)) setMode(event.newValue);
    };
    const onCustom = (event: Event) => {
      const next = (event as CustomEvent<unknown>).detail;
      if (isDiffMode(next)) setMode(next);
    };
    window.addEventListener("storage", onStorage);
    window.addEventListener(DIFF_MODE_STORAGE_EVENT, onCustom);
    return () => {
      window.removeEventListener("storage", onStorage);
      window.removeEventListener(DIFF_MODE_STORAGE_EVENT, onCustom);
    };
  }, []);

  const setPreferred = useCallback((next: DiffMode) => {
    setMode(next);
    if (typeof window === "undefined") return;
    try {
      window.localStorage?.setItem(DIFF_MODE_STORAGE_KEY, next);
      window.dispatchEvent(
        new CustomEvent<DiffMode>(DIFF_MODE_STORAGE_EVENT, { detail: next }),
      );
    } catch {
      // Storage may be unavailable in private/sandboxed contexts; the local
      // component state still updated.
    }
  }, []);

  return [mode, setPreferred] as const;
}

export function DiffBlock({ attributes, children }: BlockProps) {
  const filename = attributes.filename?.trim() || undefined;
  const lang = attributes.lang?.trim() || undefined;
  const content = typeof children === "string" ? children.replace(/^\n+/, "") : "";

  const [mode, setMode] = usePreferredDiffMode();
  const [expanded, setExpanded] = useState<Set<number>>(() => new Set());

  const diffLike = useMemo(() => isDiffLike(content), [content]);
  const rows = useMemo(
    () => (diffLike ? parseUnifiedDiff(content) : []),
    [content, diffLike],
  );
  const stats = useMemo(() => diffStats(rows), [rows]);
  const fileParts = useMemo(() => splitFilename(filename), [filename]);

  const toggleRun = (key: number) =>
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });

  const meta = (
    <span className="flex min-w-0 items-center gap-2 font-mono">
      {filename ? (
        <span className="flex min-w-0 items-baseline gap-1.5">
          <span className="max-w-[14rem] truncate font-semibold text-foreground">
            {fileParts.basename}
          </span>
          {fileParts.directory && (
            <span className="min-w-0 truncate text-[11px] text-muted-foreground/70">
              {fileParts.directory}
            </span>
          )}
        </span>
      ) : null}
      {lang && !filename ? <span>{lang}</span> : null}
      {diffLike && (rows.length > 0 ? (
        <span className="flex shrink-0 items-center gap-2">
          <span className="text-emerald-400">+{stats.added}</span>
          <span className="text-red-400">−{stats.removed}</span>
        </span>
      ) : null)}
      {diffLike && (
        <span className="ml-auto flex shrink-0 items-center overflow-hidden rounded-md border">
          <ModeButton
            active={mode === "unified"}
            onClick={() => setMode("unified")}
            icon={LeftToRightListBulletIcon}
            label="Unified"
          />
          <ModeButton
            active={mode === "split"}
            onClick={() => setMode("split")}
            icon={LayoutTwoColumnIcon}
            label="Split"
          />
        </span>
      )}
    </span>
  );

  // Not diff-like — fall back to a plain code surface so the authored text is
  // never lost or misrendered.
  if (!diffLike) {
    return (
      <BlockShell
        label="Diff"
        accent="text-sky-400"
        flush
        meta={
          filename || lang ? (
            <span className="truncate font-mono">{filename ?? lang}</span>
          ) : null
        }
      >
        <pre
          dir="ltr"
          className="overflow-x-auto px-4 py-3 font-mono text-xs leading-relaxed text-foreground [unicode-bidi:isolate]"
        >
          {content}
        </pre>
      </BlockShell>
    );
  }

  const noChanges = stats.added === 0 && stats.removed === 0;

  return (
    <BlockShell label="Diff" accent="text-sky-400" flush meta={meta}>
      <div dir="ltr" className="[unicode-bidi:isolate]">
        {noChanges ? (
          <div className="px-4 py-6 text-center font-mono text-sm text-muted-foreground">
            No changes
          </div>
        ) : mode === "split" ? (
          <SplitView rows={rows} lang={lang} />
        ) : (
          <UnifiedView
            rows={rows}
            lang={lang}
            expanded={expanded}
            onToggleRun={toggleRun}
          />
        )}
      </div>
    </BlockShell>
  );
}

function ModeButton({
  active,
  onClick,
  icon,
  label,
}: {
  active: boolean;
  onClick: () => void;
  icon: typeof LeftToRightListBulletIcon;
  label: string;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-pressed={active}
      title={label}
      className={cn(
        "flex cursor-pointer items-center gap-1 px-2 py-1 text-[11px] font-medium transition-colors",
        active
          ? "bg-accent text-accent-foreground"
          : "text-muted-foreground hover:bg-muted/70 hover:text-foreground",
      )}
    >
      <HugeiconsIcon icon={icon} size={13} />
      {label}
    </button>
  );
}

/* ── Unified view ──────────────────────────────────────────────────────────── */

function UnifiedView({
  rows,
  lang,
  expanded,
  onToggleRun,
}: {
  rows: DiffRow[];
  lang?: string;
  expanded: Set<number>;
  onToggleRun: (key: number) => void;
}) {
  void lang;
  const segments = useMemo(() => segmentRows(rows), [rows]);
  let runIndex = 0;
  return (
    <div className="overflow-x-auto py-1">
      <div className="w-max min-w-full">
        {segments.map((segment, idx) => {
          if ("collapsed" in segment) {
            const key = runIndex++;
            const open = expanded.has(key);
            return (
              <div key={`run-${key}`}>
                <CollapsedRow
                  count={segment.rows.length}
                  open={open}
                  onClick={() => onToggleRun(key)}
                />
                {open &&
                  segment.rows.map((row, ri) => (
                    <UnifiedRow key={`run-${key}-${ri}`} row={row} />
                  ))}
              </div>
            );
          }
          return <UnifiedRow key={idx} row={segment} />;
        })}
      </div>
    </div>
  );
}

function UnifiedRow({ row }: { row: DiffRow }) {
  return (
    <div className={cn("flex min-h-5 min-w-full", ROW_BG[row.kind])}>
      <span className={LINE_NO_CLASS}>{row.oldNo ?? ""}</span>
      <span className={LINE_NO_CLASS}>{row.newNo ?? ""}</span>
      <span
        className={cn(
          "w-6 shrink-0 select-none py-0 text-center font-mono text-xs font-semibold leading-5",
          GUTTER_BG[row.kind],
          SIGN_COLOR[row.kind],
        )}
      >
        {SIGN[row.kind]}
      </span>
      <span className={CODE_CLASS}>{row.text || " "}</span>
    </div>
  );
}

function CollapsedRow({
  count,
  open,
  onClick,
}: {
  count: number;
  open: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-expanded={open}
      className="flex w-full cursor-pointer items-center gap-2 border-y bg-muted/40 px-3 py-1 text-left text-[11px] text-muted-foreground transition-colors hover:bg-muted/70 hover:text-foreground"
    >
      <HugeiconsIcon
        icon={open ? ArrowRight01Icon : MoreHorizontalIcon}
        size={13}
        className={cn("shrink-0", open && "rotate-90")}
      />
      <span>
        {open ? "Hide" : "Show"} {count} unchanged line{count === 1 ? "" : "s"}
      </span>
    </button>
  );
}

/* ── Split (side-by-side) view ─────────────────────────────────────────────── */

function SplitView({ rows, lang }: { rows: DiffRow[]; lang?: string }) {
  void lang;
  const pairs = useMemo(() => pairSplitRows(rows), [rows]);
  return (
    <div className="flex w-full py-1">
      <div className="min-w-0 flex-1 overflow-x-auto border-r">
        <div className="inline-block min-w-full">
          {pairs.map((pair, idx) => (
            <SplitCell key={`old-${idx}`} row={pair.left} side="old" />
          ))}
        </div>
      </div>
      <div className="min-w-0 flex-1 overflow-x-auto">
        <div className="inline-block min-w-full">
          {pairs.map((pair, idx) => (
            <SplitCell key={`new-${idx}`} row={pair.right} side="new" />
          ))}
        </div>
      </div>
    </div>
  );
}

function SplitCell({ row, side }: { row?: DiffRow; side: "old" | "new" }) {
  if (!row) {
    return (
      <div className="flex min-h-5 min-w-full bg-muted/20">
        <span className={LINE_NO_CLASS} />
        <span className="w-6 shrink-0 bg-muted/30" />
        <span className={CODE_CLASS}> </span>
      </div>
    );
  }
  const sign = side === "old" ? "−" : "+";
  const showSign = row.kind !== "context";
  const lineNo = side === "old" ? row.oldNo : row.newNo;
  return (
    <div className={cn("flex min-h-5 min-w-full", ROW_BG[row.kind])}>
      <span className={LINE_NO_CLASS}>{lineNo ?? ""}</span>
      <span
        className={cn(
          "w-6 shrink-0 select-none py-0 text-center font-mono text-xs font-semibold leading-5",
          GUTTER_BG[row.kind],
          SIGN_COLOR[row.kind],
        )}
      >
        {showSign ? sign : " "}
      </span>
      <span className={CODE_CLASS}>{row.text || " "}</span>
    </div>
  );
}

/** Re-export used to keep `SplitRow` in the type graph for consumers/tests. */
export type { SplitRow };
