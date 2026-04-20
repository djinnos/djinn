/**
 * ConnectionStatus component - Visual indicator for SSE connection state
 *
 * Features:
 * - Green dot: connected
 * - Orange pulsing dot: reconnecting
 * - Red dot: error
 * - Shows tooltip with status description on hover
 */

import { useSSEStatus } from "../hooks/useSSEStatus";

export function ConnectionStatus() {
  const { status, reconnectAttempt } = useSSEStatus();

  const getStatusConfig = () => {
    switch (status) {
      case "connected":
        return {
          color: "bg-green-500",
          pulse: false,
          label: "Connected",
          description: "Live updates active",
        };
      case "reconnecting":
        return {
          color: "bg-orange-500",
          pulse: true,
          label: "Reconnecting",
          description: `Attempt ${reconnectAttempt + 1}`,
        };
      case "error":
        return {
          color: "bg-red-500",
          pulse: false,
          label: "Connection Error",
          description: "Failed to connect after max retries",
        };
      default:
        return {
          color: "bg-gray-400",
          pulse: false,
          label: "Unknown",
          description: "Status unknown",
        };
    }
  };

  const config = getStatusConfig();

  return (
    <div
      className="flex items-center gap-2 rounded-md px-2 py-1 hover:bg-muted/50 transition-colors cursor-help"
      title={`${config.label}: ${config.description}`}
    >
      <div className="relative flex h-3 w-3">
        {config.pulse && (
          <span
            className={`absolute inline-flex h-full w-full rounded-full ${config.color} opacity-75 animate-ping`}
          />
        )}
        <span
          className={`relative inline-flex h-3 w-3 rounded-full ${config.color}`}
        />
      </div>
      <span className="text-xs font-medium text-muted-foreground">
        {config.label}
      </span>
    </div>
  );
}
