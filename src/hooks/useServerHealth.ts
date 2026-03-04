import { useState, useEffect, useCallback } from "react";
import { getServerStatus, retryServerDiscovery } from "@/tauri/commands";

export type ConnectionStatus = "loading" | "connected" | "error";

export interface ServerHealthState {
  status: ConnectionStatus;
  port: number | null;
  error: string | null;
  retry: () => Promise<void>;
  isRetrying: boolean;
}

const POLL_INTERVAL_MS = 2000;

export function useServerHealth(): ServerHealthState {
  const [status, setStatus] = useState<ConnectionStatus>("loading");
  const [port, setPort] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [isRetrying, setIsRetrying] = useState(false);

  const checkStatus = useCallback(async () => {
    try {
      const serverStatus = await getServerStatus();

      if (serverStatus.is_healthy && serverStatus.port) {
        setPort(serverStatus.port);
        setStatus("connected");
        setError(null);
        return true;
      }

      if (serverStatus.has_error) {
        setStatus("error");
        setError(serverStatus.error_message || "Failed to connect to server");
        return false;
      }

      return false;
    } catch (err) {
      setStatus("error");
      setError(err instanceof Error ? err.message : "Unknown error");
      return false;
    }
  }, []);

  const retry = useCallback(async () => {
    setIsRetrying(true);
    try {
      setStatus("loading");
      setError(null);
      await retryServerDiscovery();
      // Wait a moment then check status
      await new Promise(resolve => setTimeout(resolve, 1000));
      await checkStatus();
    } catch (err) {
      setStatus("error");
      setError(err instanceof Error ? err.message : "Failed to retry connection");
    } finally {
      setIsRetrying(false);
    }
  }, [checkStatus]);

  useEffect(() => {
    // Initial check
    checkStatus();

    // Poll while not connected
    const intervalId = setInterval(() => {
      if (status !== "connected") {
        checkStatus();
      }
    }, POLL_INTERVAL_MS);

    return () => {
      clearInterval(intervalId);
    };
  }, [checkStatus, status]);

  return { status, port, error, retry, isRetrying };
}
