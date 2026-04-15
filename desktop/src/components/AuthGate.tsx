import { useAuthStore } from "@/stores/authStore";
import { type ReactNode, useEffect } from "react";
import { LoadingScreen } from "@/components/LoadingScreen";

/**
 * Auth gate for the web client.
 *
 * The Electron GitHub device-code flow was removed in the de-electron
 * migration. The server exposes its own OAuth callback on :1455, but the
 * HTTP auth endpoints the UI needs are not yet in place, so `fetchState`
 * currently resolves to an anonymous authenticated user. When server
 * auth lands, re-introduce the sign-in UI here.
 */
export function AuthGate({ children }: { children: ReactNode }) {
  const { isAuthenticated, isLoading, fetchState } = useAuthStore();

  useEffect(() => {
    void fetchState();
  }, [fetchState]);

  if (isLoading) {
    return <LoadingScreen message="Checking authentication..." />;
  }

  if (!isAuthenticated) {
    return <LoadingScreen message="Signing you in..." />;
  }

  return <>{children}</>;
}
