import { useCallback, useMemo, useState } from "react";
import {
  ArrowRight01Icon,
  CodeIcon,
  Copy01Icon,
  Tick02Icon,
  UnfoldLessIcon,
  UnfoldMoreIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import { cn } from "@/lib/utils";

import type { BlockProps } from "./types";
import {
  JSON_EXPLORER_DEFAULT_COLLAPSED_DEPTH,
  containerEntries,
  containerSummary,
  formatLeaf,
  isContainer,
  isNonEmptyContainer,
  parseJsonValue,
  valueKind,
  type JsonObject,
  type JsonValue,
} from "./jsonExplorer";

/**
 * JsonExplorer — a browser-devtools / Postman-style collapsible, type-colored
 * JSON tree for proposal specs.
 *
 * djinn does NOT serialize a structured prop; the block's payload is its
 * *children string* (`segment.content`). We `JSON.parse` that text (see
 * `jsonExplorer.ts`) and walk the value as a tree: object/array nodes get a
 * chevron + a one-line summary (`3 keys` / `5 items`) and toggle open/closed;
 * leaf values are type-colored (string emerald, number sky, boolean violet,
 * null muted). An expand-all / collapse-all control flips every node at once,
 * and a copy button yanks the pretty-printed payload.
 *
 * Graceful + read-only: if the children are not valid JSON the renderer falls
 * back to a styled `<pre>` of the raw text plus the parse error, so an
 * oddly-authored block never blanks or crashes. The tree is display-only.
 */
export function JsonExplorerBlock({ attributes, children }: BlockProps) {
  const content = typeof children === "string" ? children : "";
  const title = attributes.title?.trim() || undefined;

  // De-chromed: no outer "JSON" card. The interactive tree carries its own
  // compact toolbar (label + expand/collapse + copy), so it reads as a data
  // viewer embedded in the document rather than a card-in-a-card.
  return <JsonTree code={content} label={title ?? "JSON"} />;
}

/* ── Type-color tokens ──────────────────────────────────────────────────────── */

const KIND_CLASS: Record<string, string> = {
  string: "text-emerald-300",
  number: "text-sky-300",
  boolean: "text-violet-300",
  null: "italic text-muted-foreground",
};

const KEY_CLASS = "text-rose-300";
const PUNCT_CLASS = "text-muted-foreground/70";

/* ── Reusable tree surface ──────────────────────────────────────────────────── */

/**
 * The reusable JSON-tree surface: a bordered code panel with an `Example`-style
 * header (label + expand/collapse-all + copy) and the collapsible tree (or a
 * `<pre>` fallback for non-JSON). The `json-explorer` block wraps it in a
 * `BlockShell`; the `api-endpoint` block reuses it directly to upgrade its JSON
 * example bodies — both share one fallback path.
 *
 * `pulse` carries an `expand`/`collapse` intent plus a nonce; bumping the nonce
 * remounts the tree so every node re-seeds from the new open/closed state.
 */
export function JsonTree({
  code,
  label = "JSON",
  collapsedDepth = JSON_EXPLORER_DEFAULT_COLLAPSED_DEPTH,
  className,
}: {
  code: string;
  label?: string;
  collapsedDepth?: number;
  className?: string;
}) {
  const parsed = useMemo(() => parseJsonValue(code), [code]);
  const [pulse, setPulse] = useState<{ open: boolean; nonce: number } | null>(
    null,
  );
  const [copied, setCopied] = useState(false);

  const value = parsed.ok ? parsed.value : null;
  const showTreeActions = value !== null && isNonEmptyContainer(value);

  const copyPayload = useCallback(() => {
    if (typeof navigator === "undefined" || !navigator.clipboard) return;
    const text = parsed.ok
      ? JSON.stringify(parsed.value, null, 2)
      : code;
    navigator.clipboard
      .writeText(text)
      .then(() => {
        setCopied(true);
        setTimeout(() => setCopied(false), 1200);
      })
      .catch(() => {
        /* clipboard unavailable — ignore */
      });
  }, [code, parsed]);

  return (
    <div
      className={cn(
        "overflow-hidden rounded-md border bg-muted/30",
        className,
      )}
    >
      <div className="flex items-center justify-between gap-2 border-b bg-muted/40 px-2.5 py-1">
        <span className="flex items-center gap-1.5 text-[10px] font-semibold uppercase tracking-wider text-muted-foreground">
          <HugeiconsIcon icon={CodeIcon} size={12} className="shrink-0" />
          {label}
        </span>
        <span className="flex items-center gap-1">
          {showTreeActions && (
            <>
              <TreeActionButton
                onClick={() =>
                  setPulse((prev) => ({
                    open: true,
                    nonce: (prev?.nonce ?? 0) + 1,
                  }))
                }
                icon={UnfoldMoreIcon}
                label="Expand all"
              />
              <TreeActionButton
                onClick={() =>
                  setPulse((prev) => ({
                    open: false,
                    nonce: (prev?.nonce ?? 0) + 1,
                  }))
                }
                icon={UnfoldLessIcon}
                label="Collapse all"
              />
            </>
          )}
          <button
            type="button"
            onClick={copyPayload}
            title="Copy JSON"
            aria-label={copied ? "Copied" : "Copy JSON"}
            className="flex shrink-0 items-center gap-1 rounded px-1.5 py-0.5 text-[11px] text-muted-foreground transition-colors hover:bg-muted/70 hover:text-foreground"
          >
            <HugeiconsIcon
              icon={copied ? Tick02Icon : Copy01Icon}
              size={12}
              className={copied ? "text-emerald-400" : undefined}
            />
          </button>
        </span>
      </div>

      <div
        dir="ltr"
        className="overflow-x-auto overflow-y-hidden px-3 py-2 font-mono text-xs leading-relaxed text-foreground [unicode-bidi:isolate]"
      >
        {parsed.ok ? (
          <JsonNode
            key={pulse?.nonce ?? 0}
            value={parsed.value}
            depth={0}
            collapsedDepth={collapsedDepth}
            forceOpen={pulse}
          />
        ) : (
          <div className="space-y-1.5">
            <pre className="overflow-x-auto overflow-y-hidden whitespace-pre-wrap break-words">
              {code || "—"}
            </pre>
            <p className="text-[11px] text-amber-300">
              Could not parse JSON: {parsed.error}
            </p>
          </div>
        )}
      </div>
    </div>
  );
}

function TreeActionButton({
  onClick,
  icon,
  label,
}: {
  onClick: () => void;
  icon: typeof UnfoldMoreIcon;
  label: string;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      title={label}
      aria-label={label}
      className="flex shrink-0 items-center rounded px-1 py-0.5 text-muted-foreground transition-colors hover:bg-muted/70 hover:text-foreground"
    >
      <HugeiconsIcon icon={icon} size={13} />
    </button>
  );
}

/* ── Tree node ──────────────────────────────────────────────────────────────── */

interface JsonNodeProps {
  /** The object key or array index label for this node (root has none). */
  label?: string | number;
  value: JsonValue;
  depth: number;
  /** Depth beyond which nodes start collapsed. */
  collapsedDepth: number;
  /** Global expand/collapse pulse — seeds the open state when its nonce changes. */
  forceOpen: { open: boolean; nonce: number } | null;
  /** True when this node has a following sibling (renders a trailing comma). */
  trailingComma?: boolean;
}

/**
 * A single tree node. Containers carry their own open/closed state, seeded from
 * `collapsedDepth` (or the global pulse when one is active). The whole tree is
 * remounted on a pulse via a `key`, so each node simply re-seeds — no effect
 * gymnastics. Leaves render inline with their type color.
 */
function JsonNode({
  label,
  value,
  depth,
  collapsedDepth,
  forceOpen,
  trailingComma,
}: JsonNodeProps) {
  const container = isContainer(value);
  const entries = container ? containerEntries(value) : [];
  const empty = entries.length === 0;
  const [open, setOpen] = useState(forceOpen?.open ?? depth < collapsedDepth);

  const keyEl =
    label !== undefined ? (
      <>
        <span className={KEY_CLASS}>
          {typeof label === "number" ? label : JSON.stringify(label)}
        </span>
        <span className={PUNCT_CLASS}>: </span>
      </>
    ) : null;

  if (!container) {
    return (
      <div className="flex items-start py-px leading-relaxed">
        <span className="whitespace-pre">{keyEl}</span>
        <span className={KIND_CLASS[valueKind(value)]}>{formatLeaf(value)}</span>
        {trailingComma && <span className={PUNCT_CLASS}>,</span>}
      </div>
    );
  }

  const isArray = Array.isArray(value);
  const openBrace = isArray ? "[" : "{";
  const closeBrace = isArray ? "]" : "}";

  return (
    <div className="leading-relaxed">
      <button
        type="button"
        aria-expanded={open}
        disabled={empty}
        onClick={() => setOpen((value) => !value)}
        className={cn(
          "flex w-full items-start gap-1 rounded py-px text-left transition-colors",
          !empty && "hover:bg-muted/50",
          empty && "cursor-default",
        )}
      >
        <HugeiconsIcon
          icon={ArrowRight01Icon}
          size={13}
          className={cn(
            "mt-0.5 shrink-0 text-muted-foreground transition-transform",
            open && "rotate-90",
            empty && "opacity-0",
          )}
        />
        <span className="min-w-0 whitespace-pre-wrap break-words">
          {keyEl}
          <span className={PUNCT_CLASS}>{openBrace}</span>
          {!open && !empty && (
            <span className="ml-1 text-muted-foreground/80">
              {containerSummary(value as JsonValue[] | JsonObject)}
            </span>
          )}
          {(!open || empty) && (
            <span className={PUNCT_CLASS}>
              {empty ? "" : " … "}
              {closeBrace}
              {trailingComma ? "," : ""}
            </span>
          )}
        </span>
      </button>

      {open && !empty && (
        <>
          <div className="ml-[6px] border-l border-border/70 pl-3.5">
            {entries.map(([entryKey, entryValue], index) => (
              <JsonNode
                key={String(entryKey)}
                label={entryKey}
                value={entryValue}
                depth={depth + 1}
                collapsedDepth={collapsedDepth}
                forceOpen={forceOpen}
                trailingComma={index < entries.length - 1}
              />
            ))}
          </div>
          <div className="flex items-start">
            <span className="ml-[19px] whitespace-pre">
              <span className={PUNCT_CLASS}>{closeBrace}</span>
              {trailingComma && <span className={PUNCT_CLASS}>,</span>}
            </span>
          </div>
        </>
      )}
    </div>
  );
}
