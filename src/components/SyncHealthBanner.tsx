import { useEffect, useState } from "react";
import { callMcpTool } from "@/api/mcpClient";
import { AlertCircle } from "lucide-react";
import { sseStore } from "@/stores/sseStore";
import type { TaskSyncStatusOutputSchema } from "@/api/generated/mcp-tools.gen";

interface SyncChannelStatus extends TaskSyncStatusOutputSchema.SyncChannelStatus {}

/**
 * Hook to monitor sync health status from the MCP server
 * Returns state for SyncHealthBanner component
 */
function useSyncHealth() {
  const [syncStatus, setSyncStatus] = useState<TaskSyncStatusOutputSchema.TaskSyncStatusOutput | null>(null);
  const [isLoading, setIsLoading] = useState(true);
  const [needsAttention, setNeedsAttention] = useState(false);
  const [errorDetails, setErrorDetails] = useState<string | null>(null);

  // Poll sync status periodically
  useEffect(() => {
    const fetchSyncStatus = async () => {
      try {
        const status = await callMcpTool("task_sync_status", {});
        setSyncStatus(status);
        setIsLoading(false);

        // Check if any channel has failure_count >= 3
        const channels = status.channels || [];
        const needyChannel = channels.find((ch: SyncChannelStatus) => ch.failure_count >= 3);

        if (needyChannel) {
          setNeedsAttention(true);
          setErrorDetails(needyChannel.last_error || "Unknown sync error");
        } else {
          setNeedsAttention(false);
          setErrorDetails(null);
        }
      } catch (err) {
        console.error("Failed to fetch sync status:", err);
        setIsLoading(false);
      }
    };

    // Initial fetch
    fetchSyncStatus();

    // Poll every 5 seconds
    const interval = setInterval(fetchSyncStatus, 5000);

    // Listen to sync_completed SSE events to trigger immediate refresh
    const unsubscribe = sseStore.getState().subscribe("sync_completed", (_event) => {
      fetchSyncStatus();
    });

    return () => {
      clearInterval(interval);
      unsubscribe();
    };
  }, []);

  return { needsAttention, errorDetails, syncStatus, isLoading };
}

/**
 * Banner component that displays when Git sync has multiple failures (failure_count >= 3)
 * Shows error details and suggests checking git remote config
 * Auto-dismisses when sync succeeds and failure_count drops below 3
 */
export function SyncHealthBanner() {
  const { needsAttention, errorDetails, isLoading } = useSyncHealth();

  // Don't render if no attention needed or still loading
  if (!needsAttention || isLoading) {
    return null;
  }

  return (
    <div className="border-l-4 border-red-500 bg-red-50 dark:bg-red-900/20 p-4 mb-4">
      <div className="flex items-start gap-3">
        <AlertCircle className="text-red-600 dark:text-red-400 mt-0.5 flex-shrink-0" size={20} />
        <div className="flex-1">
          <h3 className="font-semibold text-red-900 dark:text-red-100">
            Sync Issues Detected
          </h3>
          <p className="text-sm text-red-800 dark:text-red-200 mt-1">
            Multiple sync failures have occurred. Please check your git remote configuration.
          </p>
          {errorDetails && (
            <div className="mt-2 text-xs text-red-700 dark:text-red-300 font-mono bg-red-100 dark:bg-red-900/40 p-2 rounded">
              {errorDetails}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
