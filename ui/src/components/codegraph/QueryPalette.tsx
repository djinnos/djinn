/**
 * QueryPalette — Cmd-K (Mac) / Ctrl-K (others) fuzzy symbol search
 * over `code_graph search mode=hybrid` (PR B4).
 *
 * Selecting a result writes `selectionId` to the highlight store; the
 * canvas reducer picks it up and lights the matching node. The palette
 * itself is a thin shell over the existing `CommandDialog` primitive
 * — we trade fancy keyboard-nav (arrow-key cycling) for the simpler
 * "click result" path until D6's raw-query escape hatch needs more.
 *
 * Debounces input by 200ms so we don't spam the server while the
 * user is mid-typing.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { HugeiconsIcon } from "@hugeicons/react";
import {
  AlertCircleIcon,
  RefreshIcon,
  Search01Icon,
} from "@hugeicons/core-free-icons";

import {
  parseSearchHits,
  searchHybrid,
  truncatePathLeft,
  type SearchHit,
} from "@/api/codeGraph";
import { useCodeGraphStore } from "@/stores/codeGraphStore";
import {
  CommandDialog,
  CommandEmpty,
  CommandGroup,
  CommandItem,
  CommandList,
  CommandShortcut,
} from "@/components/ui/command";
import { cn } from "@/lib/utils";

interface QueryPaletteProps {
  projectId: string;
  /** Controlled-open hook for tests / external triggers. */
  open?: boolean;
  onOpenChange?: (open: boolean) => void;
  /** Inject hits (Storybook / tests); skips the network call. */
  injectedHits?: SearchHit[];
}

const DEBOUNCE_MS = 200;
const RESULT_LIMIT = 12;

export function QueryPalette({
  projectId,
  open: controlledOpen,
  onOpenChange,
  injectedHits,
}: QueryPaletteProps) {
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

  const setSelection = useCodeGraphStore((s) => s.setSelection);

  const [query, setQuery] = useState("");
  const [hits, setHits] = useState<SearchHit[]>(injectedHits ?? []);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // ── Cmd-K / Ctrl-K toggle ────────────────────────────────────────────────
  useEffect(() => {
    if (isControlled) return; // caller owns the open state
    const handler = (e: KeyboardEvent) => {
      if (e.key.toLowerCase() === "k" && (e.metaKey || e.ctrlKey)) {
        e.preventDefault();
        setOpen(!open);
      }
      if (e.key === "Escape" && open) {
        setOpen(false);
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [isControlled, open, setOpen]);

  // ── Debounced search ─────────────────────────────────────────────────────
  const debounceRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(() => {
    if (injectedHits) {
      setHits(injectedHits);
      return;
    }
    if (!open) return;
    const trimmed = query.trim();
    if (trimmed.length === 0) {
      setHits([]);
      setError(null);
      setLoading(false);
      return;
    }
    if (debounceRef.current) clearTimeout(debounceRef.current);
    debounceRef.current = setTimeout(() => {
      let cancelled = false;
      setLoading(true);
      setError(null);
      (async () => {
        try {
          const raw = await searchHybrid(projectId, trimmed, {
            limit: RESULT_LIMIT,
          });
          if (cancelled) return;
          setHits(parseSearchHits(raw));
        } catch (err) {
          if (cancelled) return;
          setError(err instanceof Error ? err.message : String(err));
          setHits([]);
        } finally {
          if (!cancelled) setLoading(false);
        }
      })();
      return () => {
        cancelled = true;
      };
    }, DEBOUNCE_MS);
    return () => {
      if (debounceRef.current) {
        clearTimeout(debounceRef.current);
        debounceRef.current = null;
      }
    };
  }, [open, projectId, query, injectedHits]);

  // Reset results when the dialog closes so stale matches don't flash
  // when the user reopens for a new query.
  useEffect(() => {
    if (!open) {
      setQuery("");
      if (!injectedHits) setHits([]);
      setError(null);
    }
  }, [open, injectedHits]);

  const handleSelect = useCallback(
    (hit: SearchHit) => {
      setSelection(hit.key);
      setOpen(false);
    },
    [setOpen, setSelection],
  );

  const groupHeading = useMemo(() => {
    if (loading) return "Searching…";
    if (error) return "Error";
    if (hits.length === 0) return query.trim() ? "No matches" : "Type to search";
    return `${hits.length} match${hits.length === 1 ? "" : "es"}`;
  }, [loading, error, hits.length, query]);

  return (
    <CommandDialog open={open} onOpenChange={setOpen}>
      <CommandInputWithControl
        value={query}
        onChange={setQuery}
        placeholder="Search symbols, files, modules…"
      />
      <CommandList>
        <CommandGroup heading={groupHeading}>
          {loading && (
            <div
              data-testid="query-palette-loading"
              className="flex items-center gap-2 px-2 py-3 text-xs text-muted-foreground"
            >
              <HugeiconsIcon
                icon={RefreshIcon}
                className="h-3.5 w-3.5 animate-spin [animation-duration:2s]"
              />
              <span>Searching…</span>
            </div>
          )}
          {error && (
            <div className="flex items-center gap-2 px-2 py-3 text-xs text-destructive">
              <HugeiconsIcon icon={AlertCircleIcon} className="h-3.5 w-3.5" />
              <span className="truncate">{error}</span>
            </div>
          )}
          {!loading && !error && hits.length === 0 && query.trim() && (
            <CommandEmpty />
          )}
          {!loading &&
            hits.map((hit) => (
              <CommandItem
                key={hit.key}
                searchValue={`${hit.display_name} ${hit.file ?? ""} ${hit.kind}`}
                onSelect={() => handleSelect(hit)}
                className={cn(
                  "flex flex-col items-start gap-0.5 px-2 py-2 text-left",
                )}
              >
                <div className="flex w-full items-center justify-between gap-2">
                  <span className="truncate font-mono text-sm text-foreground">
                    {hit.display_name || hit.key}
                  </span>
                  {hit.match_kind && (
                    <span className="rounded-sm bg-muted/40 px-1 text-[9px] uppercase tracking-wide text-muted-foreground">
                      {hit.match_kind}
                    </span>
                  )}
                </div>
                <div className="flex w-full items-center gap-2 text-[10px] text-muted-foreground">
                  <span className="rounded-sm bg-muted/30 px-1 font-mono uppercase">
                    {hit.kind}
                  </span>
                  {hit.file && (
                    <span className="truncate font-mono">
                      {truncatePathLeft(hit.file, 40)}
                    </span>
                  )}
                  <CommandShortcut>↩︎</CommandShortcut>
                </div>
              </CommandItem>
            ))}
        </CommandGroup>
      </CommandList>
    </CommandDialog>
  );
}

/**
 * Tiny adapter so we can drive the `CommandInput` from outside its
 * built-in `Command` context without duplicating its styling. The
 * `CommandDialog` already wraps `Command`, so we control the input
 * value via a sibling effect: read the input via `data-slot`, dispatch
 * via the `onChange` prop forwarded to the underlying `<input>`.
 */
function CommandInputWithControl({
  value,
  onChange,
  placeholder,
}: {
  value: string;
  onChange: (next: string) => void;
  placeholder?: string;
}) {
  return (
    <div data-slot="command-input-wrapper" className="flex h-9 items-center gap-2 border-b px-3">
      <HugeiconsIcon
        icon={Search01Icon}
        className="h-4 w-4 shrink-0 opacity-50"
      />
      <input
        autoFocus
        data-slot="command-input"
        data-testid="query-palette-input"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder={placeholder}
        className="placeholder:text-muted-foreground flex h-10 w-full rounded-md bg-transparent py-3 text-sm outline-none disabled:cursor-not-allowed disabled:opacity-50"
      />
    </div>
  );
}

// Re-using the dialog's command-input shell forced an awkward override —
// CommandInput pulls from CommandContext for query state, but we drive
// the palette's internal `query` slice directly. The custom control
// above keeps styling parity without forking the primitive.
//
// (CommandInput is left exported for the rest of the app; the palette
// just uses a sibling input that mimics its DOM.)
