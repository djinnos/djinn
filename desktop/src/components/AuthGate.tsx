import { useAuthStore } from "@/stores/authStore";
import { startGithubLogin, type AuthUser } from "@/tauri/commands";
import { listen } from "@tauri-apps/api/event";
import { type ReactNode, useEffect, useState } from "react";
import { Button } from "@/components/ui/button";

export function AuthGate({ children }: { children: ReactNode }) {
  const { isAuthenticated, isLoading, error, fetchState, setState } =
    useAuthStore();

  const [deviceCode, setDeviceCode] = useState<{
    userCode: string;
    verificationUri: string;
  } | null>(null);
  const [loginPending, setLoginPending] = useState(false);

  useEffect(() => {
    const listeners: Array<() => void> = [];

    // Don't call fetchState() eagerly — the backend drives initial state via
    // events (auth:state-changed, auth:login-required, auth:silent-refresh-failed).
    // This avoids the race where fetchState() returns { isAuthenticated: false }
    // before the silent refresh has completed.

    listen<{ isAuthenticated: boolean; user: AuthUser | null }>("auth:state-changed", (event) => {
      const state = event.payload;
      setState(state);
      setDeviceCode(null);
      setLoginPending(false);
      if (state.isAuthenticated && state.user?.email) {
        const { email, sub } = state.user;
        import("@/api/server").then(({ setUserIdentity }) =>
          setUserIdentity(email, sub),
        );
      }
    }).then((u) => listeners.push(u));

    listen("auth:silent-refresh-success", () => {
      fetchState();
    }).then((u) => listeners.push(u));

    listen("auth:login-required", () => {
      useAuthStore.setState({ isLoading: false, isAuthenticated: false });
    }).then((u) => listeners.push(u));

    listen("auth:silent-refresh-failed", () => {
      useAuthStore.setState({ isLoading: false, isAuthenticated: false });
    }).then((u) => listeners.push(u));

    listen<{ reason: string }>("auth:login-failed", (event) => {
      setDeviceCode(null);
      setLoginPending(false);
      useAuthStore.setState({ error: `Login failed: ${event.payload.reason}` });
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

  const handleLogin = async () => {
    setLoginPending(true);
    setDeviceCode(null);
    useAuthStore.setState({ error: null });
    try {
      const info = await startGithubLogin();
      setDeviceCode(info);
    } catch (e) {
      setLoginPending(false);
      useAuthStore.setState({ error: `Login failed: ${e}` });
    }
  };

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

          {deviceCode ? (
            <>
              <p className="text-sm text-muted-foreground">
                Enter this code on GitHub to sign in:
              </p>
              <code className="rounded bg-muted px-4 py-2 text-2xl font-bold tracking-widest">
                {deviceCode.userCode}
              </code>
              <p className="text-xs text-muted-foreground">
                Visit{" "}
                <a
                  href={deviceCode.verificationUri}
                  target="_blank"
                  rel="noopener noreferrer"
                  className="underline text-foreground"
                >
                  {deviceCode.verificationUri}
                </a>
              </p>
              <p className="text-xs text-muted-foreground animate-pulse">
                Waiting for authorization...
              </p>
            </>
          ) : (
            <>
              <p className="text-sm text-muted-foreground">
                {error || "Please sign in to continue to Djinn."}
              </p>
              <Button onClick={() => void handleLogin()} disabled={loginPending}>
                {loginPending ? "Starting..." : "Sign in with GitHub"}
              </Button>
            </>
          )}
        </div>
      </main>
    );
  }

  return <>{children}</>;
}
