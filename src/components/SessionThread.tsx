/**
 * SessionThread — unified chat view for task execution.
 *
 * Renders three visual block types inline:
 * 1. Agent bubbles (worker, reviewer, lead) with tool calls
 * 2. Command blocks (setup/verification results)
 * 3. System dividers (status transitions)
 */

import { useEffect, useRef, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { cn } from "@/lib/utils";
import type {
  TimelineEntry,
  ChatMessage,
  SystemDivider,
  CommandBlock,
  ContentBlock,
} from "@/hooks/useSessionMessages";

// ── Agent identity ───────────────────────────────────────────────────────────

const AGENT_CONFIG: Record<string, { label: string; color: string; border: string }> = {
  worker: { label: "Worker", color: "text-blue-400", border: "border-blue-500/30" },
  task_reviewer: { label: "Reviewer", color: "text-amber-400", border: "border-amber-500/30" },
  pm: { label: "Lead", color: "text-purple-400", border: "border-purple-500/30" },
  epic_reviewer: { label: "Epic Reviewer", color: "text-teal-400", border: "border-teal-500/30" },
};

function agentConfig(agentType: string) {
  return AGENT_CONFIG[agentType] ?? { label: agentType, color: "text-muted-foreground", border: "border-border" };
}

// ── Tool call component ──────────────────────────────────────────────────────

function ToolCallBlock({ block }: { block: ContentBlock }) {
  const [expanded, setExpanded] = useState(false);
  const name = block.name ?? "tool_call";
  const input = block.input;

  return (
    <div className="my-1">
      <button
        type="button"
        className="flex items-center gap-1.5 rounded bg-muted/50 px-2 py-1 text-xs text-muted-foreground transition-colors hover:bg-muted"
        onClick={() => setExpanded(!expanded)}
      >
        <span className="font-mono">{expanded ? "▾" : "▸"}</span>
        <span className="font-mono font-medium">{name}</span>
      </button>
      {expanded && input && (
        <pre className="mt-1 max-h-60 overflow-auto rounded bg-background/50 p-2 text-xs text-muted-foreground">
          {typeof input === "string" ? input : JSON.stringify(input, null, 2)}
        </pre>
      )}
    </div>
  );
}

// ── Tool result component ────────────────────────────────────────────────────

function ToolResultBlock({ block }: { block: ContentBlock }) {
  const [expanded, setExpanded] = useState(false);
  const content = block.content;
  const preview = typeof content === "string"
    ? content.slice(0, 80)
    : JSON.stringify(content)?.slice(0, 80) ?? "";

  return (
    <div className="my-1">
      <button
        type="button"
        className="flex items-center gap-1.5 rounded bg-muted/30 px-2 py-1 text-xs text-muted-foreground transition-colors hover:bg-muted/50"
        onClick={() => setExpanded(!expanded)}
      >
        <span className="font-mono">{expanded ? "▾" : "▸"}</span>
        <span className="truncate font-mono opacity-70">{preview}…</span>
      </button>
      {expanded && (
        <pre className="mt-1 max-h-60 overflow-auto rounded bg-background/50 p-2 text-xs text-muted-foreground">
          {typeof content === "string" ? content : JSON.stringify(content, null, 2)}
        </pre>
      )}
    </div>
  );
}

// ── Message bubble ───────────────────────────────────────────────────────────

function MessageBubble({ entry }: { entry: ChatMessage }) {
  const config = agentConfig(entry.agentType);
  const isUser = entry.role === "user";

  return (
    <div className={cn("flex gap-3 py-2", isUser ? "flex-row-reverse" : "flex-row")}>
      <div
        className={cn(
          "max-w-[85%] rounded-lg border px-3 py-2",
          isUser
            ? "border-border bg-muted/30"
            : `${config.border} bg-card`
        )}
      >
        {/* Agent label */}
        {!isUser && (
          <div className={cn("mb-1 text-[10px] font-semibold uppercase tracking-wider", config.color)}>
            {config.label}
          </div>
        )}

        {/* Content blocks */}
        {entry.content.map((block, idx) => {
          if (block.type === "tool_use") {
            return <ToolCallBlock key={idx} block={block} />;
          }
          if (block.type === "tool_result") {
            return <ToolResultBlock key={idx} block={block} />;
          }
          if (block.type === "text" && block.text) {
            return (
              <div key={idx} className="prose prose-sm max-w-none dark:prose-invert prose-p:my-1 prose-pre:my-1">
                <ReactMarkdown remarkPlugins={[remarkGfm]}>{block.text}</ReactMarkdown>
              </div>
            );
          }
          return null;
        })}
      </div>
    </div>
  );
}

// ── System divider ───────────────────────────────────────────────────────────

function DividerLine({ entry }: { entry: SystemDivider }) {
  return (
    <div className="flex items-center gap-3 py-2">
      <div className="h-px flex-1 bg-border" />
      <span className="max-w-md truncate text-center text-[11px] text-muted-foreground">
        {entry.label}
      </span>
      <div className="h-px flex-1 bg-border" />
    </div>
  );
}

// ── Command block ────────────────────────────────────────────────────────────

function CommandRow({ entry }: { entry: CommandBlock }) {
  const [expanded, setExpanded] = useState(!entry.passed);

  return (
    <div className="py-1">
      <button
        type="button"
        className="flex w-full items-center gap-2 rounded bg-muted/30 px-3 py-1.5 text-xs transition-colors hover:bg-muted/50"
        onClick={() => setExpanded(!expanded)}
      >
        <span className="font-mono text-muted-foreground">{expanded ? "▾" : "▸"}</span>
        <span className="font-medium">{entry.name}</span>
        <span className="flex-1" />
        <span className={entry.passed ? "text-emerald-400" : "text-red-400"}>
          {entry.passed ? "passed" : "failed"}
        </span>
      </button>
      {expanded && entry.body && (
        <pre className="mt-1 max-h-40 overflow-auto rounded bg-background/50 p-2 text-xs text-muted-foreground">
          {entry.body}
        </pre>
      )}
    </div>
  );
}

// ── Streaming indicator ──────────────────────────────────────────────────────

function StreamingBubble({ text, agentType }: { text: string; agentType?: string }) {
  const config = agentConfig(agentType ?? "worker");

  return (
    <div className="flex gap-3 py-2">
      <div className={cn("max-w-[85%] rounded-lg border px-3 py-2", config.border, "bg-card")}>
        <div className={cn("mb-1 text-[10px] font-semibold uppercase tracking-wider", config.color)}>
          {config.label}
        </div>
        <div className="prose prose-sm max-w-none dark:prose-invert prose-p:my-1">
          <ReactMarkdown remarkPlugins={[remarkGfm]}>{text}</ReactMarkdown>
          <span className="inline-block h-3 w-1.5 animate-pulse bg-foreground/60" />
        </div>
      </div>
    </div>
  );
}

// ── Main thread component ────────────────────────────────────────────────────

interface SessionThreadProps {
  timeline: TimelineEntry[];
  streamingText: Map<string, string>;
  loading: boolean;
  error: string | null;
  activeAgentType?: string;
}

export function SessionThread({
  timeline,
  streamingText,
  loading,
  error,
  activeAgentType,
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
    <div className="flex flex-1 flex-col overflow-y-auto px-4 py-3">
      {timeline.map((entry, idx) => {
        switch (entry.kind) {
          case "message":
            return <MessageBubble key={idx} entry={entry} />;
          case "divider":
            return <DividerLine key={idx} entry={entry} />;
          case "command":
            return <CommandRow key={idx} entry={entry} />;
          default:
            return null;
        }
      })}

      {/* Streaming text from active sessions */}
      {Array.from(streamingText.entries()).map(([sessionId, text]) =>
        text ? (
          <StreamingBubble key={`stream-${sessionId}`} text={text} agentType={activeAgentType} />
        ) : null
      )}

      <div ref={bottomRef} />
    </div>
  );
}
