import { Prism as SyntaxHighlighter } from "react-syntax-highlighter";
import { oneDark } from "react-syntax-highlighter/dist/esm/styles/prism";

/**
 * The syntax-highlighting surface, split into its own module so `AnnotatedCode`
 * can lazy-load it. `react-syntax-highlighter` (Prism + every language) is large
 * and only needed when a code block is actually on screen, so keeping it out of
 * the proposals' synchronous import graph keeps that bundle (and the test
 * suite's module load) small.
 */
export default function CodeBlock({
  language,
  content,
}: {
  language: string;
  content: string;
}) {
  return (
    <SyntaxHighlighter
      language={language}
      style={oneDark}
      customStyle={{
        margin: 0,
        background: "transparent",
        fontSize: "0.78rem",
        padding: "1rem",
      }}
      codeTagProps={{ style: { fontFamily: "var(--font-mono, monospace)" } }}
    >
      {content}
    </SyntaxHighlighter>
  );
}
