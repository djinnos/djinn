/**
 * SessionThread — chat-style view for task execution.
 *
 * Renders a centered, flowing timeline (max-w-3xl like the chat UI) that surfaces
 * only meaningful content: agent text, final tool cards, comments, and commands.
 */

import { useEffect, useRef, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { cn } from "@/lib/utils";
import { getAgentIdentity } from "@/lib/agentIdentity";
import { StepLog } from "@/components/StepLog";
import {
  Test,
  TestError,
  TestErrorStack,
  TestResults,
  TestResultsContent,
  TestResultsDuration,
  TestResultsHeader,
  TestResultsProgress,
  TestResultsSummary,
  TestSuite,
  TestSuiteContent,
  TestSuiteName,
} from "@/components/ai-elements/test-results";
import type { SetupVerificationView } from "@/lib/setupVerificationView";
import type {
  TimelineEntry,
  ChatMessage,
  SystemDivider,
  CommandBlock,
  CommentBlock,
  ContentBlock,
  VerificationBlock,
} from "@/hooks/useSessionMessages";

// ── Chat bubble wrapper with RPG avatar ─────────────────────────────────────

function ChatBubble({ agentType, badge, children }: {
  agentType: string;
  badge?: string;
  children: React.ReactNode;
}) {
  const agent = getAgentIdentity(agentType);
  return (
    <div className="relative py-3">
      {/* Speech bubble — avatar is inside, positioned absolute bottom-left into the margin */}
      <div className="relative overflow-visible rounded-lg rounded-bl-none bg-card px-4 py-3">
        {/* Avatar in the left margin */}
        <div className="absolute bottom-0 -left-32">
          <div className={cn("text-center text-[9px] font-bold uppercase tracking-wider", agent.color)}>
            {agent.label}
          </div>
          <img
            src={agent.avatar}
            alt={agent.label}
            className="h-24 object-contain drop-shadow-lg"
          />
        </div>
        {/* Speech tail — triangle on bottom-left pointing toward avatar */}
        <svg
          className="absolute -left-[14px] bottom-0 h-[14px] w-[14px] text-card"
          viewBox="0 0 16 16"
          fill="none"
        >
          <path d="M16 16H0L16 0V16Z" fill="currentColor" />
        </svg>
        {badge && (
          <div className="mb-1.5">
            <span className="rounded bg-muted/60 px-1.5 py-0.5 text-[10px] font-medium text-muted-foreground">
              {badge}
            </span>
          </div>
        )}
        {children}
      </div>
    </div>
  );
}

// ── Final tool calls that we render prominently ─────────────────────────────

const FINAL_TOOL_NAMES = new Set([
  "submit_work",
  "submit_review",
  "submit_decision",
  "submit_grooming",
  "request_lead",
  "request_architect",
]);

// ── Render a final tool call as a formatted card ────────────────────────────

function FinalToolCard({ block, agentType }: { block: ContentBlock; agentType: string }) {
  const name = block.name ?? "result";
  const input = block.input as Record<string, unknown> | undefined;
  if (!input) return null;

  const md = buildFinalToolMarkdown(name, input);

  return (
    <ChatBubble agentType={agentType} badge={formatToolLabel(name)}>
      <div className="prose prose-sm max-w-none dark:prose-invert prose-p:my-1 prose-pre:my-1 prose-ul:my-1 prose-li:my-0.5">
        <ReactMarkdown remarkPlugins={[remarkGfm]}>{md}</ReactMarkdown>
      </div>
    </ChatBubble>
  );
}

function formatToolLabel(name: string): string {
  switch (name) {
    case "submit_work": return "Work Submitted";
    case "submit_review": return "Review Submitted";
    case "submit_decision": return "Lead Decision";
    case "submit_grooming": return "Grooming Complete";
    case "request_lead": return "Escalated to Lead";
    case "request_architect": return "Escalated to Architect";
    default: return name;
  }
}

function buildFinalToolMarkdown(name: string, input: Record<string, unknown>): string {
  const lines: string[] = [];

  switch (name) {
    case "submit_work": {
      if (input.summary) lines.push(String(input.summary));
      const files = input.files_changed as string[] | undefined;
      if (files?.length) {
        lines.push("");
        lines.push("**Files changed:**");
        for (const f of files) lines.push(`- \`${f}\``);
      }
      const concerns = input.remaining_concerns as string[] | undefined;
      if (concerns?.length) {
        lines.push("");
        lines.push("**Remaining concerns:**");
        for (const c of concerns) lines.push(`- ${c}`);
      }
      break;
    }
    case "submit_review": {
      const verdict = input.verdict as string | undefined;
      if (verdict) {
        const icon = verdict === "approved" ? "✓" : "✗";
        lines.push(`**Verdict:** ${icon} ${verdict}`);
      }
      if (input.feedback) {
        lines.push("");
        lines.push(String(input.feedback));
      }
      const ac = input.acceptance_criteria as Array<{ criterion: string; met: boolean }> | undefined;
      if (ac?.length) {
        lines.push("");
        lines.push("**Acceptance Criteria:**");
        for (const c of ac) {
          lines.push(`- ${c.met ? "✓" : "✗"} ${c.criterion}`);
        }
      }
      break;
    }
    case "submit_decision": {
      if (input.decision) lines.push(`**Decision:** ${input.decision}`);
      if (input.rationale) {
        lines.push("");
        lines.push(String(input.rationale));
      }
      const created = input.created_tasks as string[] | undefined;
      if (created?.length) {
        lines.push("");
        lines.push("**Created tasks:**");
        for (const t of created) lines.push(`- ${t}`);
      }
      break;
    }
    case "submit_grooming": {
      if (input.summary) lines.push(String(input.summary));
      const tasks = input.tasks_reviewed as Array<{ task_id: string; action: string; changes?: string }> | undefined;
      if (tasks?.length) {
        lines.push("");
        for (const t of tasks) {
          lines.push(`- **${t.task_id}** — ${t.action}${t.changes ? `: ${t.changes}` : ""}`);
        }
      }
      break;
    }
    case "request_lead":
    case "request_architect": {
      if (input.reason) lines.push(String(input.reason));
      if (input.suggested_breakdown) {
        lines.push("");
        lines.push("**Suggested breakdown:**");
        lines.push(String(input.suggested_breakdown));
      }
      break;
    }
    default:
      lines.push("```json\n" + JSON.stringify(input, null, 2) + "\n```");
  }

  return lines.join("\n");
}


// ── Thinking block component ────────────────────────────────────────────────

function ThinkingBlock({ text }: { text: string }) {
  const [expanded, setExpanded] = useState(false);
  const preview = text.slice(0, 120).replace(/\n/g, " ");

  return (
    <div className="my-1">
      <button
        type="button"
        className="flex items-center gap-1.5 rounded bg-purple-500/10 px-2 py-1 text-xs text-purple-300/80 transition-colors hover:bg-purple-500/15"
        onClick={() => setExpanded(!expanded)}
      >
        <span className="font-mono">{expanded ? "▾" : "▸"}</span>
        <span className="italic">thinking</span>
        {!expanded && (
          <span className="max-w-xs truncate opacity-60">{preview}…</span>
        )}
      </button>
      {expanded && (
        <pre className="mt-1 max-h-80 overflow-auto rounded bg-purple-500/5 p-2 text-xs text-purple-200/70 whitespace-pre-wrap break-words">
          {text}
        </pre>
      )}
    </div>
  );
}

// ── Process a message into renderable pieces ────────────────────────────────

interface ProcessedMessage {
  finalTools: ContentBlock[];
  textBlocks: ContentBlock[];
  thinkingBlocks: ContentBlock[];
  agentType: string;
}

function processMessage(entry: ChatMessage): ProcessedMessage | null {
  // Hide user messages entirely
  if (entry.role === "user") return null;

  const finalTools: ContentBlock[] = [];
  const textBlocks: ContentBlock[] = [];
  const thinkingBlocks: ContentBlock[] = [];

  for (const block of entry.content) {
    if (block.type === "tool_use") {
      const name = block.name ?? "";
      if (FINAL_TOOL_NAMES.has(name)) {
        finalTools.push(block);
      }
    } else if (block.type === "text" && block.text) {
      textBlocks.push(block);
    } else if (block.type === "thinking" && block.thinking) {
      thinkingBlocks.push(block);
    }
  }

  if (finalTools.length === 0 && textBlocks.length === 0 && thinkingBlocks.length === 0) return null;

  return { finalTools, textBlocks, thinkingBlocks, agentType: entry.agentType };
}

// ── Message rendering ───────────────────────────────────────────────────────

function MessageBlock({ entry }: { entry: ChatMessage }) {
  const processed = processMessage(entry);
  if (!processed) return null;

  const { finalTools, textBlocks, thinkingBlocks, agentType } = processed;

  // Skip messages that only have tool calls and no visible content
  if (finalTools.length === 0 && textBlocks.length === 0 && thinkingBlocks.length === 0) return null;

  return (
    <div>
      {/* Thinking blocks — collapsible reasoning */}
      {thinkingBlocks.map((block, idx) => (
        <ThinkingBlock key={`think-${idx}`} text={block.thinking as string} />
      ))}

      {/* Agent text responses — chat bubble with RPG avatar */}
      {textBlocks.length > 0 && (
        <ChatBubble agentType={agentType}>
          {textBlocks.map((block, idx) => (
            <div key={idx} className="prose prose-sm max-w-none dark:prose-invert prose-p:my-1 prose-pre:my-1">
              <ReactMarkdown remarkPlugins={[remarkGfm]}>{block.text!}</ReactMarkdown>
            </div>
          ))}
        </ChatBubble>
      )}

      {/* Final tool cards */}
      {finalTools.map((block, idx) => (
        <FinalToolCard key={idx} block={block} agentType={agentType} />
      ))}
    </div>
  );
}


// ── Comment card (architect notes, corrective notes, etc.) ──────────────────

function CommentCard({ entry }: { entry: CommentBlock }) {
  return (
    <div className="py-3">
      <div className="rounded-lg bg-muted/30 px-4 py-3">
        <div className="mb-1.5">
          <span className="text-[10px] font-medium uppercase tracking-wider text-muted-foreground">
            System Note
          </span>
        </div>
        <div className="prose prose-sm max-w-none text-muted-foreground dark:prose-invert prose-p:my-1 prose-pre:my-1">
          <ReactMarkdown remarkPlugins={[remarkGfm]}>{entry.body}</ReactMarkdown>
        </div>
      </div>
    </div>
  );
}

// ── Result card (status transition outcome) ─────────────────────────────────

function ResultCard({ entry }: { entry: CommandBlock }) {
  const [expanded, setExpanded] = useState(!entry.passed);
  const hasBody = !!entry.body?.trim();

  return (
    <div
      className={cn(
        "my-2 overflow-hidden rounded-lg border",
        entry.passed ? "border-emerald-500/30" : "border-red-500/30",
      )}
    >
      <div
        className={cn(
          "flex items-center gap-2.5 px-4 py-2.5",
          entry.passed ? "bg-emerald-500/5" : "bg-red-500/5",
        )}
      >
        <div
          className={cn(
            "flex h-5 w-5 items-center justify-center rounded-full text-xs font-bold",
            entry.passed
              ? "bg-emerald-500/20 text-emerald-400"
              : "bg-red-500/20 text-red-400",
          )}
        >
          {entry.passed ? "✓" : "✗"}
        </div>
        <span
          className={cn(
            "text-sm font-medium",
            entry.passed ? "text-emerald-400" : "text-red-400",
          )}
        >
          {entry.passed ? "Passed" : "Failed"}
        </span>
        {hasBody && (
          <button
            type="button"
            className="ml-auto text-xs text-muted-foreground/60 transition-colors hover:text-muted-foreground"
            onClick={() => setExpanded(!expanded)}
          >
            {expanded ? "Hide details" : "Show details"}
          </button>
        )}
      </div>
      {expanded && hasBody && (
        <div className={cn("border-t px-4 py-3", entry.passed ? "border-emerald-500/20" : "border-red-500/20")}>
          <div className="prose prose-sm max-w-none dark:prose-invert prose-p:my-1 prose-pre:my-1">
            <ReactMarkdown remarkPlugins={[remarkGfm]}>{entry.body}</ReactMarkdown>
          </div>
        </div>
      )}
    </div>
  );
}

// ── Verification block (historical step results from activity log) ──────────

function VerificationCard({ entry }: { entry: VerificationBlock }) {
  const phases = new Set(entry.steps.map((s) => s.phase));
  const hasMultiplePhases = phases.size > 1;

  // Group steps by phase for TestSuite sections
  const groupedByPhase = hasMultiplePhases
    ? Array.from(
        entry.steps.reduce((map, step) => {
          const group = map.get(step.phase) ?? [];
          group.push(step);
          map.set(step.phase, group);
          return map;
        }, new Map<string, typeof entry.steps>())
      )
    : [[phases.values().next().value as string, entry.steps] as const];

  const passed = entry.steps.filter((s) => s.passed).length;
  const failed = entry.steps.filter((s) => !s.passed).length;
  const total = entry.steps.length;

  return (
    <div className="my-2">
      <TestResults
        summary={{
          passed,
          failed,
          skipped: 0,
          total,
          duration: entry.totalDurationMs,
        }}
      >
        <TestResultsHeader>
          <TestResultsSummary />
          <TestResultsDuration />
        </TestResultsHeader>
        <div className="border-b px-4 py-3">
          <TestResultsProgress />
        </div>
        <TestResultsContent>
          {groupedByPhase.map(([phase, steps]) => {
            const suiteStatus = steps.every((s) => s.passed) ? "passed" : "failed";
            const label = phase === "setup" ? "Setup" : "Verification";
            return (
              <TestSuite key={phase} defaultOpen={suiteStatus === "failed" || steps.length <= 5} name={label} status={suiteStatus}>
                <TestSuiteName />
                <TestSuiteContent>
                  {steps.map((step, idx) =>
                    !step.passed && step.stderr ? (
                      <div key={idx}>
                        <Test
                          name={step.command}
                          status="failed"
                          duration={step.durationMs}
                        />
                        <TestError>
                          <TestErrorStack>{step.stderr}</TestErrorStack>
                        </TestError>
                      </div>
                    ) : (
                      <Test
                        key={idx}
                        name={step.command}
                        status={step.passed ? "passed" : "failed"}
                        duration={step.durationMs}
                      />
                    )
                  )}
                </TestSuiteContent>
              </TestSuite>
            );
          })}
        </TestResultsContent>
      </TestResults>
    </div>
  );
}

// ── Command block (setup/verification output) ───────────────────────────────

function splitOutput(body: string): { stdout: string; stderr: string } {
  const stderrIdx = body.indexOf("stderr:");
  const stdoutIdx = body.indexOf("stdout:");
  if (stdoutIdx !== -1 && stderrIdx !== -1) {
    const stdoutStart = stdoutIdx + 7;
    const stderrStart = stderrIdx + 7;
    if (stdoutIdx < stderrIdx) {
      return {
        stdout: body.slice(stdoutStart, stderrIdx).trim(),
        stderr: body.slice(stderrStart).trim(),
      };
    }
    return {
      stderr: body.slice(stderrStart, stdoutIdx).trim(),
      stdout: body.slice(stdoutStart).trim(),
    };
  }
  return { stdout: "", stderr: body };
}

function CommandRow({ entry }: { entry: CommandBlock }) {
  const [expanded, setExpanded] = useState(!entry.passed);
  const { stdout, stderr } = entry.body ? splitOutput(entry.body) : { stdout: "", stderr: "" };
  const hasOutput = !!(stdout || stderr);
  const label = entry.name === "setup" ? "Setup" : entry.name === "verification" ? "Verification" : entry.name;

  return (
    <div className="my-2 overflow-hidden rounded-lg border border-border">
      <button
        type="button"
        className="flex w-full items-center gap-2.5 px-3 py-2 text-xs transition-colors hover:bg-muted/30"
        onClick={() => setExpanded(!expanded)}
      >
        <span className="font-mono text-muted-foreground/60">{expanded ? "▾" : "▸"}</span>
        <span
          className={cn(
            "flex items-center gap-1.5 rounded-full px-2 py-0.5 text-[11px] font-medium",
            entry.passed
              ? "bg-emerald-500/15 text-emerald-400"
              : "bg-red-500/15 text-red-400"
          )}
        >
          {entry.passed ? "✓" : "✗"} {entry.passed ? "passed" : "failed"}
        </span>
        <span className="font-medium text-foreground">{label}</span>
        {entry.command && (
          <span className="font-mono text-muted-foreground">{entry.command}</span>
        )}
        {entry.exitCode != null && !entry.passed && (
          <span className="ml-auto font-mono text-muted-foreground">exit {entry.exitCode}</span>
        )}
      </button>
      {expanded && hasOutput && (
        <div className="border-t border-border">
          {stderr && (
            <div className="border-l-2 border-red-400 bg-red-950/40">
              <pre className="max-h-80 overflow-auto p-3 font-mono text-xs text-red-200 whitespace-pre-wrap break-words">
                {stderr}
              </pre>
            </div>
          )}
          {stdout && (
            <pre className="max-h-60 overflow-auto p-3 font-mono text-xs text-foreground/80 whitespace-pre-wrap break-words">
              {stdout}
            </pre>
          )}
        </div>
      )}
    </div>
  );
}

// ── Streaming thinking indicator ─────────────────────────────────────────────

function StreamingThinkingBubble({ text, agentType }: { text: string; agentType?: string }) {
  const agent = getAgentIdentity(agentType);
  return (
    <div className="relative py-3">
      <div className="relative overflow-visible rounded-lg border border-purple-500/20 bg-card px-4 py-3">
        <div className="absolute bottom-0 -left-32">
          <div className={cn("text-center text-[9px] font-bold uppercase tracking-wider", agent.color)}>
            {agent.label}
          </div>
          <img
            src={agent.avatar}
            alt={agent.label}
            className="h-24 object-contain drop-shadow-lg animate-pulse"
          />
        </div>
        <div className="mb-1.5">
          <span className="text-purple-400/60 text-[11px] italic">thinking</span>
        </div>
        <pre className="max-h-40 overflow-auto text-xs text-purple-200/60 whitespace-pre-wrap break-words">
          {text}
          <span className="inline-block h-3 w-1.5 animate-pulse bg-purple-400/60" />
        </pre>
      </div>
    </div>
  );
}

// ── Streaming indicator ──────────────────────────────────────────────────────

function StreamingBubble({ text, agentType }: { text: string; agentType?: string }) {
  return (
    <ChatBubble agentType={agentType ?? "worker"}>
      <div className="prose prose-sm max-w-none dark:prose-invert prose-p:my-1">
        <ReactMarkdown remarkPlugins={[remarkGfm]}>{text}</ReactMarkdown>
        <span className="inline-block h-3 w-1.5 animate-pulse bg-foreground/60" />
      </div>
    </ChatBubble>
  );
}

// ── Main thread component ────────────────────────────────────────────────────

interface SessionThreadProps {
  timeline: TimelineEntry[];
  streamingText: Map<string, string>;
  streamingThinking?: Map<string, string>;
  loading: boolean;
  error: string | null;
  activeAgentType?: string;
  setupVerification?: SetupVerificationView;
}

export function SessionThread({
  timeline,
  streamingText,
  streamingThinking,
  loading,
  error,
  activeAgentType,
  setupVerification,
}: SessionThreadProps) {
  const bottomRef = useRef<HTMLDivElement>(null);

  // Auto-scroll on new messages
  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [timeline.length, streamingText]);

  if (loading && timeline.length === 0) {
    return (
      <div className="flex flex-1 items-center justify-center text-sm text-muted-foreground">
        Loading session history…
      </div>
    );
  }

  if (error) {
    return (
      <div className="flex flex-1 items-center justify-center text-sm text-red-400">
        {error}
      </div>
    );
  }

  if (timeline.length === 0 && streamingText.size === 0) {
    return (
      <div className="flex flex-1 items-center justify-center text-sm text-muted-foreground">
        No session activity yet.
      </div>
    );
  }

  return (
    <div className="flex flex-1 flex-col overflow-y-auto">
      <div className="mx-auto w-full max-w-3xl px-4 py-3">
      {timeline.map((entry, idx) => {
        switch (entry.kind) {
          case "message":
            return <MessageBlock key={idx} entry={entry} />;
          case "divider": {
            // Hide divider text, but render StepLog at the last verification transition
            if (!setupVerification?.hasData) return null;
            const isVerificationTransition = entry.label.includes("Verifying") || entry.label.includes("Review");
            const isLastOfKind = isVerificationTransition && !timeline.slice(idx + 1).some(
              (e) => e.kind === "divider" && (
                (e as SystemDivider).label.includes("Verifying") ||
                (e as SystemDivider).label.includes("Review")
              )
            );
            if (!isLastOfKind) return null;
            return (
              <div key={idx} className="my-2">
                <StepLog
                  steps={setupVerification.steps}
                  status={setupVerification.status}
                  originalDurationMs={setupVerification.totalDuration}
                  emphasizedStepId={setupVerification.failedStepId}
                />
              </div>
            );
          }
          case "verification":
            return <VerificationCard key={idx} entry={entry} />;
          case "command":
            return entry.name === "result"
              ? <ResultCard key={idx} entry={entry} />
              : <CommandRow key={idx} entry={entry} />;
          case "comment":
            return <CommentCard key={idx} entry={entry} />;
          default:
            return null;
        }
      })}

      {/* Streaming thinking from active sessions */}
      {streamingThinking && Array.from(streamingThinking.entries()).map(([sessionId, text]) =>
        text ? (
          <StreamingThinkingBubble key={`thinking-${sessionId}`} text={text} agentType={activeAgentType} />
        ) : null
      )}

      {/* Streaming text from active sessions */}
      {Array.from(streamingText.entries()).map(([sessionId, text]) =>
        text ? (
          <StreamingBubble key={`stream-${sessionId}`} text={text} agentType={activeAgentType} />
        ) : null
      )}

      <div ref={bottomRef} />
      </div>
    </div>
  );
}
