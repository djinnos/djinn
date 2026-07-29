import { getServerBaseUrl } from "@/api/serverUrl";

/** JSON values persisted by the readiness service (composition, evidence, and outputs). */
export type ReadinessJson =
  | null
  | boolean
  | number
  | string
  | ReadonlyArray<ReadinessJson>
  | { readonly [key: string]: ReadinessJson };

/** Stable summary returned by GET /api/projects/:projectId/readiness. */
export interface ReadinessRunSummary {
  id: string;
  project_id: string;
  status: string;
  repository_snapshot: string;
  skill_name: string;
  skill_version: string;
  expected_area_count: number | null;
  created_at: string;
  completed_at: string | null;
}

export interface ReadinessAttempt {
  id: string;
  attempt_number: number;
  status: string;
  payload_digest: string | null;
  created_at: string;
  terminal_at: string | null;
  is_current: boolean;
}

export interface ReadinessFinding {
  id: string;
  attempt_id: string;
  guardrail_key: string;
  status: string;
  severity: string;
  confidence: number;
  evidence: ReadinessJson;
  created_at: string;
}

export interface ReadinessOutput {
  attempt_id: string;
  result: ReadinessJson;
  created_at: string;
}

export interface ReadinessArea {
  id: string;
  area_key: string;
  composition: ReadinessJson;
  path_scopes: ReadinessJson;
  frozen_at: string;
  status: string;
  attempts: ReadinessAttempt[];
  accepted_findings: ReadinessFinding[];
  accepted_outputs: ReadinessOutput[];
}

export interface ReadinessAreaScore {
  area_id: string;
  score: number;
  applicable_weight: number;
  covered_weight: number;
  status: string;
  created_at: string;
}

export interface ReadinessProjectScore {
  score: number;
  band: string;
  created_at: string;
}

export interface ReadinessSuggestion {
  id: string;
  dedupe_key: string;
  suggestion: ReadinessJson;
  created_at: string;
}

export interface ReadinessEvent {
  id: string;
  event_kind: string;
  payload: ReadinessJson;
  created_at: string;
}

/** Stable complete projection returned by GET .../readiness/:runId. */
export interface ReadinessRunDetail {
  run: ReadinessRunSummary;
  areas: ReadinessArea[];
  area_scores: ReadinessAreaScore[];
  project_score: ReadinessProjectScore | null;
  suggestions: ReadinessSuggestion[];
  events: ReadinessEvent[];
}

export interface ReadinessKickoffResponse {
  run: ReadinessRunSummary;
  identification_task_id: string;
  created: boolean;
  reused: boolean;
}

export type ReadinessHttpFailureKind =
  | "unauthorized"
  | "missing"
  | "conflict"
  | "server"
  | "client";

/**
 * A non-2xx readiness response. The route's error code remains available so
 * callers can distinguish a missing project/run, an active-run conflict, and
 * a server failure without string matching an Error message.
 */
export class ReadinessHttpError extends Error {
  readonly status: number;
  readonly code: string | null;
  readonly kind: ReadinessHttpFailureKind;

  constructor(status: number, code: string | null, statusText = "") {
    super(`Readiness request failed: ${status}${code ? ` ${code}` : statusText ? ` ${statusText}` : ""}`);
    this.name = "ReadinessHttpError";
    this.status = status;
    this.code = code;
    this.kind = readinessHttpFailureKind(status);
  }
}

function readinessHttpFailureKind(status: number): ReadinessHttpFailureKind {
  if (status === 401 || status === 403) return "unauthorized";
  if (status === 404) return "missing";
  if (status === 409) return "conflict";
  if (status >= 500) return "server";
  return "client";
}

async function errorCode(response: Response): Promise<string | null> {
  try {
    const body: unknown = await response.json();
    if (typeof body === "object" && body !== null && !Array.isArray(body)) {
      const code = (body as { code?: unknown }).code;
      return typeof code === "string" ? code : null;
    }
  } catch {
    // Some proxies return a non-JSON error page. Preserve the HTTP status.
  }
  return null;
}

async function requireOk(response: Response): Promise<Response> {
  if (response.ok) return response;
  throw new ReadinessHttpError(response.status, await errorCode(response), response.statusText);
}

function readinessUrl(projectId: string): string {
  return `${getServerBaseUrl()}/api/projects/${encodeURIComponent(projectId)}/readiness`;
}

const READINESS_HEADERS = { Accept: "application/json" };

/** Reads the active run, falling back to the latest terminal run, without starting work. */
export async function fetchActiveOrLatestReadiness(
  projectId: string,
  options: { signal?: AbortSignal } = {},
): Promise<ReadinessRunSummary | null> {
  const response = await requireOk(await fetch(readinessUrl(projectId), {
    credentials: "include",
    headers: READINESS_HEADERS,
    signal: options.signal,
  }));
  return response.json() as Promise<ReadinessRunSummary | null>;
}

/** Reads one existing readiness run scoped to its project, without starting work. */
export async function fetchReadinessRunDetail(
  projectId: string,
  runId: string,
  options: { signal?: AbortSignal } = {},
): Promise<ReadinessRunDetail> {
  const response = await requireOk(await fetch(
    `${readinessUrl(projectId)}/${encodeURIComponent(runId)}`,
    { credentials: "include", headers: READINESS_HEADERS, signal: options.signal },
  ));
  return response.json() as Promise<ReadinessRunDetail>;
}

export interface KickoffReadinessOptions {
  /** Supply the key again when retrying the same explicit user action. */
  idempotencyKey?: string;
  signal?: AbortSignal;
}

/** Generate a browser-owned idempotency key for exactly one explicit kickoff action. */
export function createReadinessIdempotencyKey(): string {
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
    return crypto.randomUUID();
  }
  const bytes = new Uint8Array(16);
  if (typeof crypto !== "undefined" && typeof crypto.getRandomValues === "function") {
    crypto.getRandomValues(bytes);
    return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
  }
  // This fallback only supports older browser test environments; supported
  // browsers use cryptographically strong Web Crypto values above.
  return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
}

/**
 * Starts readiness only when invoked explicitly. Pass an existing idempotency
 * key to retry the same action; reads and polling never call this function.
 */
export async function kickoffReadiness(
  projectId: string,
  options: KickoffReadinessOptions = {},
): Promise<ReadinessKickoffResponse> {
  const idempotencyKey = options.idempotencyKey ?? createReadinessIdempotencyKey();
  const response = await requireOk(await fetch(`${readinessUrl(projectId)}/kickoff`, {
    method: "POST",
    credentials: "include",
    headers: { ...READINESS_HEADERS, "Content-Type": "application/json" },
    body: JSON.stringify({ idempotency_key: idempotencyKey }),
    signal: options.signal,
  }));
  return response.json() as Promise<ReadinessKickoffResponse>;
}

/**
 * Captures one key for a user action. Calling `kickoff` again (for example
 * after a transient network failure) always sends that same key.
 */
export interface ReadinessKickoffAction {
  readonly idempotencyKey: string;
  kickoff(projectId: string, options?: { signal?: AbortSignal }): Promise<ReadinessKickoffResponse>;
}

export function createReadinessKickoffAction(idempotencyKey = createReadinessIdempotencyKey()): ReadinessKickoffAction {
  return {
    idempotencyKey,
    kickoff: (projectId, options = {}) => kickoffReadiness(projectId, { ...options, idempotencyKey }),
  };
}
