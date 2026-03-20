/**
 * useSessionMessages — fetches historical messages and subscribes to live SSE deltas.
 *
 * Returns a merged timeline of:
 *  - Session messages from the DB (via task_timeline MCP tool)
 *  - Activity log entries (status changes, verification, comments)
 *  - Live streaming deltas from SSE (session.message events)
 */

import { useCallback, useEffect, useState } from "react";
import { callMcpTool } from "@/api/mcpClient";
import { sseStore } from "@/stores/sseStore";

// ── Types ────────────────────────────────────────────────────────────────────

export interface ContentBlock {
  type: string;
  text?: string;
  name?: string;
  input?: Record<string, unknown>;
  tool_use_id?: string;
  content?: unknown;
  [k: string]: unknown;
}

export interface ChatMessage {
  kind: "message";
  role: "system" | "user" | "assistant";
  content: ContentBlock[];
  sessionId: string;
  agentType: string;
  modelId: string;
  timestamp?: string;
}

export interface SystemDivider {
  kind: "divider";
  label: string;
  timestamp: string;
}

export interface CommandBlock {
  kind: "command";
  name: string;
  body: string;
  passed: boolean;
  timestamp: string;
}

export interface StreamingDelta {
  kind: "streaming";
  sessionId: string;
  agentType: string;
  text: string;
}

export type TimelineEntry = ChatMessage | SystemDivider | CommandBlock | StreamingDelta;

export interface SessionInfo {
  id: string;
  agentType: string;
  modelId: string;
  startedAt: string;
  endedAt?: string;
  status: string;
  tokensIn: number;
  tokensOut: number;
}

// ── Hook ─────────────────────────────────────────────────────────────────────

export function useSessionMessages(taskId: string | null, projectPath: string | null) {
  const [timeline, setTimeline] = useState<TimelineEntry[]>([]);
  const [sessions, setSessions] = useState<SessionInfo[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [streamingText, setStreamingText] = useState<Map<string, string>>(new Map());

  const fetchData = useCallback(async () => {
    if (!taskId || !projectPath) return;

    setLoading(true);
    setError(null);

    try {
      // Single MCP call fetches sessions + messages + activity
      const result = await callMcpTool("task_timeline", {
        task_id: taskId,
        project: projectPath,
      });

      if (result.error) {
        setError(result.error);
        setLoading(false);
        return;
      }

      const sessionList = result.sessions ?? [];
      setSessions(
        sessionList.map((s) => ({
          id: s.id,
          agentType: s.agent_type,
          modelId: s.model_id,
          startedAt: s.started_at,
          endedAt: s.ended_at ?? undefined,
          status: s.status,
          tokensIn: s.tokens_in,
          tokensOut: s.tokens_out,
        }))
      );

      // Build timeline entries
      const entries: TimelineEntry[] = [];

      // Add session messages (already sorted by timestamp from server)
      for (const msg of result.messages ?? []) {
        entries.push({
          kind: "message",
          role: msg.role as "user" | "assistant",
          content: msg.content as ContentBlock[],
          sessionId: msg.session_id,
          agentType: msg.agent_type,
          modelId: msg.model_id,
          timestamp: msg.timestamp,
        });
      }

      // Add activity log entries as dividers and commands
      for (const entry of result.activity ?? []) {
        if (entry.event_type === "status_changed") {
          const payload = entry.payload as Record<string, unknown>;
          const from = payload?.from_status as string | undefined;
          const to = payload?.to_status as string | undefined;
          const reason = payload?.reason as string | undefined;
          if (from && to) {
            let label = `${formatStatus(from)} → ${formatStatus(to)}`;
            if (reason) label += ` — ${reason}`;
            entries.push({
              kind: "divider",
              label,
              timestamp: entry.timestamp,
            });
          }
        } else if (entry.event_type === "verification" || entry.event_type === "setup") {
          const body = ((entry.payload as Record<string, unknown>)?.body as string) ?? "";
          entries.push({
            kind: "command",
            name: entry.event_type,
            body,
            passed: !body,
            timestamp: entry.timestamp,
          });
        } else if (entry.event_type === "comment") {
          const body = ((entry.payload as Record<string, unknown>)?.body as string) ?? "";
          if (body) {
            entries.push({
              kind: "divider",
              label: body,
              timestamp: entry.timestamp,
            });
          }
        }
      }

      // Sort by timestamp
      entries.sort((a, b) => {
        const tsA = "timestamp" in a ? a.timestamp ?? "" : "";
        const tsB = "timestamp" in b ? b.timestamp ?? "" : "";
        return tsA.localeCompare(tsB);
      });

      setTimeline(entries);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [taskId, projectPath]);

  // Subscribe to live SSE session_message events for this task via sseStore
  useEffect(() => {
    if (!taskId) return;

    const unsub = sseStore.getState().subscribe("session_message", (event) => {
      const envelope = event.data as Record<string, unknown>;
      const data = (envelope.data ?? envelope) as Record<string, unknown>;
      if (data.task_id !== taskId) return;

      const msg = data.message as Record<string, unknown> | undefined;
      if (!msg) return;

      if (msg.type === "delta") {
        setStreamingText((prev) => {
          const next = new Map(prev);
          const current = next.get(data.session_id as string) ?? "";
          next.set(data.session_id as string, current + ((msg.text as string) ?? ""));
          return next;
        });
      } else {
        setStreamingText((prev) => {
          const next = new Map(prev);
          next.delete(data.session_id as string);
          return next;
        });

        const content = (msg.content as ContentBlock[]) ?? [{ type: "text", text: (msg.text as string) ?? "" }];
        setTimeline((prev) => [
          ...prev,
          {
            kind: "message" as const,
            role: (msg.role as "assistant") ?? "assistant",
            content,
            sessionId: data.session_id as string,
            agentType: data.agent_type as string,
            modelId: "",
            timestamp: new Date().toISOString(),
          },
        ]);
      }
    });

    return unsub;
  }, [taskId]);

  // Initial fetch
  useEffect(() => {
    fetchData();
  }, [fetchData]);

  return { timeline, sessions, loading, error, streamingText, refetch: fetchData };
}

// ── Helpers ──────────────────────────────────────────────────────────────────

const STATUS_LABELS: Record<string, string> = {
  open: "Open",
  in_progress: "Coding",
  verifying: "Verifying",
  needs_task_review: "Review",
  in_task_review: "Reviewing",
  needs_lead_intervention: "Lead Intervention",
  in_lead_intervention: "Lead Intervening",
  closed: "Done",
};

function formatStatus(status: string): string {
  return STATUS_LABELS[status] ?? status;
}
