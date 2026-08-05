/**
 * Derives the ledger view model from a live session timeline.
 *
 * The timeline is flat and message-shaped; the ledger is structural. Two rules
 * carry most of the work:
 *
 *   - A PHASE is a contiguous run of sessions with the same agent type. When
 *     the agent changes, that is a HANDOFF. When it does not change but the
 *     session id does, the phase gained another ATTEMPT — this is how a
 *     reviewer that died five times and respawned reads as one review phase
 *     with six attempts rather than as nothing at all.
 *   - Reasoning and tool calls collapse into one chronological ACTIVITY strand
 *     per phase, flushed whenever prose or a submission interrupts them, so
 *     order is preserved without a chip per thought.
 */

import type {
  ChatMessage,
  CommandBlock,
  CommentBlock,
  ContentBlock,
  SessionInfo,
  TimelineEntry,
} from "@/hooks/useSessionMessages";
import type {
  AcceptanceCriterion as LedgerCriterion,
  ActivityStep,
  ArtifactVariant,
  LedgerAgentStatus,
  LedgerEntry,
  LedgerLiveState,
  LedgerPhase,
  LedgerTurn,
  TurnBlock,
} from "./ledger";

// ── Tool classification ─────────────────────────────────────────────────────

/** Submissions get their own card; everything else belongs in the strand. */
const FINAL_TOOLS: Record<string, ArtifactVariant> = {
  submit_work: "work_submitted",
  submit_review: "review_submitted",
  submit_decision: "lead_decision",
  submit_grooming: "grooming",
  request_lead: "escalated",
  request_architect: "escalated",
};

const RUNNING_STATUSES = new Set(["running", "active", "starting"]);
const FAILED_STATUSES = new Set(["failed", "errored", "crashed", "timeout"]);

const PHASE_TITLES: Record<string, string> = {
  worker: "Implementation",
  reviewer: "Review",
  epic_reviewer: "Epic review",
  lead: "Lead",
  pm: "Lead",
  planner: "Planning",
  architect: "Architecture",
  advocate: "Advocacy",
  adversary: "Challenge",
  judge: "Judgement",
};

function phaseTitle(agentType: string): string {
  return PHASE_TITLES[agentType] ?? agentType.replace(/_/g, " ");
}

// ── Formatting ──────────────────────────────────────────────────────────────

export function formatClock(iso?: string): string {
  if (!iso) return "";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "";
  return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

export function formatDuration(fromIso?: string, toIso?: string): string | undefined {
  if (!fromIso) return undefined;
  const from = new Date(fromIso).getTime();
  const to = toIso ? new Date(toIso).getTime() : Date.now();
  if (Number.isNaN(from) || Number.isNaN(to) || to < from) return undefined;
  const mins = Math.floor((to - from) / 60_000);
  if (mins < 60) return `${mins}m`;
  return `${Math.floor(mins / 60)}h ${mins % 60}m`;
}

/**
 * A one-line preview of what a tool call actually did. Tools name their
 * subject differently, so probe the conventional keys before falling back to
 * a compact JSON rendering.
 */
export function toolDetail(input?: Record<string, unknown>): string {
  if (!input) return "";
  const keys = ["command", "file_path", "path", "pattern", "query", "url", "id", "task_id"];
  for (const key of keys) {
    const value = input[key];
    if (typeof value === "string" && value.trim()) return value.trim();
  }
  const json = JSON.stringify(input);
  return json.length > 120 ? `${json.slice(0, 117)}…` : json;
}

/** Reasoning text arrives as `thinking`, or as a provider-specific summary array. */
function reasoningText(block: ContentBlock): string | undefined {
  if (typeof block.thinking === "string" && block.thinking.trim()) {
    return block.thinking.trim();
  }
  const summary = block.summary;
  if (Array.isArray(summary)) {
    for (const item of summary) {
      const text = (item as { text?: unknown })?.text;
      if (typeof text === "string" && text.trim()) return text.trim();
    }
  }
  return undefined;
}

/** Strip the leading `**Bold headline**` providers wrap reasoning summaries in. */
function reasoningHeadline(text: string): string {
  const firstLine = text.split("\n").find((l) => l.trim()) ?? text;
  return firstLine.replace(/\*\*/g, "").trim();
}

function isReasoning(block: ContentBlock): boolean {
  return block.type === "thinking" || block.type.includes("reasoning");
}

// ── Phase accumulator ───────────────────────────────────────────────────────

class PhaseBuilder {
  readonly agentType: string;
  readonly sessionIds = new Set<string>();
  private readonly blocks: TurnBlock[] = [];
  private steps: ActivityStep[] = [];

  constructor(agentType: string, sessionId?: string) {
    this.agentType = agentType;
    if (sessionId) this.sessionIds.add(sessionId);
  }

  pushStep(step: ActivityStep) {
    this.steps.push(step);
  }

  /** Activity only stays contiguous while nothing else interrupts it. */
  private flush() {
    if (this.steps.length === 0) return;
    this.blocks.push({ kind: "activity", steps: this.steps });
    this.steps = [];
  }

  pushBlock(block: TurnBlock) {
    this.flush();
    this.blocks.push(block);
  }

  hasContent(): boolean {
    return this.blocks.length > 0 || this.steps.length > 0;
  }

  build(id: string, sessions: SessionInfo[]): LedgerPhase {
    this.flush();

    const own = sessions.filter((s) => this.sessionIds.has(s.id));
    const running = own.some((s) => RUNNING_STATUSES.has(s.status));
    const failed = own.filter((s) => FAILED_STATUSES.has(s.status)).length;
    const first = own[0];
    const last = own[own.length - 1];

    const turns: LedgerTurn[] =
      this.blocks.length > 0
        ? [{ id: `${id}-turn`, agentType: this.agentType, blocks: this.blocks }]
        : [];

    return {
      kind: "phase",
      id,
      title: phaseTitle(this.agentType),
      agentType: this.agentType,
      modelId: first?.modelId,
      durationLabel: formatDuration(first?.startedAt, running ? undefined : last?.endedAt),
      running,
      attempts: own.length > 1 ? { total: own.length, failed } : undefined,
      turns,
    };
  }
}

// ── Timeline entry handling ─────────────────────────────────────────────────

function applyMessage(phase: PhaseBuilder, entry: ChatMessage) {
  for (const block of entry.content) {
    if (isReasoning(block)) {
      const text = reasoningText(block);
      if (text) phase.pushStep({ kind: "thinking", label: reasoningHeadline(text) });
      continue;
    }

    if (block.type === "tool_use") {
      const name = block.name ?? "tool";
      const variant = FINAL_TOOLS[name];
      if (variant) {
        phase.pushBlock(buildArtifact(variant, block, entry.timestamp));
      } else {
        phase.pushStep({ kind: "tool", label: name, detail: toolDetail(block.input) });
      }
      continue;
    }

    if (block.type === "text" && typeof block.text === "string" && block.text.trim()) {
      phase.pushBlock({ kind: "say", markdown: block.text });
    }
  }
}

function asStringArray(value: unknown): string[] | undefined {
  if (typeof value === "string") return value.trim() ? [value.trim()] : undefined;
  if (!Array.isArray(value)) return undefined;
  const out = value.filter((v): v is string => typeof v === "string" && v.trim().length > 0);
  return out.length > 0 ? out : undefined;
}

function buildArtifact(
  variant: ArtifactVariant,
  block: ContentBlock,
  timestamp?: string,
): TurnBlock {
  const input = block.input ?? {};
  const summary =
    (typeof input.summary === "string" && input.summary) ||
    (typeof input.reason === "string" && input.reason) ||
    (typeof input.feedback === "string" && input.feedback) ||
    "";
  const outcome =
    (typeof input.verdict === "string" && input.verdict) ||
    (typeof input.decision === "string" && input.decision) ||
    undefined;

  return {
    kind: "artifact",
    variant,
    summary,
    files: asStringArray(input.files_changed ?? input.files),
    concerns: asStringArray(input.remaining_concerns ?? input.concerns),
    outcome,
    timestamp: formatClock(timestamp),
  };
}

function applyCommand(phase: PhaseBuilder, entry: CommandBlock) {
  phase.pushStep({
    kind: "tool",
    label: entry.name,
    detail: entry.command ?? entry.body.split("\n")[0] ?? "",
    meta: entry.passed ? "pass" : `exit ${entry.exitCode ?? 1}`,
  });
}

function applyComment(phase: PhaseBuilder, entry: CommentBlock) {
  phase.pushBlock({ kind: "say", markdown: entry.body });
}

// ── Entry point ─────────────────────────────────────────────────────────────

export interface BuildLedgerArgs {
  timeline: TimelineEntry[];
  sessions: SessionInfo[];
  description?: string;
  criteria?: { criterion: string; met: boolean }[];
  filedBy?: string;
  filedAt?: string;
}

export interface BuiltLedger {
  entries: LedgerEntry[];
  agents: LedgerAgentStatus[];
  criteria: LedgerCriterion[];
  live: LedgerLiveState | null;
}

export function buildLedger({
  timeline,
  sessions,
  description,
  criteria = [],
  filedBy,
  filedAt,
}: BuildLedgerArgs): BuiltLedger {
  const sessionsById = new Map(sessions.map((s) => [s.id, s]));
  const entries: LedgerEntry[] = [];

  if (description?.trim()) {
    entries.push({
      kind: "phase",
      id: "brief",
      title: "Brief",
      turns: [],
      brief: {
        body: description.trim(),
        filedBy: filedBy ?? "unknown",
        timestamp: formatClock(filedAt),
      },
    });
  }

  let current: PhaseBuilder | null = null;
  let phaseIndex = 0;
  let previousAgent: string | undefined;

  const closeCurrent = () => {
    if (!current) return;
    if (current.hasContent()) {
      entries.push(current.build(`phase-${phaseIndex++}`, sessions));
    }
    current = null;
  };

  for (const entry of timeline) {
    // Only messages carry the session identity that defines a phase; other
    // entries attach to whichever phase is open around them.
    if (entry.kind === "message") {
      if (entry.role === "user") continue;

      const agentType = entry.agentType || "worker";
      if (!current || current.agentType !== agentType) {
        closeCurrent();
        entries.push({
          kind: "handoff",
          id: `handoff-${entries.length}`,
          from: previousAgent,
          to: agentType,
          label: previousAgent ? "Handoff" : "Dispatched",
          timestamp: formatClock(
            sessionsById.get(entry.sessionId)?.startedAt ?? entry.timestamp,
          ),
        });
        previousAgent = agentType;
        current = new PhaseBuilder(agentType, entry.sessionId);
      } else if (entry.sessionId) {
        // Same agent, new session id — a retry of this phase, not a new phase.
        current.sessionIds.add(entry.sessionId);
      }

      applyMessage(current, entry);
      continue;
    }

    if (!current) continue;
    if (entry.kind === "command") applyCommand(current, entry);
    else if (entry.kind === "comment") applyComment(current, entry);
  }

  closeCurrent();

  // Sessions that produced no renderable messages still happened. Fold their
  // attempt counts into the matching phase so a crash-looping agent is visible.
  for (const session of sessions) {
    const known = entries.some(
      (e) => e.kind === "phase" && e.agentType === session.agentType,
    );
    if (known) continue;
    if (!RUNNING_STATUSES.has(session.status) && !FAILED_STATUSES.has(session.status)) continue;
    entries.push({
      kind: "phase",
      id: `phase-${phaseIndex++}`,
      title: phaseTitle(session.agentType),
      agentType: session.agentType,
      modelId: session.modelId,
      durationLabel: formatDuration(session.startedAt, session.endedAt),
      running: RUNNING_STATUSES.has(session.status),
      turns: [],
    });
  }

  // Attempt counts per agent across the whole task, not just per phase run.
  const byAgent = new Map<string, SessionInfo[]>();
  for (const s of sessions) {
    const list = byAgent.get(s.agentType) ?? [];
    list.push(s);
    byAgent.set(s.agentType, list);
  }
  for (const entry of entries) {
    if (entry.kind !== "phase" || !entry.agentType) continue;
    const own = byAgent.get(entry.agentType) ?? [];
    const failed = own.filter((s) => FAILED_STATUSES.has(s.status)).length;
    if (own.length > 1) entry.attempts = { total: own.length, failed };
  }

  const agents: LedgerAgentStatus[] = Array.from(byAgent.entries()).map(([agentType, own]) => {
    const running = own.some((s) => RUNNING_STATUSES.has(s.status));
    const failed = own.filter((s) => FAILED_STATUSES.has(s.status)).length;
    const first = own[0];
    const last = own[own.length - 1];
    return {
      agentType,
      durationLabel: formatDuration(first?.startedAt, running ? undefined : last?.endedAt) ?? "",
      status: running ? "running" : failed === own.length ? "failed" : "done",
      running,
    };
  });

  const activeSession = sessions.find((s) => RUNNING_STATUSES.has(s.status));
  const live: LedgerLiveState | null = activeSession
    ? {
        agentType: activeSession.agentType,
        durationLabel: formatDuration(activeSession.startedAt) ?? "",
        stepLabel: "",
        nowLabel: "",
      }
    : null;

  return {
    entries,
    agents,
    criteria: criteria.map((c) => ({ text: c.criterion, met: c.met })),
    live,
  };
}
