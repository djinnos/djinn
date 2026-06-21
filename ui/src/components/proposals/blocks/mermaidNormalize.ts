/**
 * Mermaid source normalization.
 *
 * LLM- and human-authored diagram sources frequently trip Mermaid's flowchart
 * grammar in two ways:
 *
 *  1. "Prettified" unicode arrow/dash glyphs (`→`, `⟶`, `⇒`, `➜`, em/en dashes)
 *     where the grammar wants the ASCII edge operators `-->` / `--`.
 *  2. Node labels with unquoted special characters — `(`, `)`, `:`, `-D`,
 *     `--check`, etc. — e.g. `A[clippy -D warnings (exists)]`. Mermaid's parser
 *     reads the inner `(` as the start of a round-node shape and chokes.
 *
 * `normalizeMermaidSource` fixes both, conservatively:
 *
 *  - Arrow/dash edge sequences outside labels are rewritten to ASCII operators.
 *    Glyphs inside node/edge labels (`[...]`, `(...)`, `{...}`, `"..."`) are left
 *    untouched, so `A["price → total"]` keeps its arrow.
 *  - Node-shape labels (`ID[...]`, `ID(...)`, `ID{...}`, and the compound shapes
 *    `([...])`, `[[...]]`, `((...))`, `{{...}}`, `[(...)]`, `>...]`) get their
 *    inner text wrapped in double quotes when it isn't already quoted, with any
 *    inner `"` escaped to Mermaid's `#quot;` entity. Already-quoted labels are
 *    left as-is, so the transform is idempotent.
 *
 * Anything still unparseable after this is handled by the caller's graceful
 * copy-source fallback.
 */

// Unicode arrow glyphs that authors use in place of the `-->` edge operator.
//   → U+2192  ⟶ U+27F6  ⇒ U+21D2  ➜ U+279C  ⇨ U+21E8  ➡ U+27A1  ➔ U+2794
const ARROW_GLYPHS = "→⟶⇒➜⇨➡➔➙➞";
// Em/en dash + horizontal bar, used in place of the `--` link operator.
const DASH_GLYPHS = "—–―";

const ARROW_CLASS = `[${ARROW_GLYPHS}]`;
const DASH_CLASS = `[${DASH_GLYPHS}]`;

// A directed edge: an optional leading dash run, then an arrow glyph, bounded by
// whitespace (or line edges) so we don't rewrite a glyph embedded in a word.
const ARROW_EDGE = new RegExp(
  `(^|\\s)(?:${DASH_CLASS}+\\s*)?${ARROW_CLASS}(\\s|$)`,
  "gu",
);
// An undirected link: a bare run of dashes bounded by whitespace.
const DASH_EDGE = new RegExp(`(^|\\s)${DASH_CLASS}+(\\s|$)`, "gu");

/**
 * The recognized node-shape bracket pairs, longest opener first so the compound
 * shapes win over the single-char ones during matching. `open` is the literal
 * opening delimiter; `close` is the matching closing delimiter that ends the
 * label. The label text is whatever sits between them.
 *
 * `>` (the asymmetric/flag shape `id>text]`) opens with `>` and closes with `]`.
 */
const SHAPES: ReadonlyArray<{ open: string; close: string }> = [
  { open: "([", close: "])" }, // stadium
  { open: "[[", close: "]]" }, // subroutine
  { open: "[(", close: ")]" }, // cylinder
  { open: "((", close: "))" }, // circle
  { open: "{{", close: "}}" }, // hexagon
  { open: "[", close: "]" }, // rectangle
  { open: "(", close: ")" }, // round
  { open: "{", close: "}" }, // rhombus
  { open: ">", close: "]" }, // asymmetric / flag
];

// A node id directly precedes a shape opener: word chars (Mermaid ids), with no
// whitespace between the id and the bracket.
const ID_CHAR = /[A-Za-z0-9_]/;

/** Wrap a raw label in double quotes unless it's already a quoted string. */
function quoteLabel(inner: string): string {
  const trimmed = inner.trim();
  // Already a single fully-quoted string → leave it (idempotent).
  if (trimmed.length >= 2 && trimmed.startsWith('"') && trimmed.endsWith('"')) {
    return inner;
  }
  // Empty label → nothing to quote.
  if (trimmed.length === 0) return inner;
  // Escape any inner double quotes to Mermaid's entity, then wrap.
  const escaped = inner.replace(/"/g, "#quot;");
  return `"${escaped}"`;
}

/**
 * Auto-quote node-shape labels on a single line.
 *
 * Scans left-to-right. Outside a label, when a shape opener is found that is
 * immediately preceded by a node-id character, the matching close delimiter is
 * located and the inner text quoted. Edge labels (`|...|`) and quoted strings
 * are skipped verbatim so we never double-process or touch pipe labels.
 */
function quoteNodeLabels(line: string): string {
  let out = "";
  let i = 0;
  const n = line.length;

  while (i < n) {
    const ch = line[i];

    // Skip a pipe-delimited edge label verbatim: `|...|`.
    if (ch === "|") {
      const end = line.indexOf("|", i + 1);
      if (end === -1) {
        out += line.slice(i);
        break;
      }
      out += line.slice(i, end + 1);
      i = end + 1;
      continue;
    }

    // Skip a bare quoted string verbatim (not part of a shape).
    if (ch === '"') {
      const end = line.indexOf('"', i + 1);
      if (end === -1) {
        out += line.slice(i);
        break;
      }
      out += line.slice(i, end + 1);
      i = end + 1;
      continue;
    }

    // Try to match a node-shape opener at this position, but only when it is
    // glued to a node id (the preceding emitted char is an id char). This keeps
    // us from treating an edge operator's parens or a stray `(` as a shape.
    const prev = out.length > 0 ? out[out.length - 1] : "";
    if (ID_CHAR.test(prev)) {
      const shape = SHAPES.find((s) => line.startsWith(s.open, i));
      if (shape) {
        const contentStart = i + shape.open.length;
        const closeIdx = line.indexOf(shape.close, contentStart);
        if (closeIdx !== -1) {
          const inner = line.slice(contentStart, closeIdx);
          out += shape.open + quoteLabel(inner) + shape.close;
          i = closeIdx + shape.close.length;
          continue;
        }
      }
    }

    out += ch;
    i += 1;
  }

  return out;
}

/**
 * Normalize the structural (outside-label) arrow/dash glyphs on a single line.
 *
 * Labels (`"..."`, `[...]`, `(...)`, `{...}`) are copied verbatim so an arrow
 * inside a label is preserved; only the edge operators between nodes change.
 */
function normalizeArrowsLine(line: string): string {
  let out = "";
  let buf = "";
  let closing: string | null = null;

  const flushOutside = () => {
    if (!buf) return;
    // Directed edges first (arrow glyph, optionally preceded by dashes), then
    // any remaining bare dash runs become undirected links.
    let seg = buf.replace(
      ARROW_EDGE,
      (_m, pre: string, post: string) => `${pre}-->${post}`,
    );
    seg = seg.replace(
      DASH_EDGE,
      (_m, pre: string, post: string) => `${pre}--${post}`,
    );
    out += seg;
    buf = "";
  };

  for (const ch of line) {
    if (closing) {
      out += ch;
      if (ch === closing) closing = null;
      continue;
    }
    if (ch === '"') {
      flushOutside();
      out += ch;
      closing = '"';
      continue;
    }
    if (ch === "[" || ch === "(" || ch === "{") {
      flushOutside();
      out += ch;
      closing = ch === "[" ? "]" : ch === "(" ? ")" : "}";
      continue;
    }
    buf += ch;
  }
  flushOutside();
  return out;
}

/**
 * Normalize a Mermaid source so LLM/human-authored diagrams parse:
 *  1. unicode arrow/dash edge glyphs → ASCII `-->` / `--`,
 *  2. unquoted node-shape labels → quoted (special chars made safe).
 *
 * Conservative and idempotent: glyph-free sources skip the arrow pass, and
 * already-quoted labels are left untouched.
 */
export function normalizeMermaidSource(source: string): string {
  if (!source) return source;

  const hasGlyph = new RegExp(`${ARROW_CLASS}|${DASH_CLASS}`, "u").test(source);

  return source
    .split("\n")
    .map((line) => {
      const arrowed = hasGlyph ? normalizeArrowsLine(line) : line;
      return quoteNodeLabels(arrowed);
    })
    .join("\n");
}
