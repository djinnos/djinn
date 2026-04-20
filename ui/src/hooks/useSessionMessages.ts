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
import { verificationStore } from "@/stores/verificationStore";
import type { StepEntry } from "@/stores/verificationStore";

// ── Types ────────────────────────────────────────────────────────────────────

export interface ContentBlock {
  type: string;
  text?: string;
  thinking?: string;
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
  exitCode?: number;
  command?: string;
  timestamp: string;
}

export interface CommentBlock {
  kind: "comment";
  body: string;
  actorRole: string;
  timestamp: string;
}

export interface VerificationStep {
  command: string;
  phase: "setup" | "verification";
  passed: boolean;
  exitCode?: number;
  stdout?: string;
  stderr?: string;
  durationMs?: number;
}

export interface VerificationBlock {
  kind: "verification";
  steps: VerificationStep[];
  passed: boolean;
  totalDurationMs: number;
  timestamp: string;
}

export interface StreamingDelta {
  kind: "streaming";
  sessionId: string;
  agentType: string;
  text: string;
}

export type TimelineEntry = ChatMessage | SystemDivider | CommandBlock | CommentBlock | VerificationBlock | StreamingDelta;

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
  const [streamingThinking, setStreamingThinking] = useState<Map<string, string>>(new Map());

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
      const pendingCommandRuns: Array<{ steps: VerificationStep[]; timestamp: string }> = [];

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
            // Always emit the status transition divider
            entries.push({
              kind: "divider",
              label: `${formatStatus(from)} → ${formatStatus(to)}`,
              timestamp: entry.timestamp,
            });

            // If there's a reason, parse it as a command result
            if (reason) {
              const setupFailMatch = reason.match(/^(Setup|Verification) command '([^']+)' failed(?: \(exit (\d+)\))?\s*(.*)/s);
              if (setupFailMatch) {
                const [, phase, command, exitCodeStr, body] = setupFailMatch;
                entries.push({
                  kind: "command",
                  name: phase?.toLowerCase() ?? "setup",
                  body: body?.trim() ?? "",
                  passed: false,
                  exitCode: exitCodeStr ? Number(exitCodeStr) : undefined,
                  command,
                  timestamp: entry.timestamp,
                });
              }
              // Generic reasons (e.g. review rejections) are already visible
              // in the agent's submit_review / submit_decision cards — skip them.
            }
          }
        } else if (entry.event_type === "commands_run") {
          const payload = entry.payload as Record<string, unknown>;
          const phase = (payload?.phase as string) ?? "verification";
          const commands = payload?.commands as Array<Record<string, unknown>> | undefined;

          if (commands?.length) {
            const steps: VerificationStep[] = commands.map((cmd) => {
              const name = (cmd.name as string) || (cmd.command as string) || "unknown";
              const exitCode = cmd.exit_code as number | undefined;
              return {
                command: name,
                phase: phase as "setup" | "verification",
                passed: exitCode === 0,
                exitCode: exitCode ?? undefined,
                stdout: (cmd.stdout as string) || undefined,
                stderr: (cmd.stderr as string) || undefined,
                durationMs: cmd.duration_ms as number | undefined,
              };
            });
            pendingCommandRuns.push({ steps, timestamp: entry.timestamp });
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
              kind: "comment",
              body,
              actorRole: (entry as Record<string, unknown>).actor_role as string ?? "system",
              timestamp: entry.timestamp,
            });
          }
        }
      }

      // Group commands_run events into verification blocks.
      // A setup event followed by a verification event (within 60s) = one cycle.
      // Consecutive events with the same phase = separate cycles.
      if (pendingCommandRuns.length > 0) {
        pendingCommandRuns.sort((a, b) => a.timestamp.localeCompare(b.timestamp));

        let i = 0;
        while (i < pendingCommandRuns.length) {
          const current = pendingCommandRuns[i];
          const currentPhases = new Set(current.steps.map((s) => s.phase));

          // Check if next event is a different phase within 60s (same cycle)
          const next = pendingCommandRuns[i + 1];
          const nextPhases = next ? new Set(next.steps.map((s) => s.phase)) : null;
          const timeDiff = next
            ? new Date(next.timestamp).getTime() - new Date(current.timestamp).getTime()
            : Infinity;

          const shouldMerge = next
            && timeDiff <= 60_000
            && !setsOverlap(currentPhases, nextPhases!);

          if (shouldMerge) {
            const allSteps = [...current.steps, ...next!.steps];
            const totalDuration = allSteps.reduce((sum, s) => sum + (s.durationMs ?? 0), 0);
            entries.push({
              kind: "verification",
              steps: allSteps,
              passed: allSteps.every((s) => s.passed),
              totalDurationMs: totalDuration,
              timestamp: current.timestamp,
            });
            i += 2;
          } else {
            const totalDuration = current.steps.reduce((sum, s) => sum + (s.durationMs ?? 0), 0);
            entries.push({
              kind: "verification",
              steps: current.steps,
              passed: current.steps.every((s) => s.passed),
              totalDurationMs: totalDuration,
              timestamp: current.timestamp,
            });
            i += 1;
          }
        }
      }

      // Seed verification store from persisted results
      if (taskId && result.verification_steps?.length) {
        const store = verificationStore.getState();
        const key = taskId;
        // Only seed if the store doesn't already have data (SSE may have populated it)
        if (!store.runs.has(key)) {
          const steps: StepEntry[] = result.verification_steps.map((s: { name: string; command?: string; phase: string; exit_code: number; duration_ms: number; stdout?: string; stderr?: string }, i: number) => ({
            index: i,
            name: s.name,
            command: s.command || undefined,
            phase: s.phase as "setup" | "verification",
            status: (s.exit_code === 0 ? "passed" : "failed") as StepEntry["status"],
            exitCode: s.exit_code,
            durationMs: s.duration_ms,
            stdout: s.stdout || undefined,
            stderr: s.stderr || undefined,
          }));
          for (const step of steps) {
            store.addStep(key, step, {
              projectId: "",
              taskId,
            });
          }
          const allPassed = steps.every((s) => s.status === "passed");
          store.setRunStatus(key, allPassed ? "passed" : "failed");
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
      } else if (msg.type === "thinking_delta") {
        setStreamingThinking((prev) => {
          const next = new Map(prev);
          const current = next.get(data.session_id as string) ?? "";
          next.set(data.session_id as string, current + ((msg.text as string) ?? ""));
          return next;
        });
      } else {
        // Full message — clear streaming state and append to timeline.
        setStreamingText((prev) => {
          const next = new Map(prev);
          next.delete(data.session_id as string);
          return next;
        });
        setStreamingThinking((prev) => {
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

  return { timeline, sessions, loading, error, streamingText, streamingThinking, refetch: fetchData };
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

function setsOverlap(a: Set<string>, b: Set<string>): boolean {
  for (const v of a) if (b.has(v)) return true;
  return false;
}

function formatStatus(status: string): string {
  return STATUS_LABELS[status] ?? status;
}
