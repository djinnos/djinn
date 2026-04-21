import { useState, useCallback } from "react";
import { callMcpTool } from "@/api/mcpClient";

interface ExecutionControl {
  busy: boolean;
  killTask: (taskId: string) => Promise<void>;
}

export function useExecutionControl(onComplete?: () => void): ExecutionControl {
  const [busy, setBusy] = useState(false);

  const killTask = useCallback(
    async (taskId: string) => {
      setBusy(true);
      try {
        await callMcpTool("execution_kill_task", { task_id: taskId });
        onComplete?.();
      } finally {
        setBusy(false);
      }
    },
    [onComplete]
  );

  return { busy, killTask };
}
