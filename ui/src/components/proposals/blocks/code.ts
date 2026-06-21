// Pure (React-free) helpers for the standalone `Code` block. The block renders a
// single syntax-highlighted snippet (distinct from `AnnotatedCode`, which is for
// line-anchored annotations). It deliberately holds ONE snippet — a "file rail"
// of several files is a future `tabs` container holding `Code` blocks — so this
// component is kept cleanly reusable as that primitive.
//
// Ported from agent-native's `code` block: filename dir/basename split, language
// inference from the file extension, a language switcher option list, and the
// collapse-to-N-lines math. Highlighting + line numbers are handled by the
// existing `CodeBlock.tsx` Prism surface, so these helpers only cover the parts
// that are pure and unit-testable.

/** Default number of lines shown before the snippet collapses behind a toggle. */
export const DEFAULT_CODE_MAX_LINES = 40;

/**
 * Split a filename into its directory prefix and basename for the header label,
 * e.g. `src/server/auth.ts` → `{ directory: "src/server", basename: "auth.ts" }`.
 * A bare filename has a `null` directory.
 */
export function splitCodeFilename(filename?: string): {
  basename: string;
  directory: string | null;
} {
  const value = filename?.trim() || "";
  if (!value) return { basename: "", directory: null };
  const segments = value.split("/").filter(Boolean);
  const basename = segments[segments.length - 1] ?? value;
  const directory =
    segments.length > 1 ? segments.slice(0, -1).join("/") : null;
  return { basename, directory };
}

/** Common file extension / shorthand → a Prism-friendly language name. */
const EXTENSION_LANGUAGE: Record<string, string> = {
  cjs: "javascript",
  cts: "typescript",
  htm: "html",
  js: "javascript",
  json: "json",
  jsonc: "json",
  jsx: "jsx",
  md: "markdown",
  mdx: "markdown",
  mjs: "javascript",
  mts: "typescript",
  py: "python",
  rb: "ruby",
  rs: "rust",
  sh: "bash",
  shell: "bash",
  sql: "sql",
  ts: "typescript",
  tsx: "tsx",
  yaml: "yaml",
  yml: "yaml",
};

/**
 * Best-effort language name from a filename / path extension, e.g. `auth.ts`
 * → `typescript`. Recognizes a couple of extension-less names (Dockerfile,
 * Makefile). Returns `null` when nothing can be inferred.
 */
export function inferCodeLanguageFromFilename(
  filename?: string | null,
): string | null {
  const basename = filename?.split("/").pop()?.trim().toLowerCase();
  if (!basename) return null;
  if (basename === "dockerfile") return "bash";
  if (basename === "makefile") return "makefile";
  const extension = basename.includes(".")
    ? basename.split(".").pop()
    : undefined;
  if (!extension) return null;
  return EXTENSION_LANGUAGE[extension] ?? null;
}

/**
 * Normalize a raw `lang` attribute to a lower-cased, `language-`-stripped hint,
 * or `null` when empty so the caller can fall back to filename inference.
 */
export function normalizeCodeLanguage(value?: string | null): string | null {
  const raw = value
    ?.trim()
    .toLowerCase()
    .replace(/^language-/, "");
  return raw ? raw : null;
}

/**
 * Resolve the effective language for a snippet: the explicit `lang` attribute
 * wins, else infer from the filename, else `null` (plain text).
 */
export function resolveCodeLanguage(
  lang?: string | null,
  filename?: string | null,
): string | null {
  return normalizeCodeLanguage(lang) ?? inferCodeLanguageFromFilename(filename);
}

export interface CollapseState {
  /** Whether a collapse toggle should be offered at all. */
  collapsible: boolean;
  /** Whether the snippet is currently collapsed (collapsible AND not expanded). */
  collapsed: boolean;
  /** Number of lines hidden while collapsed (0 when not collapsible). */
  hiddenLines: number;
  /** Total line count of the snippet. */
  lineCount: number;
}

/**
 * Compute the collapse state for a snippet. `maxLines` omitted ⇒ the default
 * cap (40); `0` ⇒ never collapse (always show everything). A snippet is only
 * collapsible when its line count exceeds the cap.
 */
export function computeCollapse(
  code: string,
  expanded: boolean,
  maxLines?: number,
): CollapseState {
  const lineCount = code ? code.split("\n").length : 0;
  const cap =
    maxLines == null
      ? DEFAULT_CODE_MAX_LINES
      : maxLines > 0
        ? maxLines
        : null;
  const collapsible = cap != null && lineCount > cap;
  const collapsed = collapsible && !expanded;
  const hiddenLines = collapsible ? lineCount - (cap as number) : 0;
  return { collapsible, collapsed, hiddenLines, lineCount };
}

/** Option for the language switcher; `value: ""` is the Auto-detect sentinel. */
export interface CodeLanguageOption {
  value: string;
  label: string;
}

/** Languages offered in the switcher menu (Auto first), matching agent-native. */
export const CODE_LANGUAGE_OPTIONS: ReadonlyArray<CodeLanguageOption> = [
  { value: "", label: "Auto" },
  { value: "typescript", label: "TypeScript" },
  { value: "javascript", label: "JavaScript" },
  { value: "tsx", label: "TSX" },
  { value: "jsx", label: "JSX" },
  { value: "json", label: "JSON" },
  { value: "html", label: "HTML" },
  { value: "css", label: "CSS" },
  { value: "bash", label: "Bash" },
  { value: "python", label: "Python" },
  { value: "sql", label: "SQL" },
  { value: "yaml", label: "YAML" },
  { value: "markdown", label: "Markdown" },
  { value: "go", label: "Go" },
  { value: "rust", label: "Rust" },
  { value: "diff", label: "Diff" },
];
