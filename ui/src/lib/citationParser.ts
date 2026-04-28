/**
 * citationParser — extract `[[file:...]]` and `[[symbol:Type:Name]]` tokens
 * from chat markdown so the renderer can rewrite them into clickable
 * anchors.
 *
 * Citation forms (per plan §"Citation format & resolution flow"):
 *
 *   [[file:path/to/file.rs:42-58]]
 *   [[file:path/to/file.rs:42]]
 *   [[file:path/to/file.rs]]
 *   [[symbol:Type:Name]]
 *
 * Anything that doesn't fit those shapes is left as plain text — the
 * renderer concatenates the un-cited segments back into the model's
 * original output verbatim.
 */

export interface FileCitation {
  kind: "file";
  /** Full token including `[[ ]]`, e.g. `[[file:src/foo.rs:42-58]]`. */
  raw: string;
  /** Repo-relative path as written. */
  path: string;
  /** First line in the optional range; null when no range was provided. */
  startLine: number | null;
  /** Last line in the optional range; null when no range was provided. */
  endLine: number | null;
}

export interface SymbolCitation {
  kind: "symbol";
  /** Full token including `[[ ]]`. */
  raw: string;
  /** SCIP symbol kind hint (e.g. `Function`, `Class`, `Method`). */
  symbolKind: string;
  /** Short name used as the `query` for `code_graph search`. */
  name: string;
}

export type ParsedCitation = FileCitation | SymbolCitation;

export type CitationSegment =
  | { type: "text"; text: string }
  | { type: "citation"; citation: ParsedCitation };

/**
 * Greedy match for `[[file:...]]` or `[[symbol:...]]`. Must match before
 * the closing `]]` so we don't swallow other markdown wikilinks. The
 * regex is intentionally permissive in the inner segment — callers
 * validate the parts in {@link parseCitationToken}.
 *
 * `g` flag → reusable across segments via `String.matchAll`.
 */
export const CITATION_REGEX = /\[\[(file|symbol):([^\]]+)\]\]/g;

/**
 * Parse a single matched token (the *inside* of `[[...]]`) into a
 * structured citation. Returns `null` if the body is malformed —
 * callers should leave the original text in place.
 */
export function parseCitationToken(
  kind: "file" | "symbol",
  body: string,
  raw: string,
): ParsedCitation | null {
  if (kind === "file") {
    return parseFileBody(body, raw);
  }
  return parseSymbolBody(body, raw);
}

function parseFileBody(body: string, raw: string): FileCitation | null {
  // The path may itself contain colons on Windows-style paths or in
  // odd repos, but in practice it doesn't — we treat the *last* colon
  // as the line-range separator. If there's no trailing `:N` or
  // `:N-M`, treat the whole body as the path.
  const trimmed = body.trim();
  if (trimmed.length === 0) return null;

  const lastColon = trimmed.lastIndexOf(":");
  if (lastColon === -1) {
    return {
      kind: "file",
      raw,
      path: trimmed,
      startLine: null,
      endLine: null,
    };
  }

  const tail = trimmed.slice(lastColon + 1);
  const head = trimmed.slice(0, lastColon);
  const range = parseLineRange(tail);
  if (!range) {
    // Trailing segment didn't look like a line range — treat the
    // whole body as the path. This is the conservative fallback
    // for paths that legitimately contain `:`.
    return {
      kind: "file",
      raw,
      path: trimmed,
      startLine: null,
      endLine: null,
    };
  }

  if (head.length === 0) return null;

  return {
    kind: "file",
    raw,
    path: head,
    startLine: range.start,
    endLine: range.end,
  };
}

function parseLineRange(s: string): { start: number; end: number | null } | null {
  if (/^\d+$/.test(s)) {
    return { start: Number(s), end: Number(s) };
  }
  const m = s.match(/^(\d+)-(\d+)$/);
  if (!m) return null;
  const start = Number(m[1]);
  const end = Number(m[2]);
  if (start > end) return null;
  return { start, end };
}

function parseSymbolBody(body: string, raw: string): SymbolCitation | null {
  // Form: `Type:Name`. Names may contain `::` (Rust paths), so once
  // we've split off the first colon-separated segment as the kind,
  // everything else is the name.
  const trimmed = body.trim();
  const firstColon = trimmed.indexOf(":");
  if (firstColon === -1) return null;
  const symbolKind = trimmed.slice(0, firstColon).trim();
  const name = trimmed.slice(firstColon + 1).trim();
  if (symbolKind.length === 0 || name.length === 0) return null;
  return { kind: "symbol", raw, symbolKind, name };
}

/**
 * Walk a chat message body and split it into a flat list of text /
 * citation segments. The order is preserved so a renderer can map
 * over the segments and emit either plain text or a clickable anchor.
 */
export function segmentByCitations(text: string): CitationSegment[] {
  if (!text) return [];

  const segments: CitationSegment[] = [];
  let cursor = 0;

  // Reset lastIndex defensively — the global regex is shared.
  CITATION_REGEX.lastIndex = 0;

  for (const match of text.matchAll(CITATION_REGEX)) {
    const matchIndex = match.index ?? 0;
    if (matchIndex > cursor) {
      segments.push({ type: "text", text: text.slice(cursor, matchIndex) });
    }

    const raw = match[0];
    const kind = match[1] as "file" | "symbol";
    const body = match[2] ?? "";

    const citation = parseCitationToken(kind, body, raw);
    if (citation) {
      segments.push({ type: "citation", citation });
    } else {
      // Malformed → preserve verbatim so the user still sees what the
      // model emitted.
      segments.push({ type: "text", text: raw });
    }

    cursor = matchIndex + raw.length;
  }

  if (cursor < text.length) {
    segments.push({ type: "text", text: text.slice(cursor) });
  }

  // Coalesce adjacent text segments — happens when malformed
  // citations are pushed back as text and bracket text on either
  // side. Keeps downstream rendering tidy.
  return coalesceText(segments);
}

function coalesceText(segs: CitationSegment[]): CitationSegment[] {
  const out: CitationSegment[] = [];
  for (const s of segs) {
    const last = out[out.length - 1];
    if (s.type === "text" && last?.type === "text") {
      last.text += s.text;
    } else {
      out.push(s);
    }
  }
  return out;
}

/**
 * Convenience: extract every citation in a message in source order,
 * skipping malformed tokens. Used by `<CodeRefsPanel>` to enumerate
 * cited refs without re-rendering the prose.
 */
export function extractCitations(text: string): ParsedCitation[] {
  return segmentByCitations(text).flatMap((seg) =>
    seg.type === "citation" ? [seg.citation] : [],
  );
}

/**
 * Build the canonical node id for a file citation.
 *
 * The snapshot bridge formats file keys as `file:<repo-relative-path>`
 * (`server/src/mcp_bridge/graph_neighbors.rs::format_node_key`). Mirror
 * that here so the chat path can dispatch a setCitation call without
 * waiting on a server round-trip — file citations resolve directly.
 */
export function fileCitationToNodeId(citation: FileCitation): string {
  return `file:${citation.path}`;
}
