// Pure (React-free) helpers for the ADR-style "Decisions" block. djinn stores a
// proposal's architecture decisions as the block's *children markdown* — the LLM
// hand-writes them, NOT as a structured `items={…}` prop (the schema's `items[]`
// field exists only to guide generation; see the Rust registry in
// `proposal_blocks.rs`). So this parser folds that freeform markdown into a flat
// list of decision records, each an ADR card (title + DECLARED status + optional
// Context/Decision/Consequences sections). Kept in its own module so it is
// unit-testable without pulling React in.
//
// AUTHORING FORMAT (ADR-style):
//
//   ### Use JWT for stateless auth
//   Status: accepted
//
//   Context
//   We need auth that survives horizontal scaling without sticky sessions.
//
//   Decision
//   Adopt short-lived JWTs validated at the edge.
//
//   Consequences
//   No central session store; revocation needs a denylist.
//
// Rules this parser enforces:
//   • A `## Title` / `### Title` heading starts each decision card. A status may
//     also be DECLARED inline on the heading as a `[accepted]` marker.
//   • Status is read ONLY from an explicit declared token — a `Status: <token>`
//     line directly under the heading, or an inline `[<token>]` heading marker.
//     The token must be one of the ADR vocabulary words (`proposed`, `accepted`,
//     `rejected`, `deprecated`, `superseded`). It is NEVER inferred from English
//     prose in the body. No declared token ⇒ `status` is `undefined` (no badge).
//   • A `superseded` decision may name what supersedes it: `Status: superseded by
//     #3` (or `… by ADR-3`, `… by the event-bus decision`). The trailing text is
//     surfaced as `supersededBy`.
//   • Optional ADR sub-sections `Context`, `Decision`, `Consequences` (as their
//     own sub-heading line or a `Context:` label) are split out; anything else is
//     the freeform `body`.
//
// Empty / heading-less input yields `[]` so the caller can fall back to the raw
// markdown render. Never throws.

/* ── Public types ──────────────────────────────────────────────────────────── */

/** The ADR status vocabulary. Status is DECLARED, never inferred from prose. */
export type DecisionStatus =
  | "proposed"
  | "accepted"
  | "rejected"
  | "deprecated"
  | "superseded";

/** The ordered, canonical ADR status tokens (for validation + docs). */
export const DECISION_STATUSES: readonly DecisionStatus[] = [
  "proposed",
  "accepted",
  "rejected",
  "deprecated",
  "superseded",
];

/** The optional ADR sub-sections parsed out of a decision body. */
export interface DecisionSections {
  /** "Context" — the forces / problem that motivated the decision. */
  context?: string;
  /** "Decision" — what was decided. */
  decision?: string;
  /** "Consequences" — the resulting trade-offs. */
  consequences?: string;
}

/** One parsed ADR decision card. */
export interface ParsedDecision {
  /** The decision title (heading text, status marker stripped). */
  title: string;
  /** The DECLARED status, or `undefined` when none was declared. */
  status?: DecisionStatus;
  /**
   * For a `superseded` decision, the trailing text naming what supersedes it
   * (e.g. `#3`, `ADR-7`, `the event-bus decision`). Undefined otherwise.
   */
  supersededBy?: string;
  /** Parsed ADR sub-sections, when the body uses Context/Decision/Consequences. */
  sections?: DecisionSections;
  /**
   * The freeform body markdown when no ADR sub-sections were found (or the
   * residual prose before the first sub-section). Undefined when empty.
   */
  body?: string;
}

/* ── Status-token detection (DECLARED only) ─────────────────────────────────── */

/** Lowercase lookup of the valid status tokens. */
const STATUS_SET = new Set<string>(DECISION_STATUSES);

/** Coerce an arbitrary token to a `DecisionStatus`, or `undefined` if invalid. */
function asStatus(token: string | undefined): DecisionStatus | undefined {
  const t = token?.trim().toLowerCase();
  return t && STATUS_SET.has(t) ? (t as DecisionStatus) : undefined;
}

/**
 * Parse a declared status value such as `accepted`, `superseded by #3`, or
 * `[rejected]`. Returns the matched status plus any trailing "by …" text. Returns
 * `undefined` status when the leading word isn't a valid ADR token (do NOT guess).
 */
function parseStatusValue(raw: string): {
  status?: DecisionStatus;
  supersededBy?: string;
} {
  const cleaned = raw.trim().replace(/[.;,]+$/, "");
  // Leading token = the status word; the remainder is the optional "by …" tail.
  const m = /^([A-Za-z-]+)\b\s*(.*)$/.exec(cleaned);
  if (!m) return {};
  const status = asStatus(m[1]);
  if (!status) return {};
  let tail = (m[2] ?? "").trim();
  if (status === "superseded" && tail) {
    // Drop a leading "by" connector: `superseded by #3` → `#3`.
    tail = tail.replace(/^by\s+/i, "").trim();
    return { status, supersededBy: tail || undefined };
  }
  return { status };
}

/* ── Heading + status-line detection ────────────────────────────────────────── */

/** A markdown heading line `## …` / `### …` (we accept any depth as a card start). */
const HEADING = /^\s*(#{1,6})\s+(.+?)\s*$/;

/**
 * A `Status: <token>` declaration line. Tolerates `Status -`, `**Status:**`, and
 * a bracketed value. Captures the raw value for {@link parseStatusValue}.
 */
const STATUS_LINE =
  /^\s*\*{0,2}status\*{0,2}\s*[:=-]\s*\*{0,2}\s*\[?\s*([^\]\n]+?)\s*\]?\s*\*{0,2}\s*$/i;

/**
 * An inline status marker on a heading line: a trailing `[accepted]` /
 * `(accepted)`. Returns the parsed status + the heading with the marker removed.
 * Only fires for a valid ADR token so a real bracketed title word is left alone.
 */
function extractHeadingStatus(title: string): {
  title: string;
  status?: DecisionStatus;
  supersededBy?: string;
} {
  const m = /^(.*?)\s*[[(]\s*([^\])]+?)\s*[\])]\s*$/.exec(title.trim());
  if (m) {
    const parsed = parseStatusValue(m[2] ?? "");
    if (parsed.status) {
      return { title: (m[1] ?? "").trim(), ...parsed };
    }
  }
  return { title: title.trim() };
}

/* ── ADR sub-section detection ──────────────────────────────────────────────── */

/** The recognised ADR sub-section labels, keyed to the `DecisionSections` field. */
const SECTION_KEYS: Record<string, keyof DecisionSections> = {
  context: "context",
  decision: "decision",
  consequences: "consequences",
};

/**
 * Detect an ADR sub-section header line: a `Context` / `Decision` / `Consequences`
 * label, optionally as a markdown heading (`#### Context`), bolded (`**Context**`),
 * or colon-suffixed (`Context:`). Returns the section key, or `null`.
 */
function asSectionHeader(line: string): keyof DecisionSections | null {
  let t = line.trim();
  // Strip a leading markdown heading marker.
  t = t.replace(/^#{1,6}\s+/, "");
  // Unwrap surrounding bold.
  t = t.replace(/^\*\*(.+?)\*\*$/, "$1");
  // Drop a trailing colon.
  t = t.replace(/:$/, "").trim().toLowerCase();
  return SECTION_KEYS[t] ?? null;
}

/* ── Per-decision body parsing ──────────────────────────────────────────────── */

/**
 * Split a decision's body lines into ADR sub-sections + leftover prose. The
 * status line (already consumed by the caller) is not present here.
 */
function parseBody(lines: string[]): {
  sections?: DecisionSections;
  body?: string;
} {
  const sections: DecisionSections = {};
  const lead: string[] = [];
  let current: keyof DecisionSections | null = null;
  const buffers: Partial<Record<keyof DecisionSections, string[]>> = {};
  let sawSection = false;

  for (const raw of lines) {
    const header = asSectionHeader(raw);
    if (header) {
      current = header;
      sawSection = true;
      buffers[header] ??= [];
      continue;
    }
    if (current) {
      buffers[current]!.push(raw);
    } else {
      lead.push(raw);
    }
  }

  if (sawSection) {
    for (const key of Object.keys(buffers) as (keyof DecisionSections)[]) {
      const text = (buffers[key] ?? []).join("\n").trim();
      if (text) sections[key] = text;
    }
  }

  const body = lead.join("\n").trim() || undefined;
  const hasSections = Object.keys(sections).length > 0;
  return {
    sections: hasSections ? sections : undefined,
    body,
  };
}

/* ── Top-level parse ────────────────────────────────────────────────────────── */

/** A heading-delimited raw section: its title line + the body lines beneath it. */
interface RawCard {
  title: string;
  lines: string[];
}

/**
 * Parse the freeform <Decisions> body into ADR decision cards. Returns `[]` for
 * empty/whitespace input, OR when the body has no `##`/`###` headings (so the
 * caller falls back to the raw markdown render rather than inventing a card).
 * Never throws.
 */
export function parseDecisions(body: string): ParsedDecision[] {
  if (!body || !body.trim()) return [];
  const lines = body.replace(/\r\n?/g, "\n").split("\n");

  // Partition into heading-delimited cards. Prose before the first heading is
  // discarded (it can't belong to a titled decision) — the heading IS the card.
  const cards: RawCard[] = [];
  let current: RawCard | null = null;
  for (const line of lines) {
    const heading = HEADING.exec(line);
    // A heading starts a NEW card UNLESS it is an ADR sub-section label written
    // as a markdown heading (`#### Context`) inside the decision currently being
    // built — those belong to the open card's body, not a new decision.
    if (heading && !(current && asSectionHeader(line))) {
      current = { title: (heading[2] ?? "").trim(), lines: [] };
      cards.push(current);
    } else if (current) {
      current.lines.push(line);
    }
  }

  // No headings at all → let the caller fall back to plain markdown.
  if (cards.length === 0) return [];

  const decisions: ParsedDecision[] = [];
  for (const card of cards) {
    const headingResult = extractHeadingStatus(card.title);
    const title = headingResult.title;
    if (!title) continue;

    let status = headingResult.status;
    let supersededBy = headingResult.supersededBy;

    // Look for a `Status:` line: scan from the top, skipping blank lines, until
    // the first non-blank line. Status is ONLY honoured when it's that first
    // content line directly under the heading (declared, not buried in prose).
    const bodyLines = [...card.lines];
    let firstContentIdx = -1;
    for (let i = 0; i < bodyLines.length; i++) {
      if (bodyLines[i]!.trim() === "") continue;
      firstContentIdx = i;
      break;
    }
    if (firstContentIdx >= 0) {
      const statusMatch = STATUS_LINE.exec(bodyLines[firstContentIdx]!);
      if (statusMatch) {
        const parsed = parseStatusValue(statusMatch[1] ?? "");
        // An explicit `Status:` line always wins over an inline heading marker.
        if (parsed.status) {
          status = parsed.status;
          supersededBy = parsed.supersededBy;
        }
        // Consume the status line regardless of validity so a declared-but-
        // misspelled token never leaks into the rendered body.
        bodyLines.splice(firstContentIdx, 1);
      }
    }

    const { sections, body: cardBody } = parseBody(bodyLines);

    decisions.push({
      title,
      ...(status ? { status } : {}),
      ...(supersededBy ? { supersededBy } : {}),
      ...(sections ? { sections } : {}),
      ...(cardBody ? { body: cardBody } : {}),
    });
  }

  return decisions;
}
