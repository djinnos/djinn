import { useState, useEffect, useCallback } from "react";
import { getServerStatus, retryServerConnection } from "@/tauri/commands";
import { listen } from "@tauri-apps/api/event";

export type ConnectionStatus = "loading" | "connected" | "error";

export interface ServerHealthState {
  status: ConnectionStatus;
  port: number | null;
  error: string | null;
  retry: () => Promise<void>;
  isRetrying: boolean;
  serverVersion: string | null;
  updateAvailable: boolean;
}

const POLL_INTERVAL_MS = 2000;

export function useServerHealth(): ServerHealthState {
  const [status, setStatus] = useState<ConnectionStatus>("loading");
  const [port, setPort] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [isRetrying, setIsRetrying] = useState(false);
  const [serverVersion, setServerVersion] = useState<string | null>(null);
  const [updateAvailable, setUpdateAvailable] = useState(false);

  const checkStatus = useCallback(async () => {
    try {
      const serverStatus = await getServerStatus();

      if (serverStatus.is_healthy && serverStatus.port) {
        setPort(serverStatus.port);
        setStatus("connected");
        setError(null);
        setServerVersion(serverStatus.server_version);
        setUpdateAvailable(serverStatus.update_available);
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
      await retryServerConnection();
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

    // Listen for backend health monitor events
    const unlistenReconnected = listen<number>("server:reconnected", (event) => {
      setPort(event.payload);
      setStatus("connected");
      setError(null);
    });

    const unlistenDisconnected = listen("server:disconnected", () => {
      setStatus("error");
      setError("Server connection lost. Attempting to reconnect...");
    });

    return () => {
      clearInterval(intervalId);
      unlistenReconnected.then((fn) => fn());
      unlistenDisconnected.then((fn) => fn());
    };
  }, [checkStatus, status]);

  return { status, port, error, retry, isRetrying, serverVersion, updateAvailable };
}
