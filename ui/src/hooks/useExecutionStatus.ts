import { useEffect, useMemo } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { callMcpTool } from "@/api/mcpClient";
import { sseStore, type SSEEventType } from "@/stores/sseStore";

export interface CapacityEntry {
  active: number;
  max: number;
}

interface ExecutionStatusState {
  state: string | null;
  runningSessions: number;
  maxSessions: number;
  capacity: Record<string, CapacityEntry>;
  raw: Record<string, unknown> | null;
  refresh: () => void;
}

interface ExecutionStatusData {
  state: string | null;
  runningSessions: number;
  maxSessions: number;
  capacity: Record<string, CapacityEntry>;
  raw: Record<string, unknown> | null;
}

const EMPTY_STATE: ExecutionStatusData = {
  state: null,
  runningSessions: 0,
  maxSessions: 0,
  capacity: {},
  raw: null,
};

// SSE events that can change execution state: session count/capacity moves
// when sessions dispatch/start/end, and scheduling can react to task CRUD or
// a project change.
const INVALIDATING_EVENTS: SSEEventType[] = [
  "session_dispatched",
  "session_started",
  "session_ended",
  "task_created",
  "task_updated",
  "task_deleted",
  "project_changed",
];

export function useExecutionStatus(projectPath?: string | null): ExecutionStatusState {
  const queryClient = useQueryClient();
  const queryKey = useMemo(
    () => ["execution_status", projectPath ?? null] as const,
    [projectPath],
  );

  const query = useQuery<ExecutionStatusData>({
    queryKey,
    queryFn: async () => {
      try {
        const result = await callMcpTool("execution_status", {
          project: projectPath ?? undefined,
        });
        return {
          state: result.state ?? null,
          runningSessions: result.running_sessions ?? 0,
          maxSessions: result.max_sessions ?? 0,
          capacity: (result.capacity ?? {}) as Record<string, CapacityEntry>,
          raw: result as unknown as Record<string, unknown>,
        };
      } catch {
        return EMPTY_STATE;
      }
    },
    // 60s safety net — real freshness comes from SSE-driven invalidations.
    refetchInterval: 60_000,
    staleTime: 30_000,
  });

  useEffect(() => {
    const subscribe = sseStore.getState().subscribe;
    const invalidate = () => {
      void queryClient.invalidateQueries({ queryKey });
    };
    const unsubs = INVALIDATING_EVENTS.map((eventType) =>
      subscribe(eventType, invalidate),
    );
    return () => {
      for (const unsub of unsubs) unsub();
    };
  }, [queryClient, queryKey]);

  const data = query.data ?? EMPTY_STATE;

  return {
    state: data.state,
    runningSessions: data.runningSessions,
    maxSessions: data.maxSessions,
    capacity: data.capacity,
    raw: data.raw,
    refresh: () => {
      void query.refetch();
    },
  };
}
