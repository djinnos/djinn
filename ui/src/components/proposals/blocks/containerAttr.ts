// Shared parsing for the JSON-attribute envelope used by the recursive
// container blocks (`Tabs`, `Columns`). The MDX-AST parser stores a `{...}`
// expression attribute as its raw inner expression text (see `parseMdxBody.ts`
// / Rust `attributes_of`), which for the decided wire format is a JSON array
// literal — parseable with `JSON.parse`. These helpers parse that text and
// normalize each entry's `body` into a proposal-MDX string, returning `null`
// (never throwing) so the caller can render a graceful fallback.

/** One tab: a button `label` and a recursively-rendered proposal-MDX `body`. */
export interface TabEntry {
  label: string;
  body: string;
}

/** One column: a recursively-rendered proposal-MDX `body`. */
export interface ColumnEntry {
  body: string;
}

/** Coerce an unknown value to a body string (non-string bodies become ""). */
function asBody(value: unknown): string {
  return typeof value === "string" ? value : "";
}

/**
 * Parse a `{...}` container attribute value into an array of entries. Returns
 * `null` when the attribute is missing, empty, not valid JSON, or not a JSON
 * array — the signal for the block to render its graceful fallback. Never
 * throws.
 */
function parseEntries(raw: string | undefined): unknown[] | null {
  if (raw === undefined) return null;
  const trimmed = raw.trim();
  if (trimmed.length === 0) return null;
  let parsed: unknown;
  try {
    parsed = JSON.parse(trimmed);
  } catch {
    return null;
  }
  if (!Array.isArray(parsed)) return null;
  return parsed;
}

/**
 * Parse the `tabs` attribute (`tabs={[{ "label", "body" }, ...]}`) into
 * normalized {@link TabEntry} items. Entries missing a usable `label` fall back
 * to a positional `Tab N` label so a partly-malformed array still renders.
 * Returns `null` for a missing / invalid / empty attribute.
 */
export function parseTabsAttr(raw: string | undefined): TabEntry[] | null {
  const entries = parseEntries(raw);
  if (!entries || entries.length === 0) return null;
  return entries.map((entry, index) => {
    const record =
      entry && typeof entry === "object" && !Array.isArray(entry)
        ? (entry as Record<string, unknown>)
        : {};
    const label =
      typeof record.label === "string" && record.label.trim().length > 0
        ? record.label
        : `Tab ${index + 1}`;
    return { label, body: asBody(record.body) };
  });
}

/**
 * Parse the `columns` attribute (`columns={[{ "body" }, ...]}`) into normalized
 * {@link ColumnEntry} items. Returns `null` for a missing / invalid / empty
 * attribute.
 */
export function parseColumnsAttr(raw: string | undefined): ColumnEntry[] | null {
  const entries = parseEntries(raw);
  if (!entries || entries.length === 0) return null;
  return entries.map((entry) => {
    const record =
      entry && typeof entry === "object" && !Array.isArray(entry)
        ? (entry as Record<string, unknown>)
        : {};
    return { body: asBody(record.body) };
  });
}
