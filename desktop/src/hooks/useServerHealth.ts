import { useState, useEffect, useCallback } from "react";
import { checkServerAvailable } from "@/electron/commands";

export type ConnectionStatus = "loading" | "connected" | "error";

export interface ServerHealthState {
  status: ConnectionStatus;
  baseUrl: string | null;
  error: string | null;
  retry: () => Promise<void>;
  isRetrying: boolean;
}

const POLL_INTERVAL_MS = 3000;

/**
 * Poll the server /health endpoint. When the server runs via docker-compose
 * on localhost:8372 this should be healthy; when it's not running we show a
 * banner instructing the user to run `docker compose up`.
 */
export function useServerHealth(): ServerHealthState {
  const [status, setStatus] = useState<ConnectionStatus>("loading");
  const [baseUrl, setBaseUrl] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [isRetrying, setIsRetrying] = useState(false);

  const checkStatus = useCallback(async () => {
    try {
      const result = await checkServerAvailable();
      setBaseUrl(result.baseUrl);
      if (result.ok) {
        setStatus("connected");
        setError(null);
        return true;
      }
      setStatus("error");
      setError(result.error ?? `Server not reachable at ${result.baseUrl}`);
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
      await checkStatus();
    } finally {
      setIsRetrying(false);
    }
  }, [checkStatus]);

  useEffect(() => {
    void checkStatus();

    const intervalId = setInterval(() => {
      void checkStatus();
    }, POLL_INTERVAL_MS);

    return () => {
      clearInterval(intervalId);
    };
  }, [checkStatus]);

  return { status, baseUrl, error, retry, isRetrying };
}
