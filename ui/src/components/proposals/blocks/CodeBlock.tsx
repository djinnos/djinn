import type { CSSProperties, HTMLProps, ReactNode } from "react";
import { Prism as SyntaxHighlighter } from "react-syntax-highlighter";
import { oneDark } from "react-syntax-highlighter/dist/esm/styles/prism";
import createElement from "react-syntax-highlighter/dist/esm/create-element";

/**
 * The syntax-highlighting surface, split into its own module so `AnnotatedCode`
 * can lazy-load it. `react-syntax-highlighter` (Prism + every language) is large
 * and only needed when a code block is actually on screen, so keeping it out of
 * the proposals' synchronous import graph keeps that bundle (and the test
 * suite's module load) small.
 *
 * Line numbers + alignment: the library's default `showLineNumbers` renders the
 * gutter as a SEPARATE floated `<code>` block of all numbers, decoupled from the
 * code body. When a code line wraps (or the two blocks' line-heights drift), the
 * numbers desync from their lines and long lines split across visual rows. To
 * guarantee one logical line === exactly one perfectly-aligned row, we instead:
 *   - use `showInlineLineNumbers`, so each line number is a child of its own row
 *     (same row box as the code — they can never drift apart), and
 *   - supply a custom `renderer` that lays each row out as `display: block` with
 *     `white-space: pre` (no wrapping → long lines scroll horizontally) and
 *     merges in the per-line `lineProps` (refs/handlers/highlight classes) that
 *     `AnnotatedCode` uses for its annotation bands + hover cross-highlighting.
 *
 * The app is DARK-ONLY.
 */

type Row = Parameters<typeof createElement>[0]["node"];

/**
 * Render one aligned row per logical source line. Each row is a block-level
 * element (so it owns a full row and can't be merged/split) with `white-space:
 * pre` (long lines scroll horizontally instead of wrapping out of alignment).
 * The inline line number is already the first child of each row.
 */
function makeRenderer(
  lineProps?: (lineNumber: number) => HTMLProps<HTMLElement>,
) {
  return function lineRenderer({
    rows,
    stylesheet,
    useInlineStyles,
  }: {
    rows: Row[];
    stylesheet: Record<string, CSSProperties>;
    useInlineStyles: boolean;
  }): ReactNode {
    return rows.map((node, i) => {
      const lineNumber = i + 1;
      const extra = lineProps ? lineProps(lineNumber) : undefined;

      // Merge our per-line props (className/handlers/refs/style) into the row
      // node's properties without losing Prism's own classes.
      const baseClass = Array.isArray(node.properties?.className)
        ? node.properties.className
        : node.properties?.className
          ? [node.properties.className]
          : [];
      const extraClass = extra?.className ? extra.className.split(/\s+/) : [];

      node.properties = {
        ...node.properties,
        ...extra,
        className: [...baseClass, ...extraClass],
        style: {
          // One block row per line; `pre` keeps long lines on a single row and
          // lets the surrounding container scroll them horizontally.
          display: "block",
          whiteSpace: "pre",
          ...(node.properties?.style as CSSProperties | undefined),
          ...(extra?.style as CSSProperties | undefined),
        },
      };

      return createElement({
        node,
        stylesheet,
        useInlineStyles,
        key: `code-line-${i}`,
      });
    });
  };
}

export default function CodeBlock({
  language,
  content,
  showLineNumbers = false,
  lineProps,
}: {
  language: string;
  content: string;
  showLineNumbers?: boolean;
  lineProps?: (lineNumber: number) => HTMLProps<HTMLElement>;
}) {
  return (
    <SyntaxHighlighter
      language={language}
      // Stable hook for the Prism↔Tailwind token-class collision guard in
      // globals.css. `useInlineStyles` means RSH never adds a `language-*`
      // class to the <pre>/<code>, so this is our only reliable CSS anchor.
      className="djinn-code"
      style={oneDark}
      showLineNumbers={showLineNumbers}
      // Inline line numbers keep each number in its line's own row box.
      showInlineLineNumbers={showLineNumbers}
      // Our renderer emits one aligned, non-wrapping block row per line and
      // applies `lineProps` for AnnotatedCode's annotation bands.
      renderer={showLineNumbers ? makeRenderer(lineProps) : undefined}
      lineNumberStyle={
        {
          display: "inline-block",
          minWidth: "2.5rem",
          paddingRight: "1rem",
          textAlign: "right",
          color: "var(--muted-foreground, #6b7280)",
          userSelect: "none",
        } as CSSProperties
      }
      customStyle={{
        margin: 0,
        background: "transparent",
        fontSize: "0.78rem",
        padding: showLineNumbers ? "1rem 1rem 1rem 0" : "1rem",
      }}
      codeTagProps={{ style: { fontFamily: "var(--font-mono, monospace)" } }}
    >
      {content}
    </SyntaxHighlighter>
  );
}
