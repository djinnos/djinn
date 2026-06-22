// Pure (React-free) helpers for the interactive json-explorer block. djinn does
// NOT serialize a structured JSON prop — the block's payload arrives as its
// *children string* (`segment.content`). So we `JSON.parse` that text into a
// value and the renderer walks it as a collapsible typed tree. Kept in its own
// module so the parsing/summarising logic is unit-testable without React.
//
// The parse is defensive: an empty or invalid payload yields `{ ok: false }`
// with an error message, and the renderer falls back to a styled `<pre>` of the
// raw text (it never throws and never blanks). The default expand depth keeps
// deep payloads scannable while showing the top levels up front.

/** The default depth beyond which container nodes start collapsed. */
export const JSON_EXPLORER_DEFAULT_COLLAPSED_DEPTH = 2;

/** A parsed JSON value — the recursive shape the tree renderer walks. */
export type JsonValue =
  | string
  | number
  | boolean
  | null
  | JsonValue[]
  | { [key: string]: JsonValue };

/** A `{ key: value }` object node (not an array, not a primitive). */
export type JsonObject = { [key: string]: JsonValue };

/** The discriminated result of {@link parseJsonValue}. */
export type JsonParseResult =
  | { ok: true; value: JsonValue }
  | { ok: false; error: string };

/** The coarse value kind, used to pick a type color in the renderer. */
export type JsonValueKind =
  | "string"
  | "number"
  | "boolean"
  | "null"
  | "array"
  | "object";

/**
 * Parse a raw children string into a JSON value. Returns a discriminated result
 * so the caller renders a tree on `ok` and a raw `<pre>` fallback otherwise.
 * Empty / whitespace-only input is treated as a (recoverable) parse failure so
 * the renderer falls back rather than rendering an empty tree.
 */
export function parseJsonValue(raw: string): JsonParseResult {
  const trimmed = (raw ?? "").trim();
  if (!trimmed) {
    return { ok: false, error: "Empty payload — nothing to explore." };
  }
  try {
    return { ok: true, value: JSON.parse(trimmed) as JsonValue };
  } catch (error) {
    return {
      ok: false,
      error: error instanceof Error ? error.message : "Invalid JSON",
    };
  }
}

/** True when the value is a container (array or non-null object). */
export function isContainer(value: JsonValue): value is JsonValue[] | JsonObject {
  return value !== null && typeof value === "object";
}

/** True when the value is a container with at least one child entry. */
export function isNonEmptyContainer(
  value: JsonValue,
): value is JsonValue[] | JsonObject {
  if (!isContainer(value)) return false;
  return Array.isArray(value)
    ? value.length > 0
    : Object.keys(value).length > 0;
}

/** The coarse kind of a value, for type-colored leaf rendering. */
export function valueKind(value: JsonValue): JsonValueKind {
  if (value === null) return "null";
  if (Array.isArray(value)) return "array";
  switch (typeof value) {
    case "string":
      return "string";
    case "number":
      return "number";
    case "boolean":
      return "boolean";
    default:
      return "object";
  }
}

/** The ordered `[key, value]` entries of a container (array indices or keys). */
export function containerEntries(
  value: JsonValue[] | JsonObject,
): Array<[string | number, JsonValue]> {
  return Array.isArray(value)
    ? value.map((item, index) => [index, item] as [number, JsonValue])
    : Object.entries(value);
}

/**
 * A devtools-style one-line summary for a collapsed container, e.g.
 * `5 items` for an array or `3 keys` for an object. Singular/plural aware.
 */
export function containerSummary(value: JsonValue[] | JsonObject): string {
  if (Array.isArray(value)) {
    const count = value.length;
    return `${count} ${count === 1 ? "item" : "items"}`;
  }
  const count = Object.keys(value).length;
  return `${count} ${count === 1 ? "key" : "keys"}`;
}

/** Render a primitive leaf to its JSON text (strings quoted, null lowercased). */
export function formatLeaf(value: string | number | boolean | null): string {
  if (value === null) return "null";
  if (typeof value === "string") return JSON.stringify(value);
  return String(value);
}
