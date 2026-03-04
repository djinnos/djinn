import { useState, useEffect, useCallback } from "react";
import { getServerPort } from "@/tauri/commands";

export type ConnectionStatus = "loading" | "connected" | "error";

export interface ServerHealthState {
  status: ConnectionStatus;
  port: number | null;
  error: string | null;
}

const POLL_INTERVAL_MS = 1000;
const MAX_RETRIES = 30;

export function useServerHealth(): ServerHealthState {
  const [status, setStatus] = useState<ConnectionStatus>("loading");
  const [port, setPort] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);

  const checkHealth = useCallback(async () => {
    try {
      const serverPort = await getServerPort();
      
      if (serverPort > 0) {
        setPort(serverPort);
        setStatus("connected");
        setError(null);
        return true;
      }
      
      return false;
    } catch (err) {
      setError(err instanceof Error ? err.message : "Unknown error");
      return false;
    }
  }, []);

  useEffect(() => {
    let retries = 0;
    let intervalId: ReturnType<typeof setInterval> | null = null;

    const poll = async () => {
      const isHealthy = await checkHealth();
      
      if (isHealthy) {
        if (intervalId) {
          clearInterval(intervalId);
          intervalId = null;
        }
        return;
      }

      retries++;
      if (retries >= MAX_RETRIES) {
        setStatus("error");
        setError("Failed to connect to server after maximum retries");
        if (intervalId) {
          clearInterval(intervalId);
          intervalId = null;
        }
      }
    };

    poll();

    intervalId = setInterval(poll, POLL_INTERVAL_MS);

    return () => {
      if (intervalId) {
        clearInterval(intervalId);
      }
    };
  }, [checkHealth]);

  return { status, port, error };
}
