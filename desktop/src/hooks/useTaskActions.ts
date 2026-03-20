import { useState, useCallback } from "react";
import { callMcpTool } from "@/api/mcpClient";

export function useTaskActions() {
  const [busy, setBusy] = useState(false);

  const transition = useCallback(
    async (taskId: string, projectPath: string, action: string, reason?: string) => {
      setBusy(true);
      try {
        await callMcpTool("task_transition", {
          project: projectPath,
          id: taskId,
          action,
          ...(reason ? { reason } : {}),
        });
      } finally {
        setBusy(false);
      }
    },
    []
  );

  return { busy, transition };
}
