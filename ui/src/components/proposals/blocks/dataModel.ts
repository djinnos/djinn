// Pure (React-free) helpers for the interactive data-model (ERD) block. These
// parse the freeform body an author writes inside <DataModel>…</DataModel> into a
// structured set of *entities*, each with a list of *fields*, then infer the
// *relations* between them from foreign-key references. Kept in their own module
// so they are unit-testable without pulling React in.
//
// djinn does NOT serialize a structured `fields={…}` JSON prop — the declared
// schema is decorative LLM-guidance only. The actual data-model content is the
// block's *children markdown* (see the Rust `validation.rs` fixture:
// `<DataModel id="user-schema" name="User"> id: uuid </DataModel>`). The LLM
// typically writes one of:
//
//   1. A markdown table — a header row + a separator + one row per field, with
//      PK / FK / nullable / default / enum hints in the cells:
//        | field    | type    | notes                    |
//        | -------- | ------- | ------------------------ |
//        | id       | uuid    | PK                       |
//        | owner_id | uuid    | FK users.id, nullable    |
//        | status   | text    | enum: active | archived   |
//
//   2. A bulleted / indented list of fields:
//        - id: uuid (PK)
//        - owner_id: uuid — FK users.id, nullable
//        - status: text, default = 'active'
//
//   3. Multiple `### EntityName` sections, each followed by a table or list, for
//      a multi-table model in one block.
//
// From the free-text `type` / `notes` we heuristically detect PK (primary key),
// FK `<table>.<col>` (foreign key), `nullable`, `default = X`, and enum value
// lists (`a | b | c`). Relations are inferred from FK fields. Anything the parser
// can't make sense of yields an empty entity list so the caller can fall back to
// the raw markdown render.

/* ── Public types ──────────────────────────────────────────────────────────── */

/** A parsed foreign-key target, e.g. `users.id` → `{ table: "users", column: "id" }`. */
export interface FkTarget {
  /** Referenced entity/table name. */
  table: string;
  /** Referenced column, if the author wrote `table.col`. */
  column?: string;
}

/** A single parsed field (column) of an entity. */
export interface DataModelField {
  name: string;
  /** Declared type (e.g. `uuid`, `text`, `int`), if any. */
  type?: string;
  /** True when this field is (part of) the primary key. */
  pk?: boolean;
  /** Foreign-key target when this field references another table. */
  fk?: FkTarget;
  /** True when the field is explicitly nullable. */
  nullable?: boolean;
  /** Default value, when the author wrote `default = X`. */
  default?: string;
  /** Enum values, when the author wrote `enum: a | b | c`. */
  enumValues?: string[];
  /** Leftover human note after the structured hints were extracted. */
  note?: string;
}

/** One table / model with a name and its fields. */
export interface DataModelEntity {
  name: string;
  fields: DataModelField[];
  /** Optional free-text note attached to the entity (prose before its fields). */
  note?: string;
}

/** Cardinality of an inferred relation. */
export type RelationKind = "1-1" | "1-n" | "n-n";

/** A relation inferred from an FK field: `from` (child) → `to` (parent). */
export interface DataModelRelation {
  /** Entity that holds the foreign key. */
  from: string;
  /** Referenced entity (FK target table). */
  to: string;
  kind: RelationKind;
  /** Label for the connector — the FK field name. */
  label: string;
  /** True when `to` isn't a known entity in this block. */
  unresolved: boolean;
}

/** The full parsed data-model: entities + inferred relations. */
export interface ParsedDataModel {
  entities: DataModelEntity[];
  relations: DataModelRelation[];
}

/* ── Hint extraction (PK / FK / nullable / default / enum) ──────────────────── */

/**
 * Detect a primary-key hint. Matches a standalone `PK`, `primary key`, or `pk`
 * token (word-boundary, case-insensitive) so it doesn't fire on e.g. `spk`.
 */
function detectPk(text: string): boolean {
  return /\b(pk|primary\s*key)\b/i.test(text);
}

/**
 * Detect a foreign-key hint and its target. Accepts `FK users.id`, `FK -> users`,
 * `references users(id)`, `foreign key users.id`, or a bare `→ users.id`. Returns
 * the target table (+ optional column) or undefined.
 */
function detectFk(text: string): FkTarget | undefined {
  // `references table(col)` or `references table.col`
  const refMatch =
    /\breferences?\s+([A-Za-z_][\w]*)\s*(?:\(\s*([\w]+)\s*\)|\.\s*([\w]+))?/i.exec(
      text,
    );
  if (refMatch) {
    return {
      table: refMatch[1]!,
      column: refMatch[2] ?? refMatch[3] ?? undefined,
    };
  }
  // `FK`/`foreign key` followed by an optional arrow then `table.col` / `table`.
  const fkMatch =
    /\b(?:fk|foreign\s*key)\b\s*(?:[-=]*>|→|:)?\s*([A-Za-z_][\w]*)\s*(?:\.\s*([\w]+))?/i.exec(
      text,
    );
  if (fkMatch) {
    return { table: fkMatch[1]!, column: fkMatch[2] ?? undefined };
  }
  return undefined;
}

/** Detect an explicit nullable hint (`nullable`, `null`, `optional`, `?`). */
function detectNullable(text: string): boolean {
  if (/\bnot\s*null\b/i.test(text)) return false;
  return /\b(nullable|optional)\b/i.test(text);
}

/**
 * Extract a `default = X` / `default: X` / `default X` value. Tolerates quoted
 * values and stops at a comma / pipe / semicolon separator. Returns the value
 * and the input with the default clause removed.
 */
function extractDefault(text: string): { value?: string; rest: string } {
  const match =
    /\bdefault\b\s*(?:[:=]\s*)?('[^']*'|"[^"]*"|`[^`]*`|[^,;|]+)/i.exec(text);
  if (!match) return { rest: text };
  let value = (match[1] ?? "").trim();
  // Unwrap surrounding quotes for a cleaner badge.
  const quoted = /^(['"`])([\s\S]*)\1$/.exec(value);
  if (quoted) value = quoted[2] ?? "";
  const rest = (
    text.slice(0, match.index) + text.slice(match.index + match[0].length)
  ).trim();
  return { value: value || undefined, rest };
}

/**
 * Extract enum values from `enum: a | b | c`, `enum(a, b, c)`, or a bare pipe-
 * separated list `a | b | c`. Returns the values and the input with the enum
 * clause removed.
 *
 * NOTE on tables: a `|`-separated enum authored inside a markdown table cell is
 * split by the table parser into adjacent cells, then re-joined with spaces, so
 * the value list can arrive WHITESPACE-separated (`enum: draft published`). The
 * labelled form therefore also splits on whitespace runs.
 */
function extractEnum(text: string): { values?: string[]; rest: string } {
  // `enum: a | b | c`, `enum a, b, c`, `enum(a b c)`. Greedy capture to end so a
  // multi-value list (incl. one re-joined from table cells) is taken in full.
  const labelled = /\benum\b\s*[:(]?\s*(.+?)\s*\)?\s*$/i.exec(text);
  if (labelled) {
    const values = splitEnumValues(labelled[1] ?? "", /\s*[|,]\s*|\s+/);
    if (values.length >= 1) {
      const rest = text.slice(0, labelled.index).trim();
      return { values, rest };
    }
  }
  // A bare pipe-separated list of short tokens (`active | archived | deleted`).
  if (/\|/.test(text) && !/[<>]/.test(text)) {
    const parts = splitEnumValues(text, /\s*[|]\s*/);
    // Require at least two values and that they all look like enum members
    // (short, no spaces of prose) to avoid eating an arbitrary note with a pipe.
    if (parts.length >= 2 && parts.every((p) => /^[\w'"-]+$/.test(p))) {
      return { values: parts, rest: "" };
    }
  }
  return { rest: text };
}

/** Split an enum value list on `separator`, trimming and unwrapping quotes. */
function splitEnumValues(text: string, separator: RegExp): string[] {
  return text
    .split(separator)
    .map((v) => v.trim().replace(/^(['"`])([\s\S]*)\1$/, "$2"))
    .filter((v) => v.length > 0);
}

/**
 * Parse the leftover `type` / `notes` free text of a field into structured hints.
 * `typeHint` is the explicit type column (from a table) when available; otherwise
 * the first bare word of the notes is taken as the type.
 */
function parseFieldHints(
  notes: string,
  typeHint: string | undefined,
): Omit<DataModelField, "name"> {
  let working = notes.trim();

  const pk = detectPk(working);
  const fk = detectFk(working);
  const nullable = detectNullable(working);

  // Type: prefer the explicit table column; else peel the first bare token off
  // the notes when it looks like a type (no spaces, not a known keyword). Doing
  // this BEFORE enum detection lets a bare `text admin | member | guest` keep
  // `text` as the type and `admin | member | guest` as the enum list.
  let type = typeHint?.trim() || undefined;
  if (!type) {
    const firstMatch = /^([A-Za-z][\w<>()[\]]*)\b\s*([\s\S]*)$/.exec(working);
    const first = firstMatch?.[1] ?? "";
    if (
      first &&
      !/^(pk|fk|nullable|optional|null|default|enum|references?|primary|foreign)$/i.test(
        first,
      )
    ) {
      type = first;
      working = (firstMatch?.[2] ?? "").trim();
    }
  }

  const def = extractDefault(working);
  working = def.rest;

  const en = extractEnum(working);
  working = en.rest;

  // Strip the structured keyword tokens we already consumed so the residual
  // `note` is just human prose.
  const note = working
    .replace(/\b(pk|primary\s*key)\b/gi, "")
    .replace(
      /\b(?:fk|foreign\s*key)\b\s*(?:[-=]*>|→|:)?\s*[A-Za-z_][\w]*\s*(?:\.\s*[\w]+)?/gi,
      "",
    )
    .replace(/\breferences?\s+[A-Za-z_][\w]*\s*(?:\([^)]*\)|\.\s*[\w]+)?/gi, "")
    .replace(/\b(nullable|optional|not\s*null)\b/gi, "")
    .replace(/[—–\-:,;]+/g, " ")
    .replace(/\s+/g, " ")
    .trim();

  return {
    type,
    pk: pk || undefined,
    fk,
    nullable: nullable || undefined,
    default: def.value,
    enumValues: en.values,
    note: note || undefined,
  };
}

/* ── Field-line / table-row parsing ─────────────────────────────────────────── */

/** Strip a leading bullet / list marker (`-`, `*`, `+`, `•`) from a row. */
function stripBullet(line: string): string {
  return line.replace(/^\s*[-*+•]\s+/u, "").trim();
}

/**
 * Parse one freeform field line of the form `name: type — notes` or
 * `name type notes`. Returns null when the leading token isn't an identifier.
 */
function parseFieldLine(raw: string): DataModelField | null {
  const bulleted = /^\s*[-*+•]\s+/u.test(raw);
  const line = stripBullet(raw);
  if (!line) return null;

  // `name: rest` (colon-delimited) is the most explicit form.
  let name: string;
  let rest: string;
  let explicit = bulleted;
  const colon = /^([A-Za-z_][\w]*)\s*:\s*([\s\S]*)$/.exec(line);
  if (colon) {
    name = colon[1]!;
    rest = (colon[2] ?? "").trim();
    explicit = true;
  } else {
    const space = /^([A-Za-z_][\w]*)\b\s*([\s\S]*)$/.exec(line);
    if (!space) return null;
    name = space[1]!;
    rest = (space[2] ?? "").trim();
  }
  if (!/^[A-Za-z_][\w]*$/.test(name)) return null;

  const hints = parseFieldHints(rest, undefined);

  // Prose guard: a bare (non-bulleted, non-colon) line is only a field when it
  // carries a structured signal — a type, PK/FK/nullable/default/enum hint, or
  // an empty rest (a lone column name). Otherwise it's a sentence and we leave
  // it as the entity note. A trailing `.` (sentence terminator) also disqualifies.
  if (!explicit) {
    const hasSignal =
      hints.type !== undefined ||
      hints.pk ||
      hints.fk !== undefined ||
      hints.nullable ||
      hints.default !== undefined ||
      hints.enumValues !== undefined ||
      rest === "";
    if (!hasSignal || /[.!?]$/.test(line)) return null;
  }

  return { name, ...hints };
}

/** Split a markdown table row `| a | b | c |` into trimmed cells. */
function splitTableRow(line: string): string[] {
  let row = line.trim();
  if (row.startsWith("|")) row = row.slice(1);
  if (row.endsWith("|")) row = row.slice(0, -1);
  // Split on unescaped pipes.
  return row.split(/\s*(?<!\\)\|\s*/).map((c) => c.replace(/\\\|/g, "|").trim());
}

/** True when a row is a markdown table separator (`| --- | :--: |`). */
function isSeparatorRow(cells: string[]): boolean {
  return (
    cells.length > 0 &&
    cells.every((c) => /^:?-{1,}:?$/.test(c.replace(/\s/g, "")))
  );
}

/** Find the header column index whose name matches any of `names`. */
function findColumn(header: string[], names: string[]): number {
  for (let i = 0; i < header.length; i++) {
    const h = header[i]!.toLowerCase().trim();
    if (names.some((n) => h === n || h.includes(n))) return i;
  }
  return -1;
}

/**
 * Parse a markdown table (header + separator + rows) into fields. Recognises a
 * `field`/`name`/`column` column, a `type` column, and a `notes`/`description`/
 * `constraints` column; remaining columns are folded into the notes text so PK/
 * FK/nullable/default/enum hints in any column are still detected.
 */
function parseTable(lines: string[]): DataModelField[] {
  if (lines.length < 2) return [];
  const header = splitTableRow(lines[0]!);
  const sep = splitTableRow(lines[1]!);
  if (!isSeparatorRow(sep)) return [];

  const nameCol = findColumn(header, ["field", "name", "column", "attribute"]);
  const typeCol = findColumn(header, ["type", "datatype"]);
  // The remaining columns (anything that isn't name/type) become note sources.
  const noteCols = header
    .map((_, i) => i)
    .filter((i) => i !== nameCol && i !== typeCol);

  const fields: DataModelField[] = [];
  for (let i = 2; i < lines.length; i++) {
    const cells = splitTableRow(lines[i]!);
    if (cells.length === 0 || cells.every((c) => c === "")) continue;
    if (isSeparatorRow(cells)) continue;

    const rawName = (nameCol >= 0 ? cells[nameCol] : cells[0]) ?? "";
    const name = rawName.replace(/`/g, "").trim();
    if (!/^[A-Za-z_][\w]*$/.test(name)) continue;

    const type =
      typeCol >= 0 ? (cells[typeCol] ?? "").replace(/`/g, "").trim() : undefined;
    // Note columns PLUS any overflow cells beyond the header width — a `|`-
    // separated enum authored in a cell produces extra cells (`enum: a` `b` `c`)
    // that the table splitter peeled off; folding them back recovers the list.
    const overflow = cells
      .map((_, i) => i)
      .filter((i) => i >= header.length);
    const notes = [...noteCols, ...overflow]
      .map((c) => cells[c] ?? "")
      .filter((c) => c.length > 0)
      .join(" ");

    const hints = parseFieldHints(notes, type || undefined);
    fields.push({ name, ...hints });
  }
  return fields;
}

/* ── Entity sectioning ──────────────────────────────────────────────────────── */

/** A heading like `### Users` or `## table: orders` introduces a new entity. */
function parseHeading(line: string): string | null {
  const md = /^#{1,6}\s+(.+?)\s*$/.exec(line);
  if (md) return cleanEntityName(md[1]!);
  // A bold-only line `**Users**` also reads as a section heading.
  const bold = /^\*\*([^*]+)\*\*\s*:?\s*$/.exec(line.trim());
  if (bold) return cleanEntityName(bold[1]!);
  return null;
}

/** Normalise an entity name: drop a leading `table:`/`entity:` label + backticks. */
function cleanEntityName(raw: string): string {
  return raw
    .replace(/^(table|entity|model)\s*:?\s*/i, "")
    .replace(/`/g, "")
    .trim();
}

/**
 * Parse the fields belonging to one entity section: a run of lines that is
 * EITHER a markdown table OR a bulleted/plain list of field lines. Returns the
 * parsed fields and the leftover prose (non-field) lines as a note.
 */
function parseEntityBody(lines: string[]): {
  fields: DataModelField[];
  note?: string;
} {
  // Collect contiguous table lines (those containing a pipe). A table needs a
  // header + separator, so try the table interpretation first.
  const tableLines = lines.filter((l) => l.includes("|") && l.trim() !== "");
  if (tableLines.length >= 2) {
    const fields = parseTable(tableLines);
    if (fields.length > 0) {
      const prose = lines
        .filter((l) => !l.includes("|") && l.trim() !== "")
        .join(" ")
        .trim();
      return { fields, note: prose || undefined };
    }
  }

  // List / line form: parse each non-empty line as a field; anything that isn't
  // a field becomes part of the entity note (prose).
  const fields: DataModelField[] = [];
  const prose: string[] = [];
  for (const line of lines) {
    if (line.trim() === "") continue;
    const field = parseFieldLine(line);
    if (field) fields.push(field);
    else prose.push(line.trim());
  }
  return { fields, note: prose.length > 0 ? prose.join(" ") : undefined };
}

/* ── Top-level parse ────────────────────────────────────────────────────────── */

/**
 * Parse the freeform <DataModel> body into entities. `fallbackName` is the
 * block's `name` attribute, used as the single entity's name when the body has
 * no `### EntityName` headings.
 *
 * Splits the body into heading-delimited sections; each section's fields are
 * parsed as a table or list. When there are no headings, the whole body is one
 * entity named `fallbackName` (or `"Model"`).
 */
export function parseDataModel(
  body: string,
  fallbackName?: string,
): DataModelEntity[] {
  if (!body || !body.trim()) return [];
  const lines = body.replace(/\r\n?/g, "\n").split("\n");

  // Partition into sections by heading.
  interface Section {
    name?: string;
    lines: string[];
  }
  const sections: Section[] = [];
  let current: Section = { lines: [] };
  let sawHeading = false;

  for (const line of lines) {
    const heading = parseHeading(line);
    if (heading !== null) {
      // Close the running section (if it has content) and open a new one.
      if (current.name !== undefined || current.lines.some((l) => l.trim())) {
        sections.push(current);
      }
      current = { name: heading, lines: [] };
      sawHeading = true;
    } else {
      current.lines.push(line);
    }
  }
  if (current.name !== undefined || current.lines.some((l) => l.trim())) {
    sections.push(current);
  }

  const entities: DataModelEntity[] = [];
  for (const section of sections) {
    const { fields, note } = parseEntityBody(section.lines);
    if (fields.length === 0) continue;
    const name =
      section.name ||
      (!sawHeading ? fallbackName?.trim() || "Model" : "Model");
    entities.push({ name, fields, note });
  }

  return entities;
}

/* ── Relation inference ─────────────────────────────────────────────────────── */

/** Resolve an FK target table name against the known entities (case-insensitive). */
function resolveEntity(
  entities: DataModelEntity[],
  ref: string,
): DataModelEntity | undefined {
  const needle = ref.trim().toLowerCase();
  return (
    entities.find((e) => e.name.toLowerCase() === needle) ??
    // Tolerate singular/plural mismatch (`user` ↔ `users`).
    entities.find(
      (e) =>
        e.name.toLowerCase() === `${needle}s` ||
        `${e.name.toLowerCase()}s` === needle,
    )
  );
}

/**
 * Infer relations from FK fields: every `fk` field yields a `1-n` relation from
 * the entity holding the FK to its target table. Marked `unresolved` when the
 * target isn't a known entity in this block.
 */
export function inferRelations(entities: DataModelEntity[]): DataModelRelation[] {
  const relations: DataModelRelation[] = [];
  const seen = new Set<string>();
  for (const entity of entities) {
    for (const field of entity.fields) {
      if (!field.fk) continue;
      const target = resolveEntity(entities, field.fk.table);
      const toName = target?.name ?? field.fk.table;
      const key = `${entity.name}→${toName}:${field.name}`;
      if (seen.has(key)) continue;
      seen.add(key);
      relations.push({
        from: entity.name,
        to: toName,
        kind: "1-n",
        label: field.name,
        unresolved: !target,
      });
    }
  }
  return relations;
}

/** Parse a data-model body end-to-end: entities + inferred relations. */
export function parseDataModelBlock(
  body: string,
  fallbackName?: string,
): ParsedDataModel {
  const entities = parseDataModel(body, fallbackName);
  return { entities, relations: inferRelations(entities) };
}
