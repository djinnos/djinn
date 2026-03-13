import { useAuthStore } from "@/stores/authStore";
import { exchangeAuthCode, getOAuthConfig, type AuthUser } from "@/tauri/commands";
import { listen } from "@tauri-apps/api/event";
import { type ReactNode, useEffect } from "react";
import { Button } from "@/components/ui/button";

export function AuthGate({ children }: { children: ReactNode }) {
  const { isAuthenticated, isLoading, error, fetchState, setState, login } =
    useAuthStore();

  useEffect(() => {
    fetchState();

    const listeners: Array<() => void> = [];

    listen<{ isAuthenticated: boolean; user: AuthUser | null }>("auth:state-changed", (event) => {
      const state = event.payload;
      setState(state);
      if (state.isAuthenticated && state.user?.email) {
        import("@/api/server").then(({ setUserIdentity }) =>
          setUserIdentity(state.user!.email!, state.user!.sub),
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

    return () => {
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
