/**
 * Mermaid source normalization.
 *
 * LLM- and human-authored diagram sources frequently use "prettified" unicode
 * arrow and dash glyphs (`→`, `⟶`, `⇒`, `➜`, em/en dashes) where Mermaid's
 * grammar requires the ASCII edge operators `-->` / `--`. Passing those glyphs
 * straight to `mermaid.render()` throws a parser error that we used to dump raw
 * to users.
 *
 * `normalizeMermaidSource` rewrites only the arrow-/dash-like *edge* sequences
 * to their ASCII equivalents. It is intentionally conservative: it never touches
 * glyphs that appear inside node/edge *labels* (text in `[...]`, `(...)`,
 * `{...}`, `"..."`), so a label like `A["price → total"]` keeps its arrow.
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
 * Normalize the structural (outside-label) part of a single line.
 *
 * Labels (`"..."`, `[...]`, `(...)`, `{...}`) are copied verbatim so an arrow
 * inside a label is preserved; only the edge operators between nodes change.
 */
function normalizeLine(line: string): string {
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
 * Normalize unicode arrow/dash edge glyphs to Mermaid's ASCII operators.
 * Conservative: only structural (outside-label) glyphs are rewritten, and the
 * source is returned untouched when no candidate glyph is present.
 */
export function normalizeMermaidSource(source: string): string {
  if (!source) return source;
  const hasCandidate = new RegExp(`${ARROW_CLASS}|${DASH_CLASS}`, "u").test(
    source,
  );
  if (!hasCandidate) return source;
  return source.split("\n").map(normalizeLine).join("\n");
}
