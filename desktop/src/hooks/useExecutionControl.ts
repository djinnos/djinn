import { useState, useCallback } from "react";
import { callMcpTool } from "@/api/mcpClient";

interface ExecutionControl {
  busy: boolean;
  start: (projectPath?: string | null) => Promise<void>;
  pause: (projectPath?: string | null) => Promise<void>;
  resume: (projectPath?: string | null) => Promise<void>;
  killTask: (taskId: string) => Promise<void>;
}

export function useExecutionControl(onComplete?: () => void): ExecutionControl {
  const [busy, setBusy] = useState(false);

  const wrap = useCallback(
    async (fn: () => Promise<unknown>) => {
      setBusy(true);
      try {
        await fn();
        onComplete?.();
      } finally {
        setBusy(false);
      }
    },
    [onComplete]
  );

  const start = useCallback(
    (projectPath?: string | null) =>
      wrap(() => callMcpTool("execution_start", { project: projectPath ?? undefined })),
    [wrap]
  );

  const pause = useCallback(
    (projectPath?: string | null) =>
      wrap(() => callMcpTool("execution_pause", { project: projectPath ?? undefined })),
    [wrap]
  );

  const resume = useCallback(
    (projectPath?: string | null) =>
      wrap(() => callMcpTool("execution_resume", { project: projectPath ?? undefined })),
    [wrap]
  );

  const killTask = useCallback(
    (taskId: string) =>
      wrap(() => callMcpTool("execution_kill_task", { task_id: taskId })),
    [wrap]
  );

  return { busy, start, pause, resume, killTask };
}
