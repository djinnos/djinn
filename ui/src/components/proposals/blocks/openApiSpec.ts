// Pure (React-free) helpers for the interactive openapi-spec block. djinn does
// NOT serialize a structured prop — the block's payload arrives as its *children
// string* (`segment.content`) = a whole OpenAPI 3 / Swagger 2 document. We parse
// that text into a normalized shape the renderer walks: operations grouped by
// tag, each carrying params / request body / per-status responses with `$ref`
// models resolved and example bodies derived from schemas. Kept in its own
// module so the parse / `$ref`-resolve / example-generate logic is unit-testable
// without pulling React in.
//
// The parse is defensive: an empty, non-JSON, or non-OpenAPI payload yields
// `{ ok: false }` with an error message, and the renderer falls back to a styled
// raw render (it never throws and never blanks).
//
// YAML: `js-yaml` is NOT a declared UI dependency (only a transitive one), so v1
// parses JSON only — `parseOpenApiSpec` is the single seam to extend when a YAML
// parser is added. A YAML-looking body produces a clean, actionable fallback
// error rather than a silent blank.

import type { ParamLocation } from "./apiEndpoint";

/* ── Public normalized shape ────────────────────────────────────────────────── */

/** A single normalized parameter on an operation (path/query/header/cookie). */
export interface OpenApiParam {
  name: string;
  /** Raw `in` location; the renderer maps known ones to a colored pill. */
  in: string;
  type?: string;
  required?: boolean;
  description?: string;
}

/** A single normalized response: a status token + description + example body. */
export interface OpenApiResponse {
  /** Raw status token, e.g. `200`, `4XX`, `default`. */
  status: string;
  description?: string;
  /** A derived JSON example body (pretty-printed), when one is available. */
  example?: string;
}

/** A single normalized operation (one method on one path). */
export interface OpenApiOperation {
  /** Stable per-operation key (`get /users`). */
  id: string;
  method: string;
  path: string;
  summary?: string;
  description?: string;
  deprecated?: boolean;
  secured?: boolean;
  params: OpenApiParam[];
  requestContentType?: string;
  requestExample?: string;
  responses: OpenApiResponse[];
}

/** Operations sharing a tag, in document order. */
export interface OpenApiTagGroup {
  tag: string;
  description?: string;
  operations: OpenApiOperation[];
}

/** The fully normalized spec the renderer walks. */
export interface NormalizedOpenApiSpec {
  title?: string;
  version?: string;
  description?: string;
  format: "OpenAPI 3" | "Swagger 2" | "Unknown";
  servers: string[];
  groups: OpenApiTagGroup[];
  operationCount: number;
}

/** The discriminated result of {@link parseOpenApiSpec}. */
export type OpenApiParseResult =
  | { ok: true; spec: NormalizedOpenApiSpec }
  | { ok: false; error: string };

/** Tag used to bucket operations that declare no `tags`. */
export const UNTAGGED_GROUP = "default";

/** Max `$ref` resolution / example-recursion depth (cheap cycle backstop). */
const MAX_EXAMPLE_DEPTH = 6;
/** Max consecutive `$ref` hops before we give up (defends against ref chains). */
const MAX_REF_HOPS = 20;
/** Cap how many object properties an example object expands (huge-spec guard). */
const MAX_EXAMPLE_PROPS = 40;
/** Cap a stringified example so a pathological schema can't blow up the DOM. */
const MAX_EXAMPLE_CHARS = 8_000;

const HTTP_METHODS = [
  "get",
  "post",
  "put",
  "patch",
  "delete",
  "head",
  "options",
  "trace",
] as const;

/* ── JSON value helpers ─────────────────────────────────────────────────────── */

type Json = unknown;
type JsonObject = Record<string, Json>;

function isObject(value: Json): value is JsonObject {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function asString(value: Json): string | undefined {
  return typeof value === "string" ? value : undefined;
}

/** Param locations the api-endpoint pills color; everything else renders plain. */
const KNOWN_LOCATIONS: ReadonlySet<string> = new Set<ParamLocation>([
  "path",
  "query",
  "header",
  "body",
]);

/** True when a location string maps to a colored param-location pill. */
export function isKnownParamLocation(location: string): location is ParamLocation {
  return KNOWN_LOCATIONS.has(location);
}

/* ── Top-level parse ────────────────────────────────────────────────────────── */

/**
 * Parse the raw spec children into a normalized spec. v1 supports JSON only —
 * `js-yaml` is not a declared UI dependency. A YAML-looking body yields an
 * actionable error so the renderer can fall back gracefully (never throws).
 * This is the single seam to extend with YAML once a parser is a real dep.
 */
export function parseOpenApiSpec(raw: string): OpenApiParseResult {
  const trimmed = (raw ?? "").trim();
  if (!trimmed) {
    return {
      ok: false,
      error: "Empty spec — paste an OpenAPI 3 / Swagger 2 document.",
    };
  }

  let doc: Json;
  try {
    doc = JSON.parse(trimmed);
  } catch (error) {
    // A `key: value` / `---` lead-in is almost certainly YAML, not malformed
    // JSON — surface a targeted hint so the fallback is actionable.
    const looksLikeYaml =
      trimmed.startsWith("---") || /^[A-Za-z][\w-]*\s*:/m.test(trimmed);
    const hint = looksLikeYaml
      ? " (YAML is not supported yet — paste JSON, or convert the spec to JSON.)"
      : "";
    return {
      ok: false,
      error: `${error instanceof Error ? error.message : "Invalid JSON"}${hint}`,
    };
  }

  if (!isObject(doc)) {
    return { ok: false, error: "Spec must be a JSON object." };
  }
  // Reject documents with no OpenAPI/Swagger marker AND no paths — those are
  // plausibly arbitrary JSON the author meant for a json-explorer block.
  const hasMarker =
    typeof doc.openapi === "string" || typeof doc.swagger === "string";
  if (!hasMarker && !isObject(doc.paths)) {
    return {
      ok: false,
      error:
        "Not an OpenAPI document — expected an `openapi`/`swagger` version and a `paths` object.",
    };
  }

  try {
    return { ok: true, spec: normalizeSpec(doc) };
  } catch (error) {
    return {
      ok: false,
      error:
        error instanceof Error
          ? error.message
          : "Could not interpret the spec document.",
    };
  }
}

/* ── $ref resolution (cycle-guarded) ────────────────────────────────────────── */

/**
 * Resolve a local JSON-pointer `$ref` (e.g. `#/components/schemas/User`) against
 * the document root. Returns `undefined` for external refs or a ref already on
 * the current resolution path (`seen`), which is the cycle guard. Decodes the
 * `~1`/`~0` JSON-pointer escapes.
 */
export function resolveRef(
  root: JsonObject,
  ref: string,
  seen: Set<string>,
): Json | undefined {
  if (!ref.startsWith("#/")) return undefined; // external refs unsupported
  if (seen.has(ref)) return undefined; // cycle guard
  seen.add(ref);
  const segments = ref
    .slice(2)
    .split("/")
    .map((seg) => seg.replace(/~1/g, "/").replace(/~0/g, "~"));
  let current: Json = root;
  for (const segment of segments) {
    if (!isObject(current)) return undefined;
    current = current[segment];
  }
  return current;
}

/**
 * Follow `$ref` hops on a schema/param object until a concrete (non-`$ref`)
 * value is reached, the chain dead-ends, or a cycle / hop budget is hit. The
 * `seen` set carries the resolution path so a self-referential model terminates.
 */
function deref(root: JsonObject, value: Json, seen: Set<string>): Json {
  let current = value;
  let hops = 0;
  while (isObject(current) && typeof current.$ref === "string" && hops < MAX_REF_HOPS) {
    const resolved = resolveRef(root, current.$ref, seen);
    if (resolved === undefined) return current; // unresolved → keep the ref node
    current = resolved;
    hops += 1;
  }
  return current;
}

/* ── Schema → type label + example generation ───────────────────────────────── */

/** Short human type label for a (possibly `$ref`'d) schema. */
function schemaTypeLabel(
  root: JsonObject,
  schema: Json,
  seen: Set<string>,
): string | undefined {
  const resolved = deref(root, schema, new Set(seen));
  if (!isObject(resolved)) return undefined;
  if (typeof resolved.type === "string") {
    if (resolved.type === "array" && resolved.items) {
      const inner = schemaTypeLabel(root, resolved.items, seen);
      return inner ? `${inner}[]` : "array";
    }
    return resolved.type;
  }
  if (typeof resolved.$ref === "string") return resolved.$ref.split("/").pop();
  if (Array.isArray(resolved.enum)) return "enum";
  if (resolved.properties) return "object";
  if (resolved.oneOf || resolved.anyOf || resolved.allOf) return "object";
  return undefined;
}

/**
 * Build a representative example value from a JSON Schema. Honors `example` /
 * `default` / `enum[0]` when present, else emits a type-based placeholder.
 * Bounded by `MAX_EXAMPLE_DEPTH` and a `$ref`-cycle guard (each branch clones
 * `seen`), so a recursive model terminates with a `…` sentinel rather than
 * spinning.
 */
export function schemaExample(
  root: JsonObject,
  schema: Json,
  seen: Set<string>,
  depth: number,
): Json {
  if (depth > MAX_EXAMPLE_DEPTH) return "…";
  const resolved = deref(root, schema, new Set(seen));
  if (!isObject(resolved)) return resolved ?? null;

  if (resolved.example !== undefined) return resolved.example;
  if (resolved.default !== undefined) return resolved.default;
  if (Array.isArray(resolved.enum) && resolved.enum.length > 0) {
    return resolved.enum[0];
  }

  // Compose `allOf` members into a single object.
  if (Array.isArray(resolved.allOf)) {
    const merged: JsonObject = {};
    for (const part of resolved.allOf) {
      const value = schemaExample(root, part, seen, depth + 1);
      if (isObject(value)) Object.assign(merged, value);
    }
    if (Object.keys(merged).length > 0) return merged;
  }
  // `oneOf` / `anyOf`: pick the first branch as the representative example.
  const variant =
    (Array.isArray(resolved.oneOf) && resolved.oneOf) ||
    (Array.isArray(resolved.anyOf) && resolved.anyOf) ||
    null;
  if (variant && variant.length > 0) {
    return schemaExample(root, variant[0], seen, depth + 1);
  }

  const type = resolved.type;
  if (type === "object" || resolved.properties) {
    const props = isObject(resolved.properties) ? resolved.properties : {};
    const out: JsonObject = {};
    for (const [key, propSchema] of Object.entries(props).slice(
      0,
      MAX_EXAMPLE_PROPS,
    )) {
      out[key] = schemaExample(root, propSchema, seen, depth + 1);
    }
    return out;
  }
  if (type === "array") {
    return [schemaExample(root, resolved.items ?? {}, seen, depth + 1)];
  }
  if (type === "integer" || type === "number") return 0;
  if (type === "boolean") return true;
  if (type === "string") {
    if (resolved.format === "date-time") return "2020-01-01T00:00:00Z";
    if (resolved.format === "date") return "2020-01-01";
    if (resolved.format === "uuid") {
      return "00000000-0000-0000-0000-000000000000";
    }
    if (resolved.format === "email") return "user@example.com";
    return "string";
  }
  return null;
}

/** Stringify a derived example, dropping empty values and bounding the size. */
function stringifyExample(value: Json): string | undefined {
  if (value === undefined) return undefined;
  try {
    const text = JSON.stringify(value, null, 2);
    if (!text || text === "null" || text === "{}" || text === "[]") {
      return undefined;
    }
    return text.length > MAX_EXAMPLE_CHARS
      ? `${text.slice(0, MAX_EXAMPLE_CHARS)}\n…`
      : text;
  } catch {
    return undefined;
  }
}

/* ── Param / operation / response normalization ─────────────────────────────── */

function normalizeParam(
  root: JsonObject,
  raw: Json,
  seen: Set<string>,
): OpenApiParam | undefined {
  const param = deref(root, raw, new Set(seen));
  if (!isObject(param)) return undefined;
  const name = asString(param.name);
  const location = asString(param.in);
  if (!name || !location) return undefined;
  // OpenAPI 3 nests the type under `schema`; Swagger 2 puts it on the param.
  const type = schemaTypeLabel(root, param.schema, seen) ?? asString(param.type);
  return {
    name,
    in: location,
    type,
    required: param.required === true,
    description: asString(param.description),
  };
}

function normalizeResponses(
  root: JsonObject,
  rawResponses: Json,
): OpenApiResponse[] {
  if (!isObject(rawResponses)) return [];
  const responses: OpenApiResponse[] = [];
  for (const [status, rawResponse] of Object.entries(rawResponses)) {
    const response = deref(root, rawResponse, new Set());
    let example: string | undefined;
    if (isObject(response)) {
      // OpenAPI 3: response.content[*].schema; Swagger 2: response.schema.
      if (isObject(response.content)) {
        const media = Object.values(response.content)[0];
        if (isObject(media)) {
          example =
            stringifyExample(media.example) ??
            stringifyExample(schemaExample(root, media.schema, new Set(), 0));
        }
      } else if (response.schema !== undefined) {
        example = stringifyExample(
          schemaExample(root, response.schema, new Set(), 0),
        );
      }
    }
    responses.push({
      status,
      description: isObject(response) ? asString(response.description) : undefined,
      example,
    });
  }
  return responses.sort((a, b) => a.status.localeCompare(b.status));
}

function normalizeOperation(
  root: JsonObject,
  path: string,
  method: string,
  rawOp: Json,
  pathLevelParams: OpenApiParam[],
  globalSecured: boolean,
): OpenApiOperation | undefined {
  if (!isObject(rawOp)) return undefined;

  const params: OpenApiParam[] = [...pathLevelParams];
  if (Array.isArray(rawOp.parameters)) {
    for (const rawParam of rawOp.parameters) {
      const normalized = normalizeParam(root, rawParam, new Set());
      if (normalized) params.push(normalized);
    }
  }

  // Request body: OpenAPI 3 `requestBody.content[*]`; Swagger 2 `in: body` param.
  let requestContentType: string | undefined;
  let requestExample: string | undefined;
  const requestBody = deref(root, rawOp.requestBody, new Set());
  if (isObject(requestBody) && isObject(requestBody.content)) {
    const [contentType, media] = Object.entries(requestBody.content)[0] ?? [];
    requestContentType = contentType;
    if (isObject(media)) {
      requestExample =
        stringifyExample(media.example) ??
        stringifyExample(schemaExample(root, media.schema, new Set(), 0));
    }
  } else {
    // Swagger 2 carries the body schema on an `in: body` parameter.
    const bodyParamRaw = Array.isArray(rawOp.parameters)
      ? rawOp.parameters
          .map((p) => deref(root, p, new Set()))
          .find((p) => isObject(p) && asString((p as JsonObject).in) === "body")
      : undefined;
    if (isObject(bodyParamRaw)) {
      requestContentType = "application/json";
      requestExample = stringifyExample(
        schemaExample(root, bodyParamRaw.schema, new Set(), 0),
      );
    }
  }

  // Swagger 2 `in: body` params are surfaced via the request body above; drop
  // them from the visible param table so they don't double-render.
  const visibleParams = params.filter((p) => p.in !== "body");

  const responses = normalizeResponses(root, rawOp.responses);

  const operationSecured =
    Array.isArray(rawOp.security) && rawOp.security.length === 0
      ? false // explicit opt-out
      : Array.isArray(rawOp.security)
        ? rawOp.security.some(
            (req) => isObject(req) && Object.keys(req).length > 0,
          )
        : globalSecured || undefined;

  return {
    id: `${method} ${path}`,
    method: method.toUpperCase(),
    path,
    summary: asString(rawOp.summary),
    description: asString(rawOp.description),
    deprecated: rawOp.deprecated === true,
    secured: operationSecured,
    params: visibleParams,
    requestContentType,
    requestExample,
    responses,
  };
}

/** The tags an operation belongs to, defaulting to the untagged group. */
function operationTags(rawOp: JsonObject): string[] {
  if (Array.isArray(rawOp.tags)) {
    const tags = rawOp.tags.filter((t): t is string => typeof t === "string");
    if (tags.length > 0) return tags;
  }
  return [UNTAGGED_GROUP];
}

function normalizeSpec(doc: JsonObject): NormalizedOpenApiSpec {
  const format: NormalizedOpenApiSpec["format"] =
    typeof doc.openapi === "string"
      ? "OpenAPI 3"
      : typeof doc.swagger === "string"
        ? "Swagger 2"
        : "Unknown";

  const info = isObject(doc.info) ? doc.info : undefined;

  // Servers: OpenAPI 3 `servers[].url`; Swagger 2 `host` + `basePath`.
  const servers: string[] = [];
  if (Array.isArray(doc.servers)) {
    for (const server of doc.servers) {
      if (isObject(server) && typeof server.url === "string") {
        servers.push(server.url);
      }
    }
  } else if (typeof doc.host === "string") {
    const basePath = typeof doc.basePath === "string" ? doc.basePath : "";
    servers.push(`${doc.host}${basePath}`);
  }

  // A non-empty global `security` requirement marks every operation secured
  // unless the operation overrides it.
  const globalSecured =
    Array.isArray(doc.security) &&
    doc.security.some((req) => isObject(req) && Object.keys(req).length > 0);

  // Document-level tag order + descriptions.
  const tagOrder: string[] = [];
  const tagDescriptions = new Map<string, string>();
  if (Array.isArray(doc.tags)) {
    for (const tag of doc.tags) {
      if (isObject(tag) && typeof tag.name === "string") {
        tagOrder.push(tag.name);
        if (typeof tag.description === "string") {
          tagDescriptions.set(tag.name, tag.description);
        }
      }
    }
  }

  const groups = new Map<string, OpenApiOperation[]>();
  let operationCount = 0;

  const paths = isObject(doc.paths) ? doc.paths : {};
  for (const [path, rawPathItem] of Object.entries(paths)) {
    const pathItem = deref(doc, rawPathItem, new Set());
    if (!isObject(pathItem)) continue;

    const pathLevelParams: OpenApiParam[] = [];
    if (Array.isArray(pathItem.parameters)) {
      for (const rawParam of pathItem.parameters) {
        const normalized = normalizeParam(doc, rawParam, new Set());
        if (normalized) pathLevelParams.push(normalized);
      }
    }

    for (const method of HTTP_METHODS) {
      const rawOp = pathItem[method];
      if (!isObject(rawOp)) continue;
      const operation = normalizeOperation(
        doc,
        path,
        method,
        rawOp,
        pathLevelParams,
        globalSecured,
      );
      if (!operation) continue;
      operationCount += 1;
      for (const tag of operationTags(rawOp)) {
        const list = groups.get(tag) ?? [];
        list.push(operation);
        groups.set(tag, list);
      }
    }
  }

  // Order: documented tags first (in declared order), then any remaining tags
  // alphabetically. The untagged `default` group always sinks to the end.
  const remaining = [...groups.keys()]
    .filter((tag) => !tagOrder.includes(tag))
    .sort((a, b) => {
      if (a === UNTAGGED_GROUP) return 1;
      if (b === UNTAGGED_GROUP) return -1;
      return a.localeCompare(b);
    });
  const orderedTagNames = [
    ...tagOrder.filter((tag) => groups.has(tag) && tag !== UNTAGGED_GROUP),
    ...remaining,
  ];

  const groupList: OpenApiTagGroup[] = orderedTagNames.map((tag) => ({
    tag,
    description: tagDescriptions.get(tag),
    operations: groups.get(tag) ?? [],
  }));

  return {
    title: info ? asString(info.title) : undefined,
    version: info ? asString(info.version) : undefined,
    description: info ? asString(info.description) : undefined,
    format,
    servers,
    groups: groupList,
    operationCount,
  };
}
