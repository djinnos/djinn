/** Unwrap an MDX expression-attribute value that the parser stores verbatim:
 *  `attr={`…`}` / `attr={"…"}` arrive as a template-literal or quoted string
 *  (delimiters included). Strip a single matching pair so the consumer sees
 *  the raw value, not a leading backtick. Shared by the blocks whose primary
 *  content ships in a schema attribute (`Diagram source`, `AnnotatedCode
 *  code`). */
export function unwrapExprValue(s: string): string {
  const t = s.trim();
  if (t.length >= 2) {
    const open = t[0];
    const close = t[t.length - 1];
    if (
      (open === "`" && close === "`") ||
      (open === '"' && close === '"') ||
      (open === "'" && close === "'")
    ) {
      return t.slice(1, -1);
    }
  }
  return s;
}
