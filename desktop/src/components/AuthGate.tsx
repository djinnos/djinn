import { useAuthStore } from "@/stores/authStore";
import { exchangeAuthCode, getOAuthConfig, type AuthUser } from "@/tauri/commands";
import { listen } from "@tauri-apps/api/event";
import { type ReactNode, useEffect } from "react";
import { Button } from "@/components/ui/button";

export function AuthGate({ children }: { children: ReactNode }) {
  const { isAuthenticated, isLoading, error, fetchState, setState, login } =
    useAuthStore();

  useEffect(() => {
    const listeners: Array<() => void> = [];

    // Don't call fetchState() eagerly — the backend drives initial state via
    // events (auth:state-changed, auth:login-required, auth:silent-refresh-failed).
    // This avoids the race where fetchState() returns { isAuthenticated: false }
    // before the silent refresh has completed.

    listen<{ isAuthenticated: boolean; user: AuthUser | null }>("auth:state-changed", (event) => {
      const state = event.payload;
      setState(state);
      if (state.isAuthenticated && state.user?.email) {
        const { email, sub } = state.user;
        import("@/api/server").then(({ setUserIdentity }) =>
          setUserIdentity(email, sub),
        );
      }
    }).then((u) => listeners.push(u));

    listen<{ code: string; state: string; code_verifier: string }>(
      "auth:callback-received",
      async (event) => {
        const { code, code_verifier } = event.payload;
        try {
          const { redirectUri, clientId } = await getOAuthConfig();
          await exchangeAuthCode(
            code,
            code_verifier,
            redirectUri,
            clientId,
          );
        } catch (e) {
          console.error("Token exchange failed:", e);
          useAuthStore.setState({ error: `Authentication failed: ${e}` });
        }
      },
    ).then((u) => listeners.push(u));

    listen("auth:silent-refresh-success", () => {
      fetchState();
    }).then((u) => listeners.push(u));

    listen("auth:login-required", () => {
      useAuthStore.setState({ isLoading: false, isAuthenticated: false });
    }).then((u) => listeners.push(u));

    listen("auth:silent-refresh-failed", () => {
      useAuthStore.setState({ isLoading: false, isAuthenticated: false });
    }).then((u) => listeners.push(u));

    // Fallback: if no backend event arrives within 5s (e.g. event fired before
    // listeners registered), poll the backend directly.
    const fallbackTimer = setTimeout(() => {
      if (useAuthStore.getState().isLoading) {
        fetchState();
      }
    }, 5000);

    return () => {
      clearTimeout(fallbackTimer);
      listeners.forEach((unlisten) => unlisten());
    };
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  if (isLoading) {
    return (
      <main className="flex min-h-screen items-center justify-center bg-background text-foreground">
        <p className="text-sm text-muted-foreground">Checking authentication...</p>
      </main>
    );
  }

  if (!isAuthenticated) {
    return (
      <main className="flex min-h-screen items-center justify-center bg-background text-foreground">
        <div className="flex w-full max-w-md flex-col items-center gap-4 rounded-lg p-8 text-center">
          <h1 className="text-2xl font-semibold">Sign in required</h1>
          <p className="text-sm text-muted-foreground">
            {error || "Please sign in to continue to Djinn."}
          </p>
          <Button onClick={() => login()}>Sign in</Button>
        </div>
      </main>
    );
  }

  return <>{children}</>;
}
