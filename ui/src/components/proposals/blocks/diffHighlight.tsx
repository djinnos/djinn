import type { ReactNode } from "react";
import refractor from "refractor";
import createElement from "react-syntax-highlighter/dist/esm/create-element";
import { oneDark } from "react-syntax-highlighter/dist/esm/styles/prism";

/**
 * The Prism token-highlighting surface for the Diff block, split into its own
 * module so `DiffBlock` can lazy-load it. We deliberately reuse the EXACT same
 * infrastructure as `CodeBlock.tsx`:
 *   - `refractor` is the very AST generator `react-syntax-highlighter`'s `Prism`
 *     export wraps (its `prism.js` does `import refractor from "refractor"`), so
 *     importing it here adds no second highlighter — every language grammar is
 *     already registered.
 *   - RSH's `createElement` turns a refractor hast node into a React element,
 *     applying the `oneDark` token styles inline (the same `style={oneDark}` the
 *     CodeBlock surface passes). This gives diff tokens the identical language
 *     colours the standalone Code block uses.
 *
 * Why per-LINE highlighting (not one whole-body pass): the Diff block owns its
 * own layout — dual old/new line-number gutters, a +/-/context sign gutter,
 * collapsible unchanged runs, and a side-by-side split view. A single Prism
 * <pre> can't lay any of that out. So instead we expose a `highlightLine(text)`
 * that returns the highlighted token nodes for ONE diff row's code, which each
 * row drops in place of its previous plain `{row.text}` string. This is exactly
 * how GitHub-style diff viewers highlight (line-oriented), keeps every existing
 * affordance intact, and is trivially memoizable per (language, text).
 *
 * Robustness / graceful fallback: an unknown/empty language, or any refractor
 * throw, yields the plain text back (uncoloured, never a crash). The app is
 * DARK-ONLY.
 */

/** Convert a refractor hast tree into React nodes with `oneDark` token styles. */
function nodesToReact(
  nodes: ReturnType<typeof refractor.highlight>,
  keyPrefix: string,
): ReactNode[] {
  return nodes.map((node, i) =>
    createElement({
      node,
      stylesheet: oneDark,
      useInlineStyles: true,
      key: `${keyPrefix}-${i}`,
    }),
  );
}

/**
 * Build a memoized per-line highlighter for a given language. Returns a function
 * that maps a single line of code to its highlighted React token nodes. When the
 * language is unknown/absent (or highlighting throws), the function returns the
 * raw text so the row renders as plain (uncoloured) monospace — the graceful
 * fallback. Results are cached per line text so re-renders (toggle/collapse/
 * hover) never re-tokenize, and split view reuses the unified view's work.
 */
export function makeLineHighlighter(
  language?: string,
): (text: string) => ReactNode {
  const lang = language?.trim().toLowerCase();
  // No language, plaintext, or a grammar refractor doesn't know → identity.
  if (!lang || lang === "text" || lang === "plaintext" || lang === "diff") {
    return (text: string) => text;
  }
  if (!refractor.registered(lang)) {
    return (text: string) => text;
  }

  const cache = new Map<string, ReactNode>();
  return (text: string): ReactNode => {
    if (text === "") return text;
    const hit = cache.get(text);
    if (hit !== undefined) return hit;
    let rendered: ReactNode;
    try {
      const tree = refractor.highlight(text, lang);
      rendered = nodesToReact(tree, "tok");
    } catch {
      rendered = text;
    }
    cache.set(text, rendered);
    return rendered;
  };
}
