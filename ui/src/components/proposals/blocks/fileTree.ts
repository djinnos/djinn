// Pure (React-free) helpers for the interactive file-tree block. These parse the
// freeform tree text an author writes inside <FileTree>…</FileTree> into a flat
// list of files (with a change status + note), then fold those flat slash-paths
// into a nested folder tree. Kept in their own module so they are unit-testable
// without pulling React in.
//
// djinn's MDX stores the tree as the block's *children text* (see the Rust
// `validation.rs` fixture: `<FileTree root="src"> src/\n  main.rs </FileTree>`),
// NOT as a structured `entries={…}` JSON prop. So the parser tolerates BOTH of
// the two authoring styles seen in the wild:
//
//   1. Indented ASCII tree — folders end in `/` (or simply have children under a
//      deeper indent), files sit beneath them:
//        src/
//          ~ main.rs
//          routes/
//            + git.ts
//
//   2. One slash-path per line:
//        src/main.rs
//        ~ src/routes/git.ts  -- why it changed
//
// ── Declared status-token grammar ──────────────────────────────────────────
// A file's change status is DECLARED by the author with a single leading token,
// never inferred from English words. The token, when present, is the FIRST
// non-whitespace character of the row (after any ASCII tree-drawing glyphs like
// `├ └ │` have been stripped) and MUST be immediately followed by whitespace and
// then the path:
//
//        +  path   → added      (a.k.a. new)
//        ~  path   → modified
//        -  path   → removed    (deleted); `−` (U+2212) is accepted too
//        >  path   → renamed    (moved)
//     (none) path  → unchanged
//
// Token vs bullet-glyph collision: `+` and `-` are ALSO common list bullets, so
// the declared token is resolved FIRST and takes precedence — a leading `+`/`-`/
// `~`/`>` followed by whitespace is ALWAYS a status token, not a bullet. Only the
// `*` glyph (which is not a status token) is still treated as a plain bullet, so
// `* foo.ts` is an unchanged file. A token must be followed by whitespace: `-x`
// (no space) is not a token and is parsed as the path `-x`.
//
// A trailing note/description after the path (e.g. `Cargo.toml — sets lints` or
// `foo.ts: does X`) is still surfaced as a muted caption. Anything the parser
// can't make sense of yields an empty file list, so the caller can fall back to
// the raw <pre>.

/** The kind of change applied to a file, driving its single-letter badge. */
export type FileChange = "added" | "modified" | "deleted" | "renamed";

/** A single parsed file: its full slash path plus optional status + note. */
export interface ParsedFile {
  /** Full slash-delimited path, e.g. `src/routes/git.ts`. */
  path: string;
  /** Last path segment (file name). */
  name: string;
  change?: FileChange;
  /** Trailing human note after the path, if any. */
  note?: string;
}

/** A folder node in the derived nested tree. */
export interface FolderNode {
  kind: "folder";
  /** Display name (may be a compacted `a/b/c` chain). */
  name: string;
  /** Full slash path of the folder — a stable key. */
  path: string;
  children: TreeNode[];
}

/** A file leaf in the derived nested tree. */
export interface FileNode {
  kind: "file";
  name: string;
  path: string;
  change?: FileChange;
  note?: string;
}

export type TreeNode = FolderNode | FileNode;

/** Tally of changes across the tree, for the summary header. */
export interface ChangeTally {
  added: number;
  modified: number;
  deleted: number;
  renamed: number;
}

/* ── Declared status token + note parsing ──────────────────────────────────── */

/** Maps a leading declared status token to its change kind. */
const STATUS_TOKENS: Record<string, FileChange> = {
  "+": "added",
  "~": "modified",
  "-": "deleted",
  "−": "deleted", // U+2212 MINUS SIGN, as some authors paste it.
  ">": "renamed",
};

/**
 * Pull a leading DECLARED status token out of a row. The token must be the first
 * character and be immediately followed by whitespace (so `-x` with no space is
 * NOT a token — it's a literal path). On a match the token + its trailing
 * whitespace are stripped and the matching change is returned; otherwise the row
 * is returned unchanged with no status. Status is NEVER inferred from words.
 */
function extractStatusToken(row: string): { change?: FileChange; rest: string } {
  const first = row[0] ?? "";
  const change = STATUS_TOKENS[first];
  // Require whitespace after the token so a path that merely starts with the
  // character (e.g. `~/home`, `-rf`) is not mistaken for a declared status.
  if (change && /\s/.test(row[1] ?? "")) {
    return { change, rest: row.slice(1).replace(/^\s+/u, "") };
  }
  return { rest: row };
}

/**
 * Status tokens safe to detect when they TRAIL the path (`planner.rs ~`,
 * `skills.rs +`). Deliberately excludes `-` and `>`, which collide with the
 * dash note-separator and quote markers — those stay LEADING-only.
 */
const TRAILING_STATUS_TOKENS: Record<string, FileChange> = {
  "+": "added",
  "~": "modified",
  "−": "deleted", // U+2212 MINUS SIGN.
};

/**
 * Recover a status token that the author placed AFTER the path instead of
 * before it. Only applied when the row carried no leading token, and only for
 * the unambiguous glyphs ({@link TRAILING_STATUS_TOKENS}). The token may stand
 * alone (`foo.rs ~`) or precede the real note (`foo.rs + new helper`). Returns
 * the trailing text unchanged when it isn't a recognized status.
 */
function extractTrailingStatus(trailing: string): {
  change?: FileChange;
  rest: string;
} {
  const text = trailing.trim();
  if (!text) return { rest: trailing };
  const change = TRAILING_STATUS_TOKENS[text[0] ?? ""];
  if (change && (text.length === 1 || /\s/.test(text[1] ?? ""))) {
    return { change, rest: text.slice(1).trim() };
  }
  return { rest: trailing };
}

/** Strip a leading note separator (dash/colon/hash) and unwrap a `(note)`. */
function cleanNote(text: string): string | undefined {
  let noteText = text.replace(/^[—–\-:#]+\s*/u, "").trim();
  const wrapped = /^\(([\s\S]*)\)$/.exec(noteText);
  if (wrapped) noteText = (wrapped[1] ?? "").trim();
  return noteText || undefined;
}

/**
 * Split one authored row (already de-indented) into its leading token (a path or
 * segment), change status, and note. Returns null when the leading token doesn't
 * look like a path/segment.
 */
function parseRow(raw: string): ParsedFile | null {
  let rest = raw.trim();
  if (!rest) return null;
  // Strip ASCII tree-drawing glyphs first (purely structural, never a status).
  rest = rest.replace(/^[│|`'*\s]*[├└][\s─]*/u, "").trim();
  rest = rest.replace(/^[│|]\s*/u, "").trim();
  if (!rest) return null;

  // Resolve a DECLARED leading status token (`+ ~ - >`) BEFORE any bullet
  // stripping, so the token wins the collision with `+`/`-` list bullets.
  const status = extractStatusToken(rest);
  let change = status.change;
  rest = status.rest;
  // Only the `*` glyph remains a plain bullet now (`+`/`-` are status tokens).
  rest = rest.replace(/^\*\s+/u, "").trim();
  if (!rest) return null;

  const firstSpace = rest.search(/\s/);
  let path = rest;
  let trailing = "";
  if (firstSpace !== -1) {
    path = rest.slice(0, firstSpace);
    trailing = rest.slice(firstSpace + 1).trim();
  }

  // A trailing `:` on the path token introduces a note (`src/x.ts: helper`).
  const colon = path.indexOf(":");
  if (colon !== -1) {
    const after = path.slice(colon + 1);
    path = path.slice(0, colon);
    trailing = after ? `${after} ${trailing}`.trim() : trailing;
  }

  // A path token must not contain angle brackets and must have real content.
  if (!path || /[<>]/.test(path)) return null;

  // No leading token? Some authors put the status AFTER the path (`foo.rs ~`).
  if (!change && trailing) {
    const tail = extractTrailingStatus(trailing);
    if (tail.change) {
      change = tail.change;
      trailing = tail.rest;
    }
  }

  const note: string | undefined = trailing ? cleanNote(trailing) : undefined;

  const isFolder = path.endsWith("/");
  const cleanPath = path.replace(/\/+$/, "");
  if (!cleanPath) return null;
  // Reject tokens with no path-ish character at all (pure punctuation).
  if (!/[A-Za-z0-9._]/.test(cleanPath)) return null;

  // A bare folder row (e.g. `src/`) with no status/note carries no file — its
  // descendants supply the folder structure via their own paths.
  if (isFolder && !change && !note) return null;

  const segments = cleanPath.split("/").filter(Boolean);
  const name = segments[segments.length - 1] ?? cleanPath;
  return { path: cleanPath, name, change, note };
}

/** Parse a single tree row's own SEGMENT name (not a full path) + status + note. */
function parseRowSegment(raw: string): {
  segment: string;
  rawEndsWithSlash: boolean;
  change?: FileChange;
  note?: string;
} | null {
  let rest = raw.trim();
  rest = rest.replace(/^[├└][\s─]*/u, "").trim();
  rest = rest.replace(/^[│|]\s*/u, "").trim();
  if (!rest) return null;

  // DECLARED status token wins over `+`/`-` bullets; resolve it first.
  const status = extractStatusToken(rest);
  let change = status.change;
  rest = status.rest;
  rest = rest.replace(/^\*\s+/u, "").trim();
  if (!rest) return null;

  const firstSpace = rest.search(/\s/);
  let token = rest;
  let trailing = "";
  if (firstSpace !== -1) {
    token = rest.slice(0, firstSpace);
    trailing = rest.slice(firstSpace + 1).trim();
  }
  if (!token || /[<>]/.test(token)) return null;

  const rawEndsWithSlash = token.endsWith("/");
  const segment = token.replace(/\/+$/, "");
  if (!segment || !/[A-Za-z0-9._]/.test(segment)) return null;

  // No leading token? Recover a trailing status (`planner.rs ~`).
  if (!change && trailing) {
    const tail = extractTrailingStatus(trailing);
    if (tail.change) {
      change = tail.change;
      trailing = tail.rest;
    }
  }

  const note: string | undefined = trailing ? cleanNote(trailing) : undefined;

  return { segment, rawEndsWithSlash, change, note };
}

/** Indentation width of a line; tabs count as 2 columns. */
function indentWidth(line: string): number {
  let width = 0;
  for (const ch of line) {
    if (ch === " ") width += 1;
    else if (ch === "\t") width += 2;
    else break;
  }
  return width;
}

/**
 * Reconstruct full slash-paths from an indentation-based ASCII tree. Each row's
 * indentation depth maps to a folder stack; a row ending in `/` (or one whose
 * next row is more deeply indented) is a folder, others are files.
 */
function parseIndentedTree(lines: string[]): ParsedFile[] {
  const rows = lines
    .map((line) => ({ indent: indentWidth(line), text: line.trim() }))
    .filter((r) => r.text.length > 0);
  if (rows.length === 0) return [];

  const hasIndent = rows.some((r) => r.indent > 0);
  if (!hasIndent) return [];

  const files: ParsedFile[] = [];
  const stack: { indent: number; segment: string }[] = [];

  for (let i = 0; i < rows.length; i++) {
    const row = rows[i]!;
    while (stack.length > 0 && stack[stack.length - 1]!.indent >= row.indent) {
      stack.pop();
    }

    const parsed = parseRowSegment(row.text);
    if (!parsed) continue;

    // A single-token "segment" that itself contains slashes is a slash-path
    // pasted into an indented tree; treat it as a full path from the stack root.
    const next = rows[i + 1];
    const isFolderByIndent = next ? next.indent > row.indent : false;
    const isFolder = parsed.rawEndsWithSlash || isFolderByIndent;

    const prefix = stack.map((s) => s.segment).join("/");
    const fullPath = prefix
      ? `${prefix}/${parsed.segment}`
      : parsed.segment;

    if (isFolder) {
      stack.push({ indent: row.indent, segment: parsed.segment });
      if (parsed.change || parsed.note) {
        files.push({
          path: fullPath,
          name: parsed.segment.split("/").pop() ?? parsed.segment,
          change: parsed.change,
          note: parsed.note,
        });
      }
    } else {
      files.push({
        path: fullPath,
        name: parsed.segment.split("/").pop() ?? parsed.segment,
        change: parsed.change,
        note: parsed.note,
      });
    }
  }

  return files;
}

/**
 * Parse the freeform <FileTree> body into a flat list of files. Tries the
 * indentation-based ASCII-tree interpretation first (the legacy authoring
 * style); if that yields nothing, falls back to treating each non-empty line as
 * an independent slash-path. Returns `[]` for input it can't parse so the caller
 * can fall back to a raw render.
 */
export function parseFileTreeBody(body: string): ParsedFile[] {
  if (!body || !body.trim()) return [];
  const lines = body.replace(/\r\n?/g, "\n").split("\n");

  const indented = parseIndentedTree(lines);
  if (indented.length > 0) return dedupe(indented);

  const flat: ParsedFile[] = [];
  for (const line of lines) {
    const parsed = parseRow(line);
    if (parsed) flat.push(parsed);
  }
  return dedupe(flat);
}

/** Drop duplicate paths, keeping the first (which preserves authored order). */
function dedupe(files: ParsedFile[]): ParsedFile[] {
  const seen = new Set<string>();
  const out: ParsedFile[] = [];
  for (const file of files) {
    if (seen.has(file.path)) continue;
    seen.add(file.path);
    out.push(file);
  }
  return out;
}

/* ── Tree construction (flat paths → nested folders) ───────────────────────── */

interface FolderBuild {
  name: string;
  path: string;
  folders: Map<string, FolderBuild>;
  files: FileNode[];
  order: string[];
}

function makeFolder(name: string, path: string): FolderBuild {
  return { name, path, folders: new Map(), files: [], order: [] };
}

/**
 * Build a nested folder tree from flat files. Folders are derived purely from
 * each path's slash segments; insertion order is preserved within a folder, with
 * folders sorted before files at each level (conventional explorer ordering).
 * Single-child folder chains are compacted into one `a/b/c` row.
 */
export function buildFileTree(files: ParsedFile[]): TreeNode[] {
  const root = makeFolder("", "");

  for (const file of files) {
    const segments = file.path.split("/").filter(Boolean);
    if (segments.length === 0) continue;
    const fileName = segments[segments.length - 1]!;
    const folderSegments = segments.slice(0, -1);

    let cursor = root;
    let prefix = "";
    for (const segment of folderSegments) {
      prefix = prefix ? `${prefix}/${segment}` : segment;
      let next = cursor.folders.get(segment);
      if (!next) {
        next = makeFolder(segment, prefix);
        cursor.folders.set(segment, next);
        cursor.order.push(`d:${segment}`);
      }
      cursor = next;
    }

    cursor.files.push({
      kind: "file",
      name: fileName,
      path: file.path,
      change: file.change,
      note: file.note,
    });
    cursor.order.push(`f:${cursor.files.length - 1}`);
  }

  const materialize = (folder: FolderBuild): TreeNode[] => {
    const nodes: TreeNode[] = [];
    for (const key of folder.order) {
      if (key.startsWith("d:")) {
        const child = folder.folders.get(key.slice(2));
        if (!child) continue;
        nodes.push({
          kind: "folder",
          name: child.name,
          path: child.path,
          children: materialize(child),
        });
      } else {
        const file = folder.files[Number(key.slice(2))];
        if (file) nodes.push(file);
      }
    }
    return [
      ...nodes.filter((node) => node.kind === "folder"),
      ...nodes.filter((node) => node.kind === "file"),
    ];
  };

  return compactTree(materialize(root));
}

/** Collapse single-child folder chains into one `a/b/c` row (explorer style). */
function compactFolderNode(folder: FolderNode): FolderNode {
  const names = [folder.name];
  let path = folder.path;
  let children = folder.children;

  while (children.length === 1 && children[0]?.kind === "folder") {
    const child = children[0];
    names.push(child.name);
    path = child.path;
    children = child.children;
  }

  return {
    kind: "folder",
    name: names.join("/"),
    path,
    children: compactTree(children),
  };
}

function compactTree(nodes: TreeNode[]): TreeNode[] {
  return nodes.map((node) =>
    node.kind === "folder" ? compactFolderNode(node) : node,
  );
}

/** Tally the change kinds across a flat file list, for the summary header. */
export function tallyChanges(files: ParsedFile[]): ChangeTally {
  const tally: ChangeTally = { added: 0, modified: 0, deleted: 0, renamed: 0 };
  for (const file of files) {
    if (file.change) tally[file.change] += 1;
  }
  return tally;
}
