import { useState, useEffect, useCallback } from "react";
import { callMcpTool } from "@/api/mcpClient";

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

export function useExecutionStatus(projectPath?: string | null): ExecutionStatusState {
  const [state, setState] = useState<string | null>(null);
  const [runningSessions, setRunningSessions] = useState(0);
  const [maxSessions, setMaxSessions] = useState(0);
  const [capacity, setCapacity] = useState<Record<string, CapacityEntry>>({});
  const [raw, setRaw] = useState<Record<string, unknown> | null>(null);

  const refresh = useCallback(() => {
    callMcpTool("execution_status", { project: projectPath ?? undefined })
      .then((result) => {
        setState(result.state ?? null);
        setRunningSessions(result.running_sessions ?? 0);
        setMaxSessions(result.max_sessions ?? 0);
        setCapacity((result.capacity ?? {}) as Record<string, CapacityEntry>);
        setRaw(result as unknown as Record<string, unknown>);
      })
      .catch(() => {
        setState(null);
        setRunningSessions(0);
        setMaxSessions(0);
        setCapacity({});
        setRaw(null);
      });
  }, [projectPath]);

  useEffect(() => {
    refresh();
    const interval = setInterval(refresh, 5000);
    return () => clearInterval(interval);
  }, [refresh]);

  return { state, runningSessions, maxSessions, capacity, raw, refresh };
}
