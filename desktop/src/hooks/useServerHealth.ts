import { useState, useEffect, useCallback } from "react";
import { getServerBaseUrl } from "@/api/serverUrl";

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
const REQUEST_TIMEOUT_MS = 3000;

async function pingServer(): Promise<{ version: string | null }> {
  const controller = new AbortController();
  const timer = window.setTimeout(() => controller.abort(), REQUEST_TIMEOUT_MS);
  try {
    const res = await fetch(`${getServerBaseUrl()}/health`, { signal: controller.signal });
    if (!res.ok) {
      throw new Error(`Health check failed: ${res.status}`);
    }
    const json = (await res.json().catch(() => ({}))) as { version?: string };
    return { version: json?.version ?? null };
  } finally {
    window.clearTimeout(timer);
  }
}

export function useServerHealth(): ServerHealthState {
  const [status, setStatus] = useState<ConnectionStatus>("loading");
  const [port] = useState<number | null>(() => {
    const url = new URL(getServerBaseUrl());
    return Number(url.port) || null;
  });
  const [error, setError] = useState<string | null>(null);
  const [isRetrying, setIsRetrying] = useState(false);
  const [serverVersion, setServerVersion] = useState<string | null>(null);

  const checkStatus = useCallback(async () => {
    try {
      const result = await pingServer();
      setStatus("connected");
      setError(null);
      setServerVersion(result.version);
      return true;
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
      await checkStatus();
    } finally {
      setIsRetrying(false);
    }
  }, [checkStatus]);

  useEffect(() => {
    void checkStatus();
    const intervalId = window.setInterval(() => {
      if (status !== "connected") {
        void checkStatus();
      }
    }, POLL_INTERVAL_MS);
    return () => window.clearInterval(intervalId);
  }, [checkStatus, status]);

  return {
    status,
    port,
    error,
    retry,
    isRetrying,
    serverVersion,
    // Update detection required the Electron host; not applicable in the web client.
    updateAvailable: false,
  };
}
