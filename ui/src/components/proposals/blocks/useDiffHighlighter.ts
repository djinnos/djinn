import { useEffect, useMemo, useState, type ReactNode } from "react";

import type { makeLineHighlighter } from "./diffHighlight";

/**
 * Lazy, memoized per-line syntax highlighter for the Diff block.
 *
 * `react-syntax-highlighter` + `refractor` (every Prism grammar) is a large
 * dependency, exactly as for `AnnotatedCode`/`CodeBlock` — so the highlighting
 * surface (`./diffHighlight`) is loaded via a dynamic `import()` only when a diff
 * with a real language is actually mounted, keeping it out of the proposals'
 * synchronous bundle and the test suite's module graph.
 *
 * While the chunk is loading (and during SSR / when no usable language is
 * resolved) the returned function is the identity — the row renders as plain
 * monospace text, which IS the graceful fallback. Once loaded, the function
 * tokenizes a single diff line to `oneDark`-coloured React nodes, memoized per
 * line text inside `makeLineHighlighter`, so toggling unified/split, expanding a
 * collapsed run, or hovering never re-tokenizes.
 */
const PLAIN = (text: string): ReactNode => text;

export function useDiffHighlighter(
  language?: string,
): (text: string) => ReactNode {
  // The loaded factory (held in state so a render that follows the load picks
  // it up); `null` until the dynamic import lands.
  const [factory, setFactory] = useState<typeof makeLineHighlighter | null>(
    null,
  );

  // Only load Prism when there's actually a language worth highlighting.
  const enabled = useMemo(() => {
    const lang = language?.trim().toLowerCase();
    return !!lang && lang !== "text" && lang !== "plaintext" && lang !== "diff";
  }, [language]);

  useEffect(() => {
    if (!enabled || factory) return;
    let cancelled = false;
    void import("./diffHighlight").then((mod) => {
      if (cancelled) return;
      // Wrap in a function-updater (the factory is itself a function, which
      // `setState` would otherwise call).
      setFactory(() => mod.makeLineHighlighter);
    });
    return () => {
      cancelled = true;
    };
  }, [enabled, factory]);

  // Rebuild the per-line highlighter (and its line cache) only when the language
  // changes or the module first becomes available.
  return useMemo(() => {
    if (!enabled || !factory) return PLAIN;
    return factory(language);
  }, [enabled, factory, language]);
}
