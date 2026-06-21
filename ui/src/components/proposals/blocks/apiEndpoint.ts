// Pure (React-free) helpers for the interactive api-endpoint block. These parse
// the freeform body an author writes inside <ApiEndpoint>…</ApiEndpoint> — driven
// by the block's `method` + `path` attributes — into a structured shape: a prose
// description, a list of PARAMETERS (name / in / type / required / description),
// and a list of RESPONSES (status code + description + example body). Kept in
// their own module so they are unit-testable without pulling React in.
//
// djinn does NOT serialize a structured `params={…}` / `responses={…}` JSON prop
// — the declared `request_schema` / `response_schema` fields are decorative
// LLM-guidance only. The actual endpoint detail is the block's *children markdown*
// (see the Rust `validation.rs` fixture:
// `<ApiEndpoint id="get-users" method="GET" path="/api/users">Returns a list of
// all users.</ApiEndpoint>`). So we parse the children. The LLM typically writes
// some mix of:
//
//   1. A prose description (one or more paragraphs).
//   2. A `## Parameters` (or `### Params`) section: a markdown table
//        | name     | in    | type   | required | description     |
//        | -------- | ----- | ------ | -------- | --------------- |
//        | id       | path  | uuid   | yes      | the user id     |
//      OR a bulleted list:
//        - `id` (path, uuid, required) — the user id
//        - `limit` (query, int) — page size
//   3. A `## Responses` (or `### Response`) section: a markdown table
//        | status | description        |
//        | ------ | ------------------ |
//        | 200    | the user object    |
//        | 404    | not found          |
//      OR a bulleted list / sub-headings, optionally with a fenced JSON body for
//      each status.
//   4. Fenced ```json example bodies, which we attach to the nearest response (or
//      surface as a request example).
//
// Anything the parser can't make sense of yields an empty params/responses list
// (the caller falls back to rendering the raw children markdown), so an oddly-
// authored endpoint is never lost and never crashes.

/* ── Public types ──────────────────────────────────────────────────────────── */

/** HTTP verbs we color-code; anything else falls back to a neutral pill. */
export type ApiMethod =
  | "GET"
  | "POST"
  | "PUT"
  | "PATCH"
  | "DELETE"
  | "HEAD"
  | "OPTIONS";

export const API_METHODS: ApiMethod[] = [
  "GET",
  "POST",
  "PUT",
  "PATCH",
  "DELETE",
  "HEAD",
  "OPTIONS",
];

/** Where a parameter lives — drives the IN-location pill color. */
export type ParamLocation = "path" | "query" | "header" | "body";

export const PARAM_LOCATIONS: ParamLocation[] = [
  "path",
  "query",
  "header",
  "body",
];

/** A single parsed request parameter. */
export interface ApiParam {
  name: string;
  in?: ParamLocation;
  type?: string;
  required?: boolean;
  description?: string;
}

/** A single parsed response: a status code + optional description + JSON body. */
export interface ApiResponse {
  /** Raw status token, e.g. `200`, `2xx`, `default`. */
  status: string;
  description?: string;
  /** A fenced JSON (or other) example body attached to this status. */
  example?: string;
}

/** The full parsed endpoint body. */
export interface ParsedApiEndpoint {
  /** Prose description (markdown) preceding any structured sections. */
  description?: string;
  params: ApiParam[];
  responses: ApiResponse[];
  /** A request example body not tied to a specific response. */
  requestExample?: string;
}

/* ── Method + status classification ─────────────────────────────────────────── */

/** Coerce a raw `method` attribute to a known verb (uppercased), or undefined. */
export function normalizeMethod(raw: string | undefined): ApiMethod | undefined {
  if (!raw) return undefined;
  const upper = raw.trim().toUpperCase();
  return API_METHODS.includes(upper as ApiMethod)
    ? (upper as ApiMethod)
    : undefined;
}

/** Detect whether the children mention auth/bearer/token so we can show a lock. */
export function mentionsAuth(body: string): boolean {
  return /\b(auth(?:entication|orization|enticated)?|bearer|oauth|api[\s-]?key|jwt|token|credentials?|x-api-key)\b/i.test(
    body,
  );
}

/** Detect a `Deprecated` marker in the children. */
export function mentionsDeprecated(body: string): boolean {
  return /\bdeprecated\b/i.test(body);
}

/**
 * Classify a status token by its leading digit into a coarse class. `2xx` →
 * `success`, `4xx` → `warn`, `5xx` → `error`, `3xx`/`1xx`/`default`/other →
 * `info`. Tolerates `2xx`, `2XX`, `200`, `default`.
 */
export type StatusClass = "success" | "info" | "warn" | "error";

export function classifyStatus(status: string): StatusClass {
  const lead = status.trim().charAt(0);
  switch (lead) {
    case "2":
      return "success";
    case "4":
      return "warn";
    case "5":
      return "error";
    default:
      return "info";
  }
}

/* ── Parameter location classification ──────────────────────────────────────── */

const LOCATION_WORDS: Record<string, ParamLocation> = {
  path: "path",
  url: "path",
  route: "path",
  query: "query",
  querystring: "query",
  qs: "query",
  search: "query",
  header: "header",
  headers: "header",
  body: "body",
  payload: "body",
  formdata: "body",
  form: "body",
};

/** Coerce a free-text location word to a known IN-location, or undefined. */
function normalizeLocation(raw: string | undefined): ParamLocation | undefined {
  if (!raw) return undefined;
  const key = raw.trim().toLowerCase().replace(/[\s_-]+/g, "");
  return LOCATION_WORDS[key];
}

/** Detect a `required` / `optional` hint (returns true / false / undefined). */
function detectRequired(text: string): boolean | undefined {
  if (/\b(optional|not\s*required|nullable)\b/i.test(text)) return false;
  if (/\b(required|mandatory)\b/i.test(text)) return true;
  if (/^\s*(yes|y|true|✓|x)\s*$/i.test(text)) return true;
  if (/^\s*(no|n|false|-|–|—)\s*$/i.test(text)) return false;
  return undefined;
}

/* ── Markdown table parsing ─────────────────────────────────────────────────── */

/** Split a markdown table row `| a | b | c |` into trimmed cells. */
function splitTableRow(line: string): string[] {
  let row = line.trim();
  if (row.startsWith("|")) row = row.slice(1);
  if (row.endsWith("|")) row = row.slice(0, -1);
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

/** Collect contiguous markdown-table lines (pipe-bearing) from a line array. */
function collectTableLines(lines: string[]): string[] {
  return lines.filter((l) => l.includes("|") && l.trim() !== "");
}

/* ── Parameter parsing ──────────────────────────────────────────────────────── */

/**
 * Parse a parameters section's lines into params. Tries a markdown table first
 * (header + separator + rows), then falls back to a bulleted / plain list.
 */
function parseParams(lines: string[]): ApiParam[] {
  const tableLines = collectTableLines(lines);
  if (tableLines.length >= 2) {
    const fromTable = parseParamTable(tableLines);
    if (fromTable.length > 0) return fromTable;
  }

  const params: ApiParam[] = [];
  for (const line of lines) {
    const param = parseParamLine(line);
    if (param) params.push(param);
  }
  return params;
}

/** Parse a `| name | in | type | required | description |` table into params. */
function parseParamTable(lines: string[]): ApiParam[] {
  if (lines.length < 2) return [];
  const header = splitTableRow(lines[0]!);
  const sep = splitTableRow(lines[1]!);
  if (!isSeparatorRow(sep)) return [];

  const nameCol = findColumn(header, ["name", "field", "param", "parameter"]);
  const inCol = findColumn(header, ["in", "location", "where"]);
  const typeCol = findColumn(header, ["type", "datatype", "schema"]);
  const reqCol = findColumn(header, ["required", "req"]);
  const descCol = findColumn(header, [
    "description",
    "desc",
    "notes",
    "note",
    "details",
  ]);

  // A params table must at least have a recognizable name column.
  if (nameCol < 0) return [];

  const params: ApiParam[] = [];
  for (let i = 2; i < lines.length; i++) {
    const cells = splitTableRow(lines[i]!);
    if (cells.length === 0 || cells.every((c) => c === "")) continue;
    if (isSeparatorRow(cells)) continue;

    const name = cleanInline((cells[nameCol] ?? "").trim());
    if (!name) continue;

    const type =
      typeCol >= 0 ? cleanInline((cells[typeCol] ?? "").trim()) : undefined;
    const location = inCol >= 0 ? normalizeLocation(cells[inCol]) : undefined;
    const required =
      reqCol >= 0 ? detectRequired(cells[reqCol] ?? "") : undefined;
    const description =
      descCol >= 0 ? cleanInline((cells[descCol] ?? "").trim()) : undefined;

    params.push({
      name,
      in: location,
      type: type || undefined,
      required,
      description: description || undefined,
    });
  }
  return params;
}

/**
 * Parse one freeform parameter line of the forms:
 *   - `name` (path, uuid, required) — description
 *   - name: type (query) — description
 *   - name [header] string, optional — description
 * Returns null when the leading token isn't an identifier.
 */
function parseParamLine(raw: string): ApiParam | null {
  const line = raw.replace(/^\s*[-*+•]\s+/u, "").trim();
  if (!line) return null;

  // Leading name token: an identifier, optionally backticked, possibly with a
  // `:type` immediately after.
  const nameMatch =
    /^`?([A-Za-z_$][\w$.[\]-]*)`?\s*(?::\s*`?([A-Za-z_$][\w$<>[\]. ]*?)`?)?\s*([\s\S]*)$/.exec(
      line,
    );
  if (!nameMatch) return null;
  const name = (nameMatch[1] ?? "").trim();
  if (!name) return null;

  let type = (nameMatch[2] ?? "").trim() || undefined;
  let rest = (nameMatch[3] ?? "").trim();

  // A bracketed/parenthesized qualifier block: `(path, uuid, required)` or
  // `[query string]`. Pull the first one and mine it for in/type/required.
  let location: ParamLocation | undefined;
  let required: boolean | undefined;
  const qualMatch = /^[([{]([^)\]}]*)[)\]}]\s*([\s\S]*)$/.exec(rest);
  if (qualMatch) {
    const inner = qualMatch[1] ?? "";
    rest = (qualMatch[2] ?? "").trim();
    for (const token of inner.split(/[,;]/)) {
      const t = token.trim();
      if (!t) continue;
      const loc = normalizeLocation(t);
      if (loc && !location) {
        location = loc;
        continue;
      }
      const req = detectRequired(t);
      if (req !== undefined) {
        required = req;
        continue;
      }
      if (!type) type = t;
    }
  }

  // Whatever IN / required hint remains in the trailing prose.
  if (!location) location = normalizeLocation(matchLocationWord(rest));
  if (required === undefined) {
    const req = detectRequired(rest);
    if (req !== undefined) required = req;
  }

  // The description is the trailing prose after stripping a leading separator.
  const description = cleanInline(rest.replace(/^[—–\-:|]+\s*/u, "").trim());

  // A bare line with no type, no qualifier, no separator, and a trailing
  // sentence-ish blob is likely prose, not a param — but if it had a `:type`,
  // a qualifier, or an IN/required hint we keep it.
  const hasSignal =
    type !== undefined ||
    location !== undefined ||
    required !== undefined ||
    qualMatch !== null;
  if (!hasSignal && !description) return null;

  return {
    name,
    in: location,
    type,
    required,
    description: description || undefined,
  };
}

/** Find a standalone location word (`path`/`query`/`header`/`body`) in text. */
function matchLocationWord(text: string): string | undefined {
  const m =
    /\b(path|query|querystring|header|headers|body|payload|route|url)\b/i.exec(
      text,
    );
  return m?.[1];
}

/** Strip markdown inline code/emphasis markers for a clean cell value. */
function cleanInline(text: string): string {
  return text
    .replace(/`([^`]*)`/g, "$1")
    .replace(/\*\*([^*]*)\*\*/g, "$1")
    .replace(/(?<!\*)\*(?!\*)([^*]*)\*/g, "$1")
    .trim();
}

/* ── Response parsing ───────────────────────────────────────────────────────── */

const STATUS_TOKEN = /\b([1-5][0-9x]{2}|default)\b/i;

/**
 * Parse a responses section's lines into responses. Tries a markdown table
 * first (a `status` column), then a bulleted / sub-heading list. Fenced code
 * blocks following a status are attached to it as an example.
 */
function parseResponses(lines: string[]): ApiResponse[] {
  const { prose, codeBlocks } = splitFencedCode(lines);

  const tableLines = collectTableLines(prose);
  if (tableLines.length >= 2) {
    const fromTable = parseResponseTable(tableLines);
    if (fromTable.length > 0) {
      attachExamples(fromTable, codeBlocks);
      return fromTable;
    }
  }

  const responses = parseResponseList(prose);
  attachExamples(responses, codeBlocks);
  return responses;
}

/** Parse a `| status | description |` table into responses. */
function parseResponseTable(lines: string[]): ApiResponse[] {
  if (lines.length < 2) return [];
  const header = splitTableRow(lines[0]!);
  const sep = splitTableRow(lines[1]!);
  if (!isSeparatorRow(sep)) return [];

  const statusCol = findColumn(header, ["status", "code", "http"]);
  const descCol = findColumn(header, [
    "description",
    "desc",
    "meaning",
    "notes",
    "note",
    "body",
  ]);
  if (statusCol < 0) return [];

  const responses: ApiResponse[] = [];
  for (let i = 2; i < lines.length; i++) {
    const cells = splitTableRow(lines[i]!);
    if (cells.length === 0 || cells.every((c) => c === "")) continue;
    if (isSeparatorRow(cells)) continue;

    const rawStatus = cleanInline((cells[statusCol] ?? "").trim());
    const statusMatch = STATUS_TOKEN.exec(rawStatus);
    const status = statusMatch ? statusMatch[1]! : rawStatus;
    if (!status) continue;

    const description =
      descCol >= 0 ? cleanInline((cells[descCol] ?? "").trim()) : undefined;
    responses.push({ status, description: description || undefined });
  }
  return responses;
}

/** Parse a bulleted / sub-heading list of `status — description` lines. */
function parseResponseList(lines: string[]): ApiResponse[] {
  const responses: ApiResponse[] = [];
  for (const raw of lines) {
    const line = raw
      .replace(/^\s*[-*+•]\s+/u, "")
      .replace(/^#{1,6}\s+/u, "")
      .trim();
    if (!line) continue;

    // Status must appear at (or very near) the start of the line.
    const lead = line.slice(0, 32);
    const match = STATUS_TOKEN.exec(lead);
    if (!match || match.index > 3) continue;
    const status = match[1]!;
    const after = line.slice(match.index + status.length);
    const description = cleanInline(after.replace(/^[—–\-:|)\s]+/u, "").trim());
    responses.push({ status, description: description || undefined });
  }
  return responses;
}

/* ── Fenced-code extraction ─────────────────────────────────────────────────── */

/** A fenced code block lifted out of the body, with the line it followed. */
interface FencedBlock {
  /** Index into the prose-line array AFTER which this block appeared. */
  afterProseIndex: number;
  lang: string;
  code: string;
}

/**
 * Strip ```fenced``` code blocks out of `lines`, returning the prose lines and
 * the extracted blocks (each tagged with the prose-line index it followed so we
 * can attach it to the right response).
 */
function splitFencedCode(lines: string[]): {
  prose: string[];
  codeBlocks: FencedBlock[];
} {
  const prose: string[] = [];
  const codeBlocks: FencedBlock[] = [];
  let inFence = false;
  let fenceLang = "";
  let buffer: string[] = [];

  for (const line of lines) {
    const fence = /^\s*```+\s*([\w-]*)\s*$/.exec(line);
    if (fence) {
      if (!inFence) {
        inFence = true;
        fenceLang = (fence[1] ?? "").toLowerCase();
        buffer = [];
      } else {
        inFence = false;
        codeBlocks.push({
          afterProseIndex: prose.length - 1,
          lang: fenceLang,
          code: buffer.join("\n"),
        });
        buffer = [];
      }
      continue;
    }
    if (inFence) {
      buffer.push(line);
    } else {
      prose.push(line);
    }
  }
  // An unterminated fence: keep what we captured as a block so content isn't lost.
  if (inFence && buffer.length > 0) {
    codeBlocks.push({
      afterProseIndex: prose.length - 1,
      lang: fenceLang,
      code: buffer.join("\n"),
    });
  }

  return { prose, codeBlocks };
}

/**
 * Attach each fenced code block to the response it most plausibly belongs to.
 * The common case (one example per status, authored right under it) lands
 * correctly; extra blocks append to the last response.
 */
function attachExamples(
  responses: ApiResponse[],
  codeBlocks: FencedBlock[],
): void {
  if (responses.length === 0 || codeBlocks.length === 0) return;
  for (let i = 0; i < codeBlocks.length; i++) {
    const target = responses[Math.min(i, responses.length - 1)]!;
    const code = codeBlocks[i]!.code.trim();
    if (!code) continue;
    target.example = target.example ? `${target.example}\n\n${code}` : code;
  }
}

/* ── Section partitioning ───────────────────────────────────────────────────── */

type SectionKind = "description" | "params" | "responses" | "request";

/** Classify a heading line into a known section, or null if it's not one. */
function classifyHeading(line: string): SectionKind | null {
  const m =
    /^\s*#{1,6}\s+(.+?)\s*$/.exec(line) ??
    /^\s*\*\*([^*]+)\*\*\s*:?\s*$/.exec(line.trim());
  if (!m) return null;
  const text = (m[1] ?? "").toLowerCase();
  if (/\b(param(eter)?s?|arguments?|inputs?|query|path\s*param)/.test(text)) {
    return "params";
  }
  if (/\b(responses?|returns?|status\s*codes?|output)/.test(text)) {
    return "responses";
  }
  if (/\b(request|body|payload)\b/.test(text)) return "request";
  return null;
}

/* ── Top-level parse ────────────────────────────────────────────────────────── */

/**
 * Parse the freeform <ApiEndpoint> children into a structured shape: a prose
 * description, a list of parameters, and a list of responses. Returns empty
 * arrays for params/responses when nothing structured is found, so the renderer
 * can fall back to rendering the raw markdown.
 */
export function parseApiEndpointBody(body: string): ParsedApiEndpoint {
  if (!body || !body.trim()) {
    return { description: undefined, params: [], responses: [] };
  }
  const lines = body.replace(/\r\n?/g, "\n").split("\n");

  // Partition into sections by heading. Everything before the first recognized
  // section heading is the description; we track fenced state so a heading-like
  // line inside a code fence doesn't split a section.
  const descLines: string[] = [];
  const paramLines: string[] = [];
  const responseLines: string[] = [];
  const requestLines: string[] = [];

  let current: SectionKind = "description";
  let inFence = false;

  for (const line of lines) {
    if (/^\s*```+/.test(line)) inFence = !inFence;

    if (!inFence) {
      const section = classifyHeading(line);
      if (section) {
        current = section;
        continue; // drop the heading line itself
      }
    }

    switch (current) {
      case "params":
        paramLines.push(line);
        break;
      case "responses":
        responseLines.push(line);
        break;
      case "request":
        requestLines.push(line);
        break;
      default:
        descLines.push(line);
    }
  }

  const params = parseParams(paramLines);
  const responses = parseResponses(responseLines);

  // A request section's fenced JSON becomes the request example.
  let requestExample: string | undefined;
  if (requestLines.length > 0) {
    const { codeBlocks } = splitFencedCode(requestLines);
    const first = codeBlocks.find((b) => b.code.trim());
    if (first) requestExample = first.code.trim();
  }

  const description = descLines.join("\n").trim();

  return {
    description: description.length > 0 ? description : undefined,
    params,
    responses,
    requestExample,
  };
}

/** True when the parsed body has any structured content worth a rich render. */
export function hasStructuredContent(parsed: ParsedApiEndpoint): boolean {
  return (
    parsed.params.length > 0 ||
    parsed.responses.length > 0 ||
    parsed.requestExample !== undefined
  );
}
