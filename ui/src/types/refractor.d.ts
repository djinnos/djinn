// Minimal ambient typings for the subset of `refractor` the Diff block's
// highlighter uses. `refractor` is already a (transitive) dependency of
// `react-syntax-highlighter` — its `Prism` export is built on it — so we add no
// new package, only the types it ships without. The returned nodes are hast
// element/text nodes, shape-compatible with RSH's `createElement` `rendererNode`.
declare module "refractor" {
  import type { createElementProps } from "react-syntax-highlighter";

  /** Hast nodes refractor emits, shape-compatible with RSH's `createElement`. */
  type RefractorNode = createElementProps["node"];

  interface Refractor {
    /** Tokenize `value` as `language`, returning a hast node tree. Throws on an
     *  unregistered language. */
    highlight(value: string, language: string): RefractorNode[];
    /** Whether a language (or alias) grammar is registered. */
    registered(language: string): boolean;
  }

  const refractor: Refractor;
  export default refractor;
}
