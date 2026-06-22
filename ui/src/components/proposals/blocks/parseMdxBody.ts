// Splits an MDX proposal body into alternating markdown / custom-block segments
// using a real MDX abstract syntax tree (mdast) instead of a regex.
//
// Why an AST instead of the previous `BLOCK_REGEX`:
//   * the non-greedy `([\s\S]*?)<\/\1>` matched the FIRST closing tag, so a
//     nested same-named tag inside a block's children truncated the block;
//   * the `[^>]*?` attribute group stopped at the first `>`, so a `>` inside an
//     attribute value truncated the opening tag;
//   * the quoted-only attribute regex could not read `{...}` JSX-expression
//     attributes (needed for forward-compat container blocks like Tabs).
// The AST walker fixes all three while preserving the exact public surface and
// producing byte-identical `content` slices (see below).
//
// We deliberately compose the MDX *JSX* micromark/mdast extensions only — NOT
// the flow/text expression extension. Proposal block children routinely contain
// bare `{ ... }` (raw JSON in JsonExplorer blocks, braces in code).
// With expression parsing enabled those would be parsed as MDX `{expression}`
// nodes and throw "Could not parse expression with acorn". Block children are
// opaque markdown that each block component re-parses itself, so we want the
// JSX element/attribute grammar (including `{...}` *attribute* values) but leave
// `{...}` inside content as literal text — exactly as the old regex did.

import { fromMarkdown } from "mdast-util-from-markdown";
import { mdxJsxFromMarkdown } from "mdast-util-mdx-jsx";
import { mdxJsx } from "micromark-extension-mdx-jsx";
import { mdxMd } from "micromark-extension-mdx-md";
import { combineExtensions } from "micromark-util-combine-extensions";
import { Parser } from "acorn";
import acornJsx from "acorn-jsx";
import type { Nodes, Root } from "mdast";
import type {
  MdxJsxFlowElement,
  MdxJsxTextElement,
  MdxJsxAttribute,
  MdxJsxAttributeValueExpression,
} from "mdast-util-mdx-jsx";

/** Either flavor of MDX JSX element — block (`mdxJsxFlowElement`) or inline
 * (`mdxJsxTextElement`). Adjacent JSX elements not separated by a blank line
 * are parsed as the inline flavor inside a paragraph, so both must be matched
 * for parity with the previous regex (which was blank-line agnostic). */
type MdxJsxElement = MdxJsxFlowElement | MdxJsxTextElement;

// ──────────────────────────────────────────────────────────────────────────────
// Types
// ──────────────────────────────────────────────────────────────────────────────

interface BlockSegment {
  kind: "block";
  tag: string;
  attributes: Record<string, string>;
  content: string;
  /** The `id` attribute value, or an auto-generated fallback. */
  id: string;
  /** Index of the block within the body (for stable fallback ids). */
  index: number;
}

interface MarkdownSegment {
  kind: "markdown";
  text: string;
}

type BodySegment = BlockSegment | MarkdownSegment;

// ──────────────────────────────────────────────────────────────────────────────
// MDX AST parsing
// ──────────────────────────────────────────────────────────────────────────────

// Acorn (with JSX) is required by the MDX JSX extension to parse `{...}`
// attribute-value expressions. Created once and reused.
const acorn = Parser.extend(acornJsx());
const acornOptions = { ecmaVersion: 2024 as const, sourceType: "module" as const };

const MDX_JSX_MICROMARK = combineExtensions([
  mdxJsx({ acorn, acornOptions, addResult: true }),
  mdxMd(),
]);

function parseMdxAst(body: string): Root {
  return fromMarkdown(body, {
    extensions: [MDX_JSX_MICROMARK],
    mdastExtensions: [mdxJsxFromMarkdown()],
  });
}

function isBlockElement(node: Nodes): node is MdxJsxElement {
  return (
    (node.type === "mdxJsxFlowElement" || node.type === "mdxJsxTextElement") &&
    typeof node.name === "string" &&
    isPascalCaseTag(node.name)
  );
}

/**
 * Collect every PascalCase MDX JSX element (block or inline) in document order.
 * Inline elements only appear nested inside a paragraph when two JSX elements
 * are not separated by a blank line; we still treat them as blocks for parity.
 * We never recurse *into* a matched block — its children are opaque markdown
 * sliced from source, exactly as the old non-greedy regex captured them.
 */
function collectBlockElements(tree: Root): MdxJsxElement[] {
  const found: MdxJsxElement[] = [];
  const visit = (node: Nodes) => {
    if (isBlockElement(node)) {
      found.push(node);
      return; // do not descend into block children
    }
    const children = (node as { children?: Nodes[] }).children;
    if (children) for (const child of children) visit(child);
  };
  for (const child of tree.children) visit(child);
  return found;
}

/**
 * Locate the offset just after the opening tag's closing `>` (or `/>`),
 * scanning from the node's start offset while skipping over quoted attribute
 * values and `{...}` expression depth. This is the start of the block's
 * children-markdown content and mirrors exactly where the old regex's content
 * group began — preserving the leading `\n` after `>` that the previous parser
 * captured.
 */
function openTagEndOffset(source: string, startOffset: number): number {
  let i = startOffset + 1; // skip the leading '<'
  let depth = 0;
  let quote: string | null = null;
  while (i < source.length) {
    const ch = source[i];
    if (quote) {
      if (ch === quote) quote = null;
      i++;
      continue;
    }
    if (ch === '"' || ch === "'") {
      quote = ch;
      i++;
      continue;
    }
    if (ch === "{") {
      depth++;
      i++;
      continue;
    }
    if (ch === "}") {
      depth--;
      i++;
      continue;
    }
    if (ch === ">" && depth === 0) return i + 1;
    i++;
  }
  return i;
}

/**
 * Source-slice the children-markdown of a block element. We slice the original
 * `body` between the end of the opening tag and the start of the closing tag
 * rather than re-stringifying the mdast children — re-stringification would
 * churn whitespace and break block sub-parsers that assert on exact `content`
 * substrings. Self-closing elements (no children) yield an empty string.
 */
function blockContent(source: string, node: MdxJsxElement): string {
  if (!node.children || node.children.length === 0) return "";
  const start = node.position?.start.offset;
  const end = node.position?.end.offset;
  if (start === undefined || end === undefined) return "";
  const contentStart = openTagEndOffset(source, start);
  const closeStart = end - `</${node.name}>`.length;
  if (closeStart <= contentStart) return "";
  return source.slice(contentStart, closeStart);
}

// ──────────────────────────────────────────────────────────────────────────────
// Attribute resolution
// ──────────────────────────────────────────────────────────────────────────────

function isMdxJsxAttribute(
  attr: MdxJsxElement["attributes"][number],
): attr is MdxJsxAttribute {
  return attr.type === "mdxJsxAttribute";
}

/**
 * Render an MDX expression attribute value (`attr={...}`) back to the source
 * expression text. The mdast node carries the raw expression in `.value`; we
 * store that verbatim so forward-compat consumers (e.g. Tabs `tabs={[...]}`)
 * can JSON-parse it. Falls back to "" for an empty expression.
 */
function expressionAttributeText(
  value: MdxJsxAttributeValueExpression,
): string {
  return (value.value ?? "").trim();
}

function attributesOf(node: MdxJsxElement): Record<string, string> {
  const attrs: Record<string, string> = {};
  for (const attr of node.attributes) {
    if (!isMdxJsxAttribute(attr)) continue; // skip {...spread} attributes
    const name = attr.name;
    const value = attr.value;
    if (value === null || value === undefined) {
      // Bare / boolean attribute: `<Tag flag>` → "true".
      attrs[name] = "true";
    } else if (typeof value === "string") {
      attrs[name] = value;
    } else {
      // Expression attribute: store the raw expression text (e.g. JSON).
      attrs[name] = expressionAttributeText(value);
    }
  }
  return attrs;
}

// ──────────────────────────────────────────────────────────────────────────────
// Public API
// ──────────────────────────────────────────────────────────────────────────────

/**
 * Returns `true` when `tag` follows the canonical PascalCase component
 * convention (starts with uppercase, followed by alphanumeric characters).
 * Used to distinguish registered block tags from ordinary HTML elements.
 */
export function isPascalCaseTag(tag: string): boolean {
  return /^[A-Z][A-Za-z0-9]*$/.test(tag);
}

/**
 * Extract just the canonical block tag names from an MDX body string.
 * Useful for validation passes that need to check all tags against the
 * registry without allocating full segment objects.
 */
export function extractBlockTags(body: string): string[] {
  const tree = parseMdxAst(body);
  return collectBlockElements(tree).map((node) => node.name as string);
}

/**
 * Split an MDX proposal body into alternating segments of regular markdown
 * and block content. Top-level PascalCase MDX elements (e.g. `<RichText>`,
 * `<Diagram />`) become block segments; every other top-level node (and the
 * raw text between blocks, including blank lines) is coalesced into markdown
 * segments by slicing the original body across its span so bytes are preserved.
 *
 * Lowercase HTML tags and other angle-bracket uses are left inside markdown
 * segments, matching the previous regex parser's segmentation semantics.
 */
export function parseMdxBody(body: string): BodySegment[] {
  const tree = parseMdxAst(body);
  const blocks = collectBlockElements(tree);

  const segments: BodySegment[] = [];
  let lastIndex = 0;
  let blockIndex = 0;

  // Linear scan over block spans — the AST analogue of the previous regex's
  // match loop. Source between blocks (incl. blank lines) is emitted verbatim
  // as markdown when non-blank; the leading/trailing markdown handling matches
  // the old parser exactly.
  for (const node of blocks) {
    const start = node.position?.start.offset;
    const end = node.position?.end.offset;
    if (start === undefined || end === undefined) continue;

    if (start > lastIndex) {
      const mdText = body.slice(lastIndex, start);
      if (mdText.trim().length > 0) {
        segments.push({ kind: "markdown", text: mdText });
      }
    }

    const attributes = attributesOf(node);
    const id = attributes["id"] ?? `block-${blockIndex}`;
    segments.push({
      kind: "block",
      tag: node.name as string,
      attributes,
      content: blockContent(body, node),
      id,
      index: blockIndex,
    });

    blockIndex++;
    lastIndex = end;
  }

  // Trailing markdown after the last block.
  if (lastIndex < body.length) {
    const mdText = body.slice(lastIndex);
    if (mdText.trim().length > 0) {
      segments.push({ kind: "markdown", text: mdText });
    }
  }

  return segments;
}
