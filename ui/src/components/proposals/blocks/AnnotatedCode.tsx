import {
  lazy,
  Suspense,
  useMemo,
  useRef,
  useState,
  type HTMLProps,
} from "react";
import {
  Copy01Icon,
  SourceCodeIcon,
  Tick02Icon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import { cn } from "@/lib/utils";

import type { BlockProps } from "./types";
import { BlockShell } from "./BlockShell";
import { splitFilename } from "./diff";
import {
  CODE_LANGUAGE_OPTIONS,
  computeCollapse,
  resolveCodeLanguage,
} from "./code";
import {
  buildLineMarkerMap,
  hasResolvedAnnotations,
  parseAnnotationsAttr,
  resolveAnnotations,
  type ResolvedAnnotation,
} from "./annotations";
import {
  AnnotationGutterMarker,
  AnnotationHiddenStack,
  AnnotationHoverCard,
} from "./annotationRail";
import {
  anchorFromElements,
  useAnnotationHover,
} from "./annotationRail.helpers";

// `react-syntax-highlighter` is heavy; load it only when a code block renders.
const CodeBlock = lazy(() => import("./CodeBlock"));

/**
 * AnnotatedCode — the single, general code block: a line-numbered,
 * syntax-highlighted snippet with a filename header, copy button, language
 * switcher, collapse-to-N-lines, and OPTIONAL line-anchored annotations.
 *
 * Header: a filename (dir/basename split) when `filename` is set, a language
 * switcher (re-highlights as another grammar; "Auto" returns to the
 * authored/inferred language), and a copy-source button. Body: the
 * Prism-highlighted source with a left line-number gutter, collapsed behind a
 * "Show all N lines" toggle once it exceeds `maxLines` (default 40; `0` never
 * collapses). Annotations come from the optional `annotations` attribute (a JSON
 * string of `{ lines | line, note, label? }[]`, where `lines` is `"3"` or
 * `"3-5"`); each renders as an amber highlight band on its line(s) plus a
 * numbered gutter pip, with the note shown in an on-hover portal card anchored
 * beside the code. Hovering a line ↔ its note cross-highlights, annotated lines
 * are keyboard-focusable (Enter/Space toggles the card, Escape closes), and a
 * visually-hidden stack keeps every note reachable by assistive tech and tests.
 * When no annotations are authored the block is simply a plain code snippet.
 *
 * Robust: invalid `annotations` JSON renders the code WITHOUT annotations plus a
 * quiet warning (never crashes, never silently swallows). The app is DARK-ONLY.
 */
export function AnnotatedCode({ attributes, children }: BlockProps) {
  const filename = attributes.filename?.trim() || undefined;
  const content =
    typeof children === "string" ? children.replace(/\s+$/, "") : "";

  // The active language can be re-picked via the switcher; it seeds from the
  // authored `lang`/`language` attr (or filename inference), with "" meaning Auto.
  const authoredLang = attributes.lang ?? attributes.language;
  const inferred = useMemo(
    () => resolveCodeLanguage(authoredLang, filename) ?? "",
    [authoredLang, filename],
  );
  const [selectedLang, setSelectedLang] = useState<string>("");
  const language = (selectedLang || inferred || "text").trim();

  // Optional collapse-to-N: `maxLines` overrides the default cap (40); `0`
  // disables collapsing entirely.
  const rawMax = attributes.maxLines ?? attributes.maxlines;
  const parsedMax = rawMax != null ? Number(rawMax) : NaN;
  const maxLines = Number.isFinite(parsedMax)
    ? Math.max(0, Math.min(2000, Math.floor(parsedMax)))
    : undefined;

  const lineCount = useMemo(
    () => (content ? content.split("\n").length : 0),
    [content],
  );

  // Parse + resolve annotations. `null` → the JSON attr was malformed.
  const parsed = useMemo(
    () => parseAnnotationsAttr(attributes.annotations),
    [attributes.annotations],
  );
  const annotationsInvalid = parsed === null;
  const resolved = useMemo(
    () => resolveAnnotations(parsed ?? [], lineCount),
    [parsed, lineCount],
  );
  const lineMarkers = useMemo(() => buildLineMarkerMap(resolved), [resolved]);
  const hasAnnotations = hasResolvedAnnotations(resolved);

  const [expanded, setExpanded] = useState(false);
  const [copied, setCopied] = useState(false);
  const codeRef = useRef<HTMLDivElement | null>(null);
  const hover = useAnnotationHover();
  const activeIndex = hover.activeIndex;

  const collapse = useMemo(
    () => computeCollapse(content, expanded, maxLines),
    [content, expanded, maxLines],
  );
  const collapsible = collapse.collapsible;
  const collapsed = collapse.collapsed;

  const copySource = () => {
    if (typeof navigator === "undefined" || !navigator.clipboard) return;
    navigator.clipboard
      .writeText(content)
      .then(() => {
        setCopied(true);
        setTimeout(() => setCopied(false), 1200);
      })
      .catch(() => {
        /* clipboard unavailable — ignore */
      });
  };

  // Per-line props attach the amber band, gutter handling, hover/focus wiring,
  // and keyboard access to lines that carry an annotation.
  const lineProps = (lineNumber: number): HTMLProps<HTMLElement> => {
    const markers = lineMarkers.get(lineNumber);
    if (!markers || markers.length === 0) {
      return { className: "annotated-code-line" };
    }
    const primary = markers[0];
    const active = markers.some((m) => m.index === activeIndex);

    const openCard = (rowEl: HTMLElement | null) => {
      const anchor = anchorFromElements(codeRef.current, rowEl);
      if (anchor) hover.open(primary.index, anchor);
    };

    return {
      "data-annotated": "true",
      "data-annotation-index": String(primary.index),
      "data-active": active ? "true" : undefined,
      tabIndex: 0,
      role: "button",
      "aria-label": `Annotation ${primary.marker}: ${primary.annotation.note}`,
      className: cn(
        "annotated-code-line relative block cursor-pointer transition-colors",
        active
          ? "bg-amber-400/20"
          : "bg-amber-400/[0.08] hover:bg-amber-400/15",
      ),
      style: { boxShadow: "inset 2px 0 0 0 rgb(251 191 36 / 0.7)" },
      onMouseEnter: (event) => openCard(event.currentTarget as HTMLElement),
      onMouseLeave: () => hover.scheduleClose(),
      onFocus: (event) => openCard(event.currentTarget as HTMLElement),
      onBlur: () => hover.scheduleClose(),
      onKeyDown: (event) => {
        if (event.key === "Enter" || event.key === " ") {
          event.preventDefault();
          const anchor = anchorFromElements(
            codeRef.current,
            event.currentTarget as HTMLElement,
          );
          if (anchor) hover.toggle(primary.index, anchor);
        } else if (event.key === "Escape") {
          hover.close();
        }
      },
    } as HTMLProps<HTMLElement>;
  };

  // Open the hover card for a footer-rail entry, anchored on its first
  // annotated row (falls back to the code block itself).
  const openForAnnotation = (item: ResolvedAnnotation) => {
    const rowEl =
      codeRef.current?.querySelector<HTMLElement>(
        `[data-annotation-index="${item.index}"]`,
      ) ?? codeRef.current;
    const anchor = anchorFromElements(codeRef.current, rowEl);
    if (anchor) hover.toggle(item.index, anchor);
  };

  const fileParts = filename ? splitFilename(filename) : null;
  const langChip = language !== "text" ? language : null;

  const rawCode = (
    <pre className="overflow-x-auto overflow-y-hidden px-4 py-4 font-mono text-xs text-foreground">
      {content}
    </pre>
  );

  const meta = (
    <span className="flex min-w-0 items-center gap-2">
      {fileParts ? (
        <span className="flex min-w-0 items-baseline gap-1.5 font-mono">
          {fileParts.directory && (
            <span className="min-w-0 truncate text-[11px] text-muted-foreground/70">
              {fileParts.directory}/
            </span>
          )}
          <span className="max-w-[14rem] truncate font-semibold text-foreground">
            {fileParts.basename}
          </span>
        </span>
      ) : null}
      {/* Language switcher — re-highlights as a different grammar on demand.
          "Auto" returns to the authored/inferred language. */}
      <label className="flex items-center gap-1">
        <span className="sr-only">Code language</span>
        <select
          aria-label="Code language"
          value={selectedLang}
          onChange={(event) => setSelectedLang(event.target.value)}
          className="rounded border bg-muted/40 px-1.5 py-0.5 font-mono text-[11px] text-muted-foreground transition-colors hover:text-foreground focus:outline-none focus:ring-1 focus:ring-ring"
        >
          {CODE_LANGUAGE_OPTIONS.map((option) => (
            <option key={option.value || "auto"} value={option.value}>
              {option.value
                ? option.label
                : langChip
                  ? `Auto (${langChip})`
                  : "Auto"}
            </option>
          ))}
        </select>
      </label>
      <button
        type="button"
        onClick={copySource}
        title="Copy source"
        aria-label={copied ? "Copied" : "Copy source"}
        className="flex shrink-0 items-center gap-1 rounded border px-1.5 py-0.5 text-[11px] text-muted-foreground transition-colors hover:bg-muted/70 hover:text-foreground"
      >
        <HugeiconsIcon
          icon={copied ? Tick02Icon : Copy01Icon}
          size={12}
          className={copied ? "text-emerald-400" : undefined}
        />
        {copied ? "Copied" : "Copy"}
      </button>
    </span>
  );

  const activeItem =
    activeIndex != null
      ? resolved.find((r) => r.index === activeIndex) ?? null
      : null;

  return (
    <BlockShell label="Code" accent="text-orange-400" flush meta={meta}>
      {annotationsInvalid && (
        <div className="flex items-center gap-2 border-b border-amber-500/30 bg-amber-500/10 px-4 py-2 text-xs text-amber-300">
          <HugeiconsIcon icon={SourceCodeIcon} size={14} />
          <span>
            Annotations could not be parsed (invalid JSON) — showing code without
            them.
          </span>
        </div>
      )}
      <div ref={codeRef} className="relative">
        <div
          className={cn(
            "annotated-code-surface relative overflow-x-auto overflow-y-hidden",
            collapsed && "max-h-[34rem]",
          )}
        >
          <Suspense fallback={rawCode}>
            <CodeBlock
              language={language}
              content={content}
              showLineNumbers
              lineProps={hasAnnotations ? lineProps : undefined}
            />
          </Suspense>
          {collapsed && (
            <div className="pointer-events-none absolute inset-x-0 bottom-0 h-16 bg-gradient-to-t from-card to-transparent" />
          )}
        </div>
        {collapsible && (
          <button
            type="button"
            onClick={() => setExpanded((v) => !v)}
            aria-expanded={expanded}
            className="flex w-full items-center justify-center border-t bg-muted/30 py-1.5 text-[11px] font-medium text-muted-foreground transition-colors hover:bg-muted/60 hover:text-foreground"
          >
            {collapsed
              ? `Show all ${collapse.lineCount} lines`
              : "Show less"}
          </button>
        )}

        {/* Visually-hidden notes for assistive tech + tests. */}
        <AnnotationHiddenStack items={resolved} />

        {/* The on-hover note card (portaled, viewport-aware). */}
        {activeItem && hover.anchor && (
          <AnnotationHoverCard
            item={activeItem}
            anchor={hover.anchor}
            onMouseEnter={hover.cancelClose}
            onMouseLeave={hover.scheduleClose}
            onClose={hover.close}
          />
        )}
      </div>

      {/* A compact summary rail listing each annotated span. Keeps notes
          scannable without hovering and provides the two-way cross-highlight:
          hovering an entry lights its gutter pip and the line band. */}
      {hasAnnotations && (
        <ul className="flex flex-wrap gap-1.5 border-t bg-muted/20 px-4 py-2">
          {resolved
            .filter((item) => item.range)
            .map((item) => (
              <li key={item.index}>
                <button
                  type="button"
                  onClick={() => openForAnnotation(item)}
                  title={item.annotation.note}
                  className={cn(
                    "flex items-center gap-1.5 rounded border px-2 py-1 text-[11px] transition-colors",
                    item.index === activeIndex
                      ? "border-amber-300/40 bg-amber-300/10 text-foreground"
                      : "border-border text-muted-foreground hover:border-amber-300/30 hover:text-foreground",
                  )}
                >
                  <AnnotationGutterMarker
                    marker={item.marker}
                    active={item.index === activeIndex}
                  />
                  <span className="max-w-[18rem] truncate">
                    {item.annotation.label ?? item.annotation.note}
                  </span>
                </button>
              </li>
            ))}
        </ul>
      )}
    </BlockShell>
  );
}
