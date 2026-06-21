import { lazy, Suspense, useMemo, useState } from "react";
import {
  Copy01Icon,
  SourceCodeIcon,
  Tick02Icon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import { cn } from "@/lib/utils";

import type { BlockProps } from "./types";
import { BlockShell } from "./BlockShell";
import {
  CODE_LANGUAGE_OPTIONS,
  computeCollapse,
  resolveCodeLanguage,
  splitCodeFilename,
} from "./code";

// `react-syntax-highlighter` (Prism) is heavy; load it only when a code block
// renders. The shared `CodeBlock` surface gives us syntax highlighting + a left
// line-number gutter, so this block layers only the filename header, copy +
// language switcher chrome, and the collapse-to-N-lines affordance on top.
const CodeBlock = lazy(() => import("./CodeBlock"));

/**
 * The presentational code surface — filename header, copy-source + language
 * switcher chrome, line-numbered highlighted body, and collapse-to-N-lines. It
 * is intentionally free of any MDX/registry knowledge so it can be reused as the
 * composable code primitive a future `tabs` block nests (pass a `filename`/`lang`
 * and the snippet `code`).
 *
 * `Code` is distinct from `AnnotatedCode`: this is the plain single-snippet
 * primitive (copy + switcher + collapse), whereas `AnnotatedCode` adds
 * line-anchored annotation rails. The app is DARK-ONLY.
 */
export function CodeSurface({
  code,
  filename,
  lang,
  maxLines,
  caption,
}: {
  code: string;
  filename?: string;
  lang?: string;
  maxLines?: number;
  caption?: string;
}) {
  // The active language can be re-picked via the switcher; it seeds from the
  // authored `lang` attr (or filename inference), with "" meaning Auto.
  const inferred = useMemo(
    () => resolveCodeLanguage(lang, filename) ?? "",
    [lang, filename],
  );
  const [selectedLang, setSelectedLang] = useState<string>("");
  const effectiveLang = (selectedLang || inferred || "text").trim();

  const [expanded, setExpanded] = useState(false);
  const [copied, setCopied] = useState(false);

  const collapse = useMemo(
    () => computeCollapse(code, expanded, maxLines),
    [code, expanded, maxLines],
  );

  const fileParts = filename ? splitCodeFilename(filename) : null;
  const langChip = effectiveLang !== "text" ? effectiveLang : null;

  const copySource = () => {
    if (typeof navigator === "undefined" || !navigator.clipboard) return;
    navigator.clipboard
      .writeText(code)
      .then(() => {
        setCopied(true);
        setTimeout(() => setCopied(false), 1200);
      })
      .catch(() => {
        /* clipboard unavailable — ignore */
      });
  };

  const rawCode = (
    <pre className="overflow-x-auto px-4 py-4 font-mono text-xs text-foreground">
      {code}
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
              {option.value ? option.label : langChip ? `Auto (${langChip})` : "Auto"}
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

  return (
    <BlockShell label="Code" accent="text-cyan-400" flush meta={meta}>
      <div className="relative">
        <div
          className={cn(
            "relative overflow-x-auto",
            collapse.collapsed && "max-h-[34rem] overflow-y-hidden",
          )}
        >
          <Suspense fallback={rawCode}>
            <CodeBlock
              language={effectiveLang}
              content={code}
              showLineNumbers
            />
          </Suspense>
          {collapse.collapsed && (
            <div className="pointer-events-none absolute inset-x-0 bottom-0 h-16 bg-gradient-to-t from-card to-transparent" />
          )}
        </div>
        {collapse.collapsible && (
          <button
            type="button"
            onClick={() => setExpanded((v) => !v)}
            aria-expanded={expanded}
            className="flex w-full items-center justify-center border-t bg-muted/30 py-1.5 text-[11px] font-medium text-muted-foreground transition-colors hover:bg-muted/60 hover:text-foreground"
          >
            {collapse.collapsed
              ? `Show all ${collapse.lineCount} lines`
              : "Show less"}
          </button>
        )}
      </div>
      {caption && (
        <p className="border-t bg-muted/20 px-4 py-2 text-[11px] text-muted-foreground">
          {caption}
        </p>
      )}
    </BlockShell>
  );
}

/**
 * CodeSnippetBlock — the standalone `Code` proposal block. A single
 * syntax-highlighted snippet with a filename header (dir/basename split),
 * copy-source button, language chip + switcher, line numbers, and
 * collapse-to-N-lines. The snippet body is the block's children text; `filename`
 * and `lang` are optional attributes, with `caption` shown beneath the surface.
 *
 * Thin wrapper around {@link CodeSurface} (the reusable primitive) — the wrapper
 * owns MDX-attribute plumbing only, keeping the surface itself reusable for a
 * future `tabs` container. Ported from agent-native's `code` block. Dark-only.
 */
export function CodeSnippetBlock({ attributes, children }: BlockProps) {
  const code = typeof children === "string" ? children.replace(/\s+$/, "") : "";
  const filename = attributes.filename?.trim() || undefined;
  const lang = attributes.lang ?? attributes.language;
  const caption = attributes.caption?.trim() || undefined;
  const rawMax = attributes.maxLines ?? attributes.maxlines;
  const parsedMax = rawMax != null ? Number(rawMax) : NaN;
  const maxLines = Number.isFinite(parsedMax)
    ? Math.max(0, Math.min(2000, Math.floor(parsedMax)))
    : undefined;

  if (!code) {
    return (
      <BlockShell label="Code" accent="text-cyan-400">
        <div className="flex items-center gap-2 text-xs text-muted-foreground">
          <HugeiconsIcon icon={SourceCodeIcon} size={14} />
          <span>Empty code snippet.</span>
        </div>
      </BlockShell>
    );
  }

  return (
    <CodeSurface
      code={code}
      filename={filename}
      lang={lang}
      maxLines={maxLines}
      caption={caption}
    />
  );
}
