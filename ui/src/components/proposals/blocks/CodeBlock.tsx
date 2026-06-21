import type { CSSProperties, HTMLProps } from "react";
import { Prism as SyntaxHighlighter } from "react-syntax-highlighter";
import { oneDark } from "react-syntax-highlighter/dist/esm/styles/prism";

/**
 * The syntax-highlighting surface, split into its own module so `AnnotatedCode`
 * can lazy-load it. `react-syntax-highlighter` (Prism + every language) is large
 * and only needed when a code block is actually on screen, so keeping it out of
 * the proposals' synchronous import graph keeps that bundle (and the test
 * suite's module load) small.
 *
 * When `showLineNumbers` is set the surface renders a left line-number gutter and
 * wraps each line in its own element, and `lineProps(lineNumber)` can attach
 * per-line props (refs via `onMouseEnter`, highlight classes, handlers) — this is
 * what `AnnotatedCode` uses to paint line-anchored annotation bands + hover
 * cross-highlighting on top of the Prism highlighting. The app is DARK-ONLY.
 */
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
      style={oneDark}
      showLineNumbers={showLineNumbers}
      wrapLines={Boolean(lineProps)}
      lineProps={lineProps}
      lineNumberStyle={
        {
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
