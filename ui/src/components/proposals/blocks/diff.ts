// Pure (React-free) helpers for the interactive Diff block. These parse the
// unified-diff text an author writes inside <Diff>…</Diff> into numbered diff
// rows (with reconstructed old/new line numbers), pair those rows into split
// (side-by-side) columns, and collapse long unchanged runs. Kept in their own
// module so they are unit-testable without pulling React in.
//
// djinn's MDX stores the diff as the block's *children text* — a git-style
// unified diff: optional `diff --git`/`index`/`---`/`+++`/`@@` headers, then a
// body whose lines are prefixed `+` (added), `-` (removed), or ` ` (context).
// `filename` and `lang` are optional attributes (cosmetic header / highlight
// hint) and are NOT parsed here.
//
// Robustness: if the children text doesn't look like a unified diff (no `+`/`-`
// prefixed lines and no `@@` hunk header), `isDiffLike` returns false so the
// React component can fall back to a plain code <pre> instead of rendering an
// empty or misleading diff. The parser itself never throws.

/** The display layout for a diff body. */
export type DiffMode = "unified" | "split";

/** Which kind of line a diff row represents. */
export type DiffRowKind = "context" | "added" | "removed";

/** A single rendered diff row with reconstructed 1-based line numbers. */
export interface DiffRow {
  kind: DiffRowKind;
  /** Line number in the OLD file (absent for added rows). */
  oldNo?: number;
  /** Line number in the NEW file (absent for removed rows). */
  newNo?: number;
  /** The line text WITHOUT its `+`/`-`/` ` prefix. */
  text: string;
}

/** A contiguous run of context rows collapsed when longer than the threshold. */
export interface CollapsedRun {
  collapsed: true;
  rows: DiffRow[];
}

/** A diff segment: either a visible row or a collapsed run of context rows. */
export type DiffSegment = DiffRow | CollapsedRun;

/** A paired split-view row: old line on the left, new line on the right. */
export interface SplitRow {
  left?: DiffRow;
  right?: DiffRow;
}

/** Number of context lines above which an unchanged run is collapsed. */
export const COLLAPSE_THRESHOLD = 6;
/** Context lines kept visible at each edge of a collapsed run. */
export const CONTEXT_EDGE = 3;

const HUNK_HEADER = /^@@\s*-(\d+)(?:,(\d+))?\s+\+(\d+)(?:,(\d+))?\s*@@/;

/**
 * Diff file headers that carry no body content. They precede the hunks and must
 * not be numbered as context/added/removed rows.
 */
function isFileHeaderLine(line: string): boolean {
  return (
    line.startsWith("diff --git ") ||
    line.startsWith("index ") ||
    line.startsWith("--- ") ||
    line.startsWith("+++ ") ||
    line.startsWith("new file mode ") ||
    line.startsWith("deleted file mode ") ||
    line.startsWith("rename from ") ||
    line.startsWith("rename to ") ||
    line.startsWith("similarity index ") ||
    line.startsWith("old mode ") ||
    line.startsWith("new mode ")
  );
}

/**
 * Heuristic: does this children text look like a unified diff we can render? We
 * accept it if there is at least one `@@` hunk header OR at least one body line
 * that starts with `+`/`-` (but is not a `+++`/`---` file header). Anything else
 * (plain prose, a bare code snippet) returns false so the caller can fall back
 * to a plain code block.
 */
export function isDiffLike(text: string): boolean {
  if (!text.trim()) return false;
  const lines = text.split("\n");
  for (const line of lines) {
    if (HUNK_HEADER.test(line)) return true;
    if (line.startsWith("+++ ") || line.startsWith("--- ")) continue;
    if (line.startsWith("+") || line.startsWith("-")) return true;
  }
  return false;
}

/**
 * Parse a unified diff into numbered rows. Line numbers are reconstructed from
 * `@@ -old,+new @@` hunk headers when present; absent a hunk header we number
 * from 1 (a "headerless" diff — just `+`/`-`/` ` prefixed lines). File header
 * lines (`diff --git`, `---`, `+++`, `@@`, mode/rename lines) are dropped from
 * the row stream. A `\ No newline at end of file` marker line is also dropped.
 *
 * Never throws: malformed hunk headers fall back to continuing the current
 * counters; lines with no recognizable prefix are treated as context.
 */
export function parseUnifiedDiff(text: string): DiffRow[] {
  const rows: DiffRow[] = [];
  if (!text) return rows;

  const lines = text.split("\n");
  // Drop a single trailing empty element produced by a trailing newline so a
  // diff ending in "\n" does not emit a phantom blank context row.
  if (lines.length > 0 && lines[lines.length - 1] === "") lines.pop();

  let oldNo = 0;
  let newNo = 0;
  let seenHunk = false;

  for (const line of lines) {
    const hunk = HUNK_HEADER.exec(line);
    if (hunk) {
      oldNo = Number.parseInt(hunk[1], 10) - 1;
      newNo = Number.parseInt(hunk[3], 10) - 1;
      seenHunk = true;
      continue;
    }
    if (isFileHeaderLine(line)) continue;
    // "\ No newline at end of file" — a marker, not content.
    if (line.startsWith("\\")) continue;

    const sign = line.charAt(0);
    const body = line.slice(1);
    if (sign === "+") {
      newNo += 1;
      rows.push({ kind: "added", newNo, text: body });
    } else if (sign === "-") {
      oldNo += 1;
      rows.push({ kind: "removed", oldNo, text: body });
    } else {
      // Context line. A leading single space is the unified-diff context marker;
      // strip it. A fully empty line (no prefix at all) is context too.
      const contextText = sign === " " ? body : line;
      oldNo += 1;
      newNo += 1;
      rows.push({ kind: "context", oldNo, newNo, text: contextText });
    }
  }

  // A headerless diff that never saw a hunk still numbered from 1 above, which
  // is the desired behaviour, so `seenHunk` is only informational.
  void seenHunk;
  return rows;
}

/**
 * Count added / removed rows for the `+N −N` header stat.
 */
export function diffStats(rows: DiffRow[]): { added: number; removed: number } {
  let added = 0;
  let removed = 0;
  for (const row of rows) {
    if (row.kind === "added") added += 1;
    else if (row.kind === "removed") removed += 1;
  }
  return { added, removed };
}

/**
 * Group rows into segments, collapsing interior runs of more than
 * COLLAPSE_THRESHOLD context rows (keeping CONTEXT_EDGE visible at each side).
 * Leading and trailing runs collapse too, but keep only the inner edge visible
 * (no point showing context that has no adjacent change). Changed rows are never
 * collapsed.
 */
export function segmentRows(rows: DiffRow[]): DiffSegment[] {
  const segments: DiffSegment[] = [];

  const collapseRun = (run: DiffRow[], atStart: boolean, atEnd: boolean) => {
    if (run.length <= COLLAPSE_THRESHOLD) {
      for (const row of run) segments.push(row);
      return;
    }
    const head = atStart ? [] : run.slice(0, CONTEXT_EDGE);
    const tail = atEnd ? [] : run.slice(run.length - CONTEXT_EDGE);
    const hidden = run.slice(head.length, run.length - tail.length);
    for (const row of head) segments.push(row);
    if (hidden.length > 0) segments.push({ collapsed: true, rows: hidden });
    for (const row of tail) segments.push(row);
  };

  let i = 0;
  while (i < rows.length) {
    if (rows[i].kind !== "context") {
      segments.push(rows[i]);
      i += 1;
      continue;
    }
    let j = i;
    while (j < rows.length && rows[j].kind === "context") j += 1;
    const run = rows.slice(i, j);
    collapseRun(run, i === 0, j === rows.length);
    i = j;
  }
  return segments;
}

/**
 * Pair removed lines (left) with added lines (right) so a modification shows the
 * old and new side by side; context lines mirror on both columns. Leftover adds
 * or removes fall through as half-empty rows (GitHub split behaviour).
 */
export function pairSplitRows(rows: DiffRow[]): SplitRow[] {
  const out: SplitRow[] = [];
  let i = 0;
  while (i < rows.length) {
    const row = rows[i];
    if (row.kind === "context") {
      out.push({ left: row, right: row });
      i += 1;
      continue;
    }
    const removed: DiffRow[] = [];
    const added: DiffRow[] = [];
    while (i < rows.length && rows[i].kind === "removed") removed.push(rows[i++]);
    while (i < rows.length && rows[i].kind === "added") added.push(rows[i++]);
    const max = Math.max(removed.length, added.length);
    for (let k = 0; k < max; k += 1) {
      out.push({ left: removed[k], right: added[k] });
    }
  }
  return out;
}

/** Split a `filename` into `{ basename, directory }` for the header. */
export function splitFilename(filename?: string): {
  basename: string;
  directory: string | null;
} {
  const value = filename?.trim() || "";
  if (!value) return { basename: "", directory: null };
  const segments = value.split("/").filter(Boolean);
  const basename = segments[segments.length - 1] ?? value;
  const directory = segments.length > 1 ? segments.slice(0, -1).join("/") : null;
  return { basename, directory };
}
