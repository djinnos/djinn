// Pure (React-free) helpers for the shared line-anchored annotation engine used
// by the `annotated-code` block (and ready to be adopted by `diff` and future
// blocks). Kept in their own module so the line-spec parsing is unit-testable
// without pulling React in.
//
// An annotation targets a 1-based line ref written as a single line ("3") or an
// inclusive range ("3-5"). The block historically also stored the ref under a
// numeric `line` field; both shapes are accepted here. Notes carry a `note`
// string and an optional short `label`.
//
// Robustness: every parser is forgiving and never throws. Malformed or fully
// out-of-range refs resolve to `null` so callers can ignore them gracefully
// while still rendering the code. `parseAnnotationsAttr` returns `null` (not an
// empty list) when the JSON itself is invalid, so the caller can distinguish
// "no annotations" from "bad input" and surface a quiet warning.

/** A raw annotation as authored on the block (before line refs are resolved). */
export interface RawAnnotation {
  /** 1-based line ref: a single line ("3") or an inclusive range ("3-5"). */
  lines: string;
  /** Optional short emphasis shown before the note. */
  label?: string;
  /** The note body (plain text). */
  note: string;
}

/** An inclusive 1-based line span, clamped to the file's line count. */
export interface LineRange {
  start: number;
  end: number;
}

/** A raw annotation resolved against the code's line count. */
export interface ResolvedAnnotation {
  /** Stable index into the original annotation list (hover/cross-highlight key). */
  index: number;
  /** 1-based marker number in authoring order. */
  marker: number;
  annotation: RawAnnotation;
  /** Resolved span, or `null` when the ref was malformed / out of range. */
  range: LineRange | null;
}

const LINE_RANGE_RE = /^\s*(\d+)\s*(?:-\s*(\d+)\s*)?$/;

/**
 * Parse a 1-based `lines` ref ("3" or "3-5") into an inclusive `[start, end]`
 * pair clamped to `[1, lineCount]`. Returns `null` for malformed refs or refs
 * that fall fully outside the file. A reversed range ("5-3") is normalized; a
 * partially out-of-range range is clamped.
 */
export function parseLineRange(
  ref: string,
  lineCount: number,
): LineRange | null {
  const match = LINE_RANGE_RE.exec(ref);
  if (!match) return null;
  let start = Number.parseInt(match[1], 10);
  let end = match[2] != null ? Number.parseInt(match[2], 10) : start;
  if (!Number.isFinite(start) || !Number.isFinite(end)) return null;
  if (start > end) [start, end] = [end, start];
  if (lineCount <= 0) return null;
  // Fully outside the file → ignore.
  if (end < 1 || start > lineCount) return null;
  return { start: Math.max(1, start), end: Math.min(lineCount, end) };
}

/**
 * Coerce one loosely-typed annotation entry (as decoded from the JSON attr) into
 * a {@link RawAnnotation}, or `null` if it carries no usable note/line ref. The
 * legacy `{ line: number, note }` shape and the new `{ lines: string }` shape
 * are both accepted; `line` is normalized to a string `lines` ref.
 */
export function coerceAnnotation(value: unknown): RawAnnotation | null {
  if (!value || typeof value !== "object") return null;
  const record = value as Record<string, unknown>;

  let lines: string | undefined;
  if (typeof record.lines === "string") {
    lines = record.lines.trim();
  } else if (typeof record.lines === "number") {
    lines = String(record.lines);
  } else if (typeof record.line === "string") {
    lines = record.line.trim();
  } else if (typeof record.line === "number") {
    lines = String(record.line);
  }
  if (!lines) return null;

  const note = typeof record.note === "string" ? record.note : "";
  const label =
    typeof record.label === "string" && record.label.trim()
      ? record.label.trim()
      : undefined;
  if (!note && !label) return null;

  return { lines, note, label };
}

/**
 * Parse the block's `annotations` attribute (a JSON string of annotation
 * objects) into a forgiving list. Returns `null` when the JSON is invalid OR is
 * not an array, so the caller can render the code without annotations AND
 * surface a quiet warning — never silently swallowing a malformed attr. Returns
 * an empty array when the JSON is a valid-but-empty list.
 */
export function parseAnnotationsAttr(
  raw: string | undefined,
): RawAnnotation[] | null {
  if (raw == null || raw.trim() === "") return [];
  let decoded: unknown;
  try {
    decoded = JSON.parse(raw);
  } catch {
    return null;
  }
  if (!Array.isArray(decoded)) return null;
  return decoded
    .map(coerceAnnotation)
    .filter((entry): entry is RawAnnotation => entry !== null);
}

/**
 * Resolve a raw annotation list into stable, marker-numbered records, parsing
 * each `lines` ref against `lineCount`. Markers are authoring-order, 1-based,
 * and assigned to ALL annotations (even unresolved ones) so numbering is stable
 * regardless of which refs happen to match.
 */
export function resolveAnnotations(
  annotations: readonly RawAnnotation[] | undefined,
  lineCount: number,
): ResolvedAnnotation[] {
  return (annotations ?? []).map((annotation, index) => ({
    index,
    marker: index + 1,
    annotation,
    range: parseLineRange(annotation.lines, lineCount),
  }));
}

/** Map a 1-based line number → the resolved annotations covering it. */
export function buildLineMarkerMap(
  resolved: readonly ResolvedAnnotation[],
): Map<number, ResolvedAnnotation[]> {
  const map = new Map<number, ResolvedAnnotation[]>();
  for (const item of resolved) {
    if (!item.range) continue;
    for (let n = item.range.start; n <= item.range.end; n += 1) {
      const list = map.get(n) ?? [];
      list.push(item);
      map.set(n, list);
    }
  }
  return map;
}

/** Human label for a resolved annotation's span ("Line 8" / "Lines 3–6"). */
export function rangeLabel(item: ResolvedAnnotation): string {
  if (!item.range) return `Lines ${item.annotation.lines}`;
  return item.range.start === item.range.end
    ? `Line ${item.range.start}`
    : `Lines ${item.range.start}–${item.range.end}`;
}

/** Whether a resolved list has at least one annotation worth a rail/marker. */
export function hasResolvedAnnotations(
  items: readonly ResolvedAnnotation[],
): boolean {
  return items.some((item) => item.range !== null);
}
