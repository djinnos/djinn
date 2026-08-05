/**
 * SessionLedger — structural view of a task's execution.
 *
 * Renders PHASE > TURN > (activity | say | artifact) with HANDOFF bands between
 * phases, replacing the flat message list. Three things it surfaces that the
 * flat thread cannot:
 *
 *   1. Tool calls. The thread drops every `tool_use` that isn't one of six
 *      "final" names, so the actual work is invisible; here reasoning and tool
 *      calls share one chronological strand.
 *   2. Retries. A phase can span several sessions; failed attempts are counted
 *      on the band instead of vanishing.
 *   3. Liveness. A running phase says what it is doing right now.
 *
 * Presentational only — driven entirely by props so stories can exercise states
 * that are hard to catch in production.
 */

import { useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { cn } from "@/lib/utils";
import { getAgentIdentity } from "@/lib/agentIdentity";
import type {
  AcceptanceCriterion,
  ActivityStrand,
  ArtifactBlock,
  ArtifactVariant,
  LedgerAgentStatus,
  LedgerHandoff,
  LedgerPhase,
  LedgerTurn,
  SayBlock,
  SessionLedgerProps,
  TurnBlock,
} from "./ledger";

// ── Per-agent accents ───────────────────────────────────────────────────────
// Mirrors the text colours in agentIdentity so the rail, band and dot agree.

interface AgentAccent {
  rail: string;
  text: string;
  dot: string;
  band: string;
}

const ACCENTS: Record<string, AgentAccent> = {
  worker: { rail: "bg-blue-400/30", text: "text-blue-300", dot: "bg-blue-400", band: "border-blue-400/25" },
  reviewer: { rail: "bg-amber-400/30", text: "text-amber-300", dot: "bg-amber-400", band: "border-amber-400/25" },
  lead: { rail: "bg-red-400/30", text: "text-red-300", dot: "bg-red-400", band: "border-red-400/25" },
  planner: { rail: "bg-purple-400/30", text: "text-purple-300", dot: "bg-purple-400", band: "border-purple-400/25" },
  architect: { rail: "bg-emerald-400/30", text: "text-emerald-300", dot: "bg-emerald-400", band: "border-emerald-400/25" },
};

const NEUTRAL_ACCENT: AgentAccent = {
  rail: "bg-zinc-500/25",
  text: "text-zinc-300",
  dot: "bg-zinc-400",
  band: "border-white/10",
};

function accentFor(agentType?: string): AgentAccent {
  return (agentType && ACCENTS[agentType]) || NEUTRAL_ACCENT;
}

// ── Small shared bits ───────────────────────────────────────────────────────

function LiveDot({ className }: { className?: string }) {
  return (
    <span className="relative inline-flex h-1.5 w-1.5 shrink-0">
      <span
        className={cn(
          "absolute inline-flex h-full w-full rounded-full opacity-70 motion-safe:animate-ping",
          className,
        )}
      />
      <span className={cn("relative inline-flex h-1.5 w-1.5 rounded-full", className)} />
    </span>
  );
}

function Eyebrow({ children, className }: { children: React.ReactNode; className?: string }) {
  return (
    <span className={cn("text-[10px] font-semibold uppercase tracking-[0.12em]", className)}>
      {children}
    </span>
  );
}

// ── Activity strand ─────────────────────────────────────────────────────────

const STEP_GLYPH: Record<string, string> = {
  thinking: "▸",
  tool: "⌘",
};

/**
 * Reasoning + tool calls as one collapsible run. Deliberately neutral-grey:
 * `--primary` is the brand/interactive purple, and spending it on the least
 * important content is what makes the current thread read wrong.
 */
function ActivityRun({ strand }: { strand: ActivityStrand }) {
  const [open, setOpen] = useState(false);

  const tools = strand.steps.filter((s) => s.kind === "tool");
  const byName = new Map<string, number>();
  for (const t of tools) byName.set(t.label, (byName.get(t.label) ?? 0) + 1);
  const summary = Array.from(byName.entries())
    .sort((a, b) => b[1] - a[1])
    .slice(0, 4)
    .map(([name, n]) => `${name.toLowerCase()} ${n}`)
    .join(" · ");

  return (
    <div className="rounded-md border border-white/[0.06] bg-white/[0.015]">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open}
        className="flex w-full items-center gap-2 rounded-md px-2.5 py-1.5 text-left text-xs text-zinc-400 transition-colors hover:bg-white/[0.03] focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-white/25"
      >
        <span className="text-zinc-600">⋯</span>
        <span className="font-medium text-zinc-300 tabular-nums">
          {strand.steps.length} {strand.steps.length === 1 ? "step" : "steps"}
        </span>
        {strand.durationLabel && (
          <span className="text-zinc-500 tabular-nums">· {strand.durationLabel}</span>
        )}
        {summary && <span className="truncate text-zinc-500">· {summary}</span>}
        <span className="ml-auto flex shrink-0 items-center gap-2">
          {strand.running && (
            <span className="flex items-center gap-1.5 text-[11px] text-zinc-300">
              <LiveDot className="bg-emerald-400" />
              running
            </span>
          )}
          <span className="text-[11px] text-zinc-500">{open ? "collapse" : "expand"}</span>
        </span>
      </button>

      {strand.running && strand.nowLabel && !open && (
        <div className="flex items-center gap-2 border-t border-white/[0.05] px-2.5 py-1.5 text-[11px] text-zinc-400">
          <span className="text-zinc-600">now →</span>
          <span className="truncate">{strand.nowLabel}</span>
        </div>
      )}

      {open && (
        <ol className="border-t border-white/[0.05] py-1">
          {strand.steps.map((step, i) => (
            <li
              key={i}
              className="flex items-baseline gap-2.5 px-2.5 py-[3px] text-[11px] leading-relaxed"
            >
              <span className="w-5 shrink-0 text-right tabular-nums text-zinc-600">
                {String(i + 1).padStart(2, "0")}
              </span>
              <span className="w-3 shrink-0 text-zinc-600">{STEP_GLYPH[step.kind]}</span>
              <span
                className={cn(
                  "w-16 shrink-0 truncate",
                  step.kind === "tool" ? "font-medium text-zinc-300" : "italic text-zinc-500",
                )}
              >
                {step.kind === "tool" ? step.label : "thinking"}
              </span>
              <span className="min-w-0 flex-1 truncate font-mono text-zinc-400">
                {step.kind === "tool" ? step.detail : step.label}
              </span>
              {step.meta && (
                <span className="shrink-0 tabular-nums text-zinc-600">{step.meta}</span>
              )}
            </li>
          ))}
          {strand.running && strand.nowLabel && (
            <li className="flex items-baseline gap-2.5 px-2.5 py-[3px] text-[11px]">
              <span className="w-5 shrink-0 text-right text-zinc-600">→</span>
              <span className="w-3 shrink-0" />
              <span className="flex items-center gap-1.5 text-zinc-300">
                <LiveDot className="bg-emerald-400" />
                {strand.nowLabel}
              </span>
            </li>
          )}
        </ol>
      )}
    </div>
  );
}

// ── Artifact card ───────────────────────────────────────────────────────────

const ARTIFACT_LABEL: Record<ArtifactVariant, string> = {
  work_submitted: "Work submitted",
  review_submitted: "Review submitted",
  lead_decision: "Lead decision",
  grooming: "Grooming complete",
  escalated: "Escalated",
};

function ArtifactCard({ block, agentType }: { block: ArtifactBlock; agentType: string }) {
  const [filesOpen, setFilesOpen] = useState(false);
  const accent = accentFor(agentType);
  const files = block.files ?? [];
  const concerns = block.concerns ?? [];

  return (
    <div className={cn("overflow-hidden rounded-lg border bg-card/80", accent.band)}>
      <div
        className={cn(
          "flex items-center gap-2 border-b px-3 py-2",
          accent.band,
          "bg-white/[0.02]",
        )}
      >
        <Eyebrow className={accent.text}>{ARTIFACT_LABEL[block.variant]}</Eyebrow>
        {block.outcome && (
          <span className="rounded bg-white/[0.06] px-1.5 py-0.5 text-[10px] font-medium text-zinc-200">
            {block.outcome}
          </span>
        )}
        <span className="ml-auto shrink-0 text-[11px] tabular-nums text-zinc-500">
          {block.timestamp}
        </span>
      </div>

      <div className="px-3 py-2.5">
        <p className="text-sm leading-relaxed text-zinc-200">{block.summary}</p>

        {(files.length > 0 || concerns.length > 0) && (
          <div className="mt-2.5 flex flex-col gap-1.5">
            {files.length > 0 && (
              <div>
                <button
                  type="button"
                  onClick={() => setFilesOpen((v) => !v)}
                  aria-expanded={filesOpen}
                  className="flex w-full items-center gap-2 rounded px-1 py-1 text-left text-xs text-zinc-400 transition-colors hover:bg-white/[0.03] focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-white/25"
                >
                  <span className="text-zinc-600">{filesOpen ? "▾" : "▸"}</span>
                  <span className="font-medium tabular-nums text-zinc-300">
                    {files.length} files changed
                  </span>
                  <span className="ml-auto text-[11px] text-zinc-500">view diff</span>
                </button>
                {filesOpen && (
                  <ul className="mt-1 flex flex-col gap-px overflow-x-auto pl-6">
                    {files.map((f) => (
                      <li key={f} className="whitespace-nowrap font-mono text-[11px] text-zinc-400">
                        {f}
                      </li>
                    ))}
                  </ul>
                )}
              </div>
            )}

            {concerns.map((c) => (
              <div
                key={c}
                className="flex items-baseline gap-2 rounded border border-amber-400/20 bg-amber-400/[0.06] px-2 py-1.5"
              >
                <span className="shrink-0 text-amber-400/80">⚠</span>
                <span className="text-xs leading-relaxed text-amber-100/80">{c}</span>
              </div>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

// ── Say block ───────────────────────────────────────────────────────────────

function Say({ block }: { block: SayBlock }) {
  return (
    <div className="rounded-lg bg-card px-3.5 py-2.5">
      <div className="prose prose-sm max-w-none dark:prose-invert prose-p:my-1 prose-pre:my-1 prose-ul:my-1 prose-li:my-0.5 prose-headings:text-sm">
        <ReactMarkdown remarkPlugins={[remarkGfm]}>{block.markdown}</ReactMarkdown>
      </div>
    </div>
  );
}

// ── Turn ────────────────────────────────────────────────────────────────────

function TurnBlockView({ block, agentType }: { block: TurnBlock; agentType: string }) {
  switch (block.kind) {
    case "activity":
      return <ActivityRun strand={block} />;
    case "say":
      return <Say block={block} />;
    case "artifact":
      return <ArtifactCard block={block} agentType={agentType} />;
  }
}

/** One agent, one loop iteration — a single avatar no matter how many blocks. */
function Turn({ turn }: { turn: LedgerTurn }) {
  const agent = getAgentIdentity(turn.agentType);
  const accent = accentFor(turn.agentType);

  return (
    <div className="flex gap-3">
      <div className="flex w-12 shrink-0 flex-col items-center">
        <img src={agent.avatar} alt="" className="h-12 w-12 shrink-0 object-contain" />
        <Eyebrow className={cn("mt-0.5 text-center leading-tight", accent.text)}>
          {agent.label}
        </Eyebrow>
        <div className={cn("mt-1.5 w-px flex-1 rounded", accent.rail)} />
      </div>
      <div className="flex min-w-0 flex-1 flex-col gap-2 pb-4">
        {turn.blocks.map((block, i) => (
          <TurnBlockView key={i} block={block} agentType={turn.agentType} />
        ))}
      </div>
    </div>
  );
}

// ── Brief ───────────────────────────────────────────────────────────────────

function BriefBand({ phase }: { phase: LedgerPhase }) {
  const [open, setOpen] = useState(true);
  const brief = phase.brief;
  if (!brief) return null;

  if (!open) {
    return (
      <button
        type="button"
        onClick={() => setOpen(true)}
        className="flex w-full items-center gap-2 rounded-md border border-white/[0.06] bg-white/[0.015] px-3 py-2 text-left text-xs text-zinc-400 transition-colors hover:bg-white/[0.03] focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-white/25"
      >
        <span className="text-zinc-600">▸</span>
        <Eyebrow className="text-zinc-400">Brief</Eyebrow>
        <span className="text-zinc-500">
          filed by {brief.filedBy} · {brief.timestamp}
        </span>
        <span className="ml-auto text-[11px] text-zinc-500">expand</span>
      </button>
    );
  }

  return (
    <section className="rounded-lg border border-white/[0.08] bg-white/[0.02]">
      <header className="flex items-center gap-2 border-b border-white/[0.06] px-3.5 py-2">
        <Eyebrow className="text-zinc-300">Brief</Eyebrow>
        <span className="text-[11px] text-zinc-500">
          filed by {brief.filedBy} · {brief.timestamp}
        </span>
        <button
          type="button"
          onClick={() => setOpen(false)}
          className="ml-auto rounded px-1.5 py-0.5 text-[11px] text-zinc-500 transition-colors hover:bg-white/[0.05] hover:text-zinc-300 focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-white/25"
        >
          collapse
        </button>
      </header>
      <div className="px-3.5 py-3">
        <p className="max-w-[68ch] text-sm leading-relaxed text-zinc-300">{brief.body}</p>
        {brief.facets && brief.facets.length > 0 && (
          <div className="mt-3 flex flex-wrap gap-1.5">
            {brief.facets.map((f) => (
              <span
                key={f.label}
                className="rounded border border-white/[0.08] px-2 py-0.5 text-[11px] text-zinc-400"
              >
                ▸ {f.label}
                {f.count !== undefined && (
                  <span className="ml-1 tabular-nums text-zinc-500">{f.count}</span>
                )}
              </span>
            ))}
          </div>
        )}
      </div>
    </section>
  );
}

// ── Phase + handoff ─────────────────────────────────────────────────────────

function HandoffBand({ handoff }: { handoff: LedgerHandoff }) {
  const to = getAgentIdentity(handoff.to);
  const from = handoff.from ? getAgentIdentity(handoff.from) : null;

  return (
    <div className="flex items-center gap-3 py-1.5" role="separator">
      <div className="h-px flex-1 bg-gradient-to-r from-transparent to-white/15" />
      <div className="flex shrink-0 items-center gap-2 text-[11px]">
        <span className="text-zinc-600">⇄</span>
        <Eyebrow className="text-zinc-400">{handoff.label}</Eyebrow>
        <span className="text-zinc-500">
          {from ? `${from.label} → ${to.label}` : to.label}
        </span>
        <span className="tabular-nums text-zinc-600">{handoff.timestamp}</span>
      </div>
      <div className="h-px flex-1 bg-gradient-to-l from-transparent to-white/15" />
    </div>
  );
}

function PhaseBand({ phase }: { phase: LedgerPhase }) {
  const accent = accentFor(phase.agentType);
  const agent = phase.agentType ? getAgentIdentity(phase.agentType) : null;

  return (
    <section className="flex flex-col gap-3">
      <header className={cn("flex items-center gap-2 border-l-2 pl-2.5", accent.band)}>
        <Eyebrow className={accent.text}>{phase.title}</Eyebrow>
        <span className="flex min-w-0 items-center gap-1.5 text-[11px] text-zinc-500">
          {agent && <span className="shrink-0">{agent.label}</span>}
          {phase.modelId && <span className="truncate font-mono">· {phase.modelId}</span>}
          {phase.durationLabel && (
            <span className="shrink-0 tabular-nums">· {phase.durationLabel}</span>
          )}
        </span>
        <span className="ml-auto flex shrink-0 items-center gap-2">
          {phase.attempts && phase.attempts.failed > 0 && (
            <span
              className="rounded border border-red-400/25 bg-red-400/10 px-1.5 py-0.5 text-[10px] font-medium tabular-nums text-red-300"
              title={`${phase.attempts.failed} of ${phase.attempts.total} sessions failed and respawned`}
            >
              attempt {phase.attempts.total} · {phase.attempts.failed} failed
            </span>
          )}
          {phase.running && (
            <span className="flex items-center gap-1.5 text-[11px] text-zinc-300">
              <LiveDot className="bg-emerald-400" />
              running
            </span>
          )}
        </span>
      </header>

      <div className="flex flex-col">
        {phase.turns.map((turn) => (
          <Turn key={turn.id} turn={turn} />
        ))}
      </div>
    </section>
  );
}

// ── Rail ────────────────────────────────────────────────────────────────────

function CriteriaMeter({ criteria }: { criteria: AcceptanceCriterion[] }) {
  const met = criteria.filter((c) => c.met).length;
  return (
    <div className="flex items-center gap-2">
      <div
        className="flex h-1.5 flex-1 gap-px overflow-hidden rounded-full"
        role="img"
        aria-label={`${met} of ${criteria.length} acceptance criteria met`}
      >
        {criteria.map((c, i) => (
          <span
            key={i}
            className={cn("h-full flex-1 rounded-sm", c.met ? "bg-emerald-400/70" : "bg-white/10")}
          />
        ))}
      </div>
      <span className="shrink-0 text-[11px] tabular-nums text-zinc-400">
        {met}/{criteria.length}
      </span>
    </div>
  );
}

function Rail({
  criteria,
  agents,
  blockers,
}: {
  criteria: AcceptanceCriterion[];
  agents: LedgerAgentStatus[];
  blockers: string[];
}) {
  return (
    <aside className="flex w-52 shrink-0 flex-col gap-5 overflow-y-auto border-r border-white/[0.06] px-3 py-4">
      <section className="flex flex-col gap-2.5">
        <Eyebrow className="text-zinc-500">Criteria</Eyebrow>
        <CriteriaMeter criteria={criteria} />
        <ul className="flex flex-col gap-2">
          {criteria.map((c, i) => (
            <li key={i} className="flex gap-1.5">
              <span
                className={cn(
                  "mt-[3px] shrink-0 text-[9px]",
                  c.met ? "text-emerald-400" : "text-zinc-600",
                )}
              >
                {c.met ? "●" : "○"}
              </span>
              <div className="min-w-0">
                <p
                  className={cn(
                    "line-clamp-3 text-[11px] leading-snug",
                    c.met ? "text-zinc-500" : "text-zinc-300",
                  )}
                  title={c.text}
                >
                  {c.text}
                </p>
                {c.metAt && (
                  <span className="text-[10px] tabular-nums text-zinc-600">{c.metAt}</span>
                )}
                {c.note && <p className="text-[10px] leading-snug text-amber-300/70">{c.note}</p>}
              </div>
            </li>
          ))}
        </ul>
      </section>

      <section className="flex flex-col gap-2">
        <Eyebrow className="text-zinc-500">Agents</Eyebrow>
        <ul className="flex flex-col gap-1.5">
          {agents.map((a) => {
            const identity = getAgentIdentity(a.agentType);
            const accent = accentFor(a.agentType);
            return (
              <li key={a.agentType} className="flex items-center gap-2 text-[11px]">
                <span className={cn("h-1.5 w-1.5 shrink-0 rounded-full", accent.dot)} />
                <span className={cn("shrink-0", accent.text)}>{identity.label}</span>
                <span className="tabular-nums text-zinc-600">{a.durationLabel}</span>
                <span className="ml-auto flex shrink-0 items-center gap-1 text-zinc-500">
                  {a.running && <LiveDot className="bg-emerald-400" />}
                  {a.status}
                </span>
              </li>
            );
          })}
        </ul>
      </section>

      <section className="flex flex-col gap-2">
        <Eyebrow className="text-zinc-500">Blockers</Eyebrow>
        {blockers.length === 0 ? (
          <span className="text-[11px] text-zinc-600">none</span>
        ) : (
          <ul className="flex flex-col gap-1">
            {blockers.map((b) => (
              <li key={b} className="text-[11px] leading-snug text-red-300/80">
                {b}
              </li>
            ))}
          </ul>
        )}
      </section>
    </aside>
  );
}

// ── Shell ───────────────────────────────────────────────────────────────────

export function SessionLedger({
  taskShortId,
  taskTitle,
  statusLabel,
  usageLabel,
  criteria,
  agents,
  blockers = [],
  entries,
  live = null,
  showHeader = true,
  emptyMessage = "No session activity yet.",
}: SessionLedgerProps) {
  const metCount = criteria.filter((c) => c.met).length;
  const liveIdentity = live ? getAgentIdentity(live.agentType) : null;
  const liveAccent = accentFor(live?.agentType);

  return (
    <div className="flex h-full flex-col bg-background text-foreground">
      {showHeader && (
      <header className="flex shrink-0 items-center gap-3 border-b border-white/[0.06] px-4 py-2.5">
        <span className="shrink-0 font-mono text-xs text-zinc-500">{taskShortId}</span>
        <h1 className="min-w-0 flex-1 truncate text-sm font-semibold text-zinc-100">{taskTitle}</h1>
        <span className="hidden shrink-0 items-center gap-1.5 text-[11px] text-zinc-500 md:flex">
          <span className="tabular-nums">
            {metCount}/{criteria.length} met
          </span>
        </span>
        {usageLabel && (
          <span className="hidden shrink-0 font-mono text-[11px] tabular-nums text-zinc-600 lg:inline">
            {usageLabel}
          </span>
        )}
        <span className="shrink-0 rounded border border-amber-400/30 bg-amber-400/10 px-2 py-0.5 text-[10px] font-medium text-amber-300">
          {statusLabel}
        </span>
      </header>
      )}

      <div className="flex min-h-0 flex-1">
        <Rail criteria={criteria} agents={agents} blockers={blockers} />

        <main className="min-w-0 flex-1 overflow-y-auto">
          <div className="mx-auto flex max-w-3xl flex-col gap-4 px-5 py-5">
            {entries.length === 0 && (
              <p className="py-8 text-center text-sm text-zinc-500">{emptyMessage}</p>
            )}
            {entries.map((entry) =>
              entry.kind === "handoff" ? (
                <HandoffBand key={entry.id} handoff={entry} />
              ) : entry.brief ? (
                <BriefBand key={entry.id} phase={entry} />
              ) : (
                <PhaseBand key={entry.id} phase={entry} />
              ),
            )}
          </div>
        </main>
      </div>

      <footer className="flex shrink-0 items-center gap-2.5 border-t border-white/[0.06] bg-white/[0.02] px-4 py-2 text-[11px]">
        {live && liveIdentity ? (
          <>
            <LiveDot className="bg-emerald-400" />
            <span className={cn("font-medium", liveAccent.text)}>{liveIdentity.label}</span>
            <span className="tabular-nums text-zinc-500">{live.durationLabel}</span>
            <span className="text-zinc-600">·</span>
            <span className="tabular-nums text-zinc-500">{live.stepLabel}</span>
            <span className="min-w-0 truncate text-zinc-400">{live.nowLabel}</span>
            <div className="ml-auto flex shrink-0 gap-1.5">
              <button
                type="button"
                className="rounded border border-white/10 px-2 py-0.5 text-zinc-400 transition-colors hover:bg-white/[0.06] hover:text-zinc-200 focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-white/25"
              >
                Logs
              </button>
              <button
                type="button"
                className="rounded border border-red-400/25 px-2 py-0.5 text-red-300/90 transition-colors hover:bg-red-400/10 focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-red-400/40"
              >
                Kill
              </button>
            </div>
          </>
        ) : (
          <>
            <span className="h-1.5 w-1.5 shrink-0 rounded-full bg-zinc-600" />
            <span className="text-zinc-500">no session running</span>
            <span className="ml-auto text-zinc-600">{statusLabel}</span>
          </>
        )}
      </footer>
    </div>
  );
}
