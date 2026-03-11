import {
  authGetState,
  authLogin,
  authLogout,
  exchangeAuthCode,
  type AuthState,
  CLIENT_ID,
} from "@/tauri/commands";
import { listen } from "@tauri-apps/api/event";
import { type ReactNode, useEffect, useState } from "react";
import { Button } from "@/components/ui/button";

type AuthGateProps = {
  children: ReactNode;
  sidebarContent?: ReactNode;
};

export function AuthGate({ children, sidebarContent }: AuthGateProps) {
  const [authState, setAuthState] = useState<AuthState | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let active = true;
    const listeners: Array<() => void> = [];

    const loadState = async () => {
      const state = await authGetState();
      if (active) {
        setAuthState(state);
        setError(null);
      }
    };

    loadState();

    // Listen for auth state changes
    listen<AuthState>("auth:state-changed", (event) => {
      if (!active) return;
      setAuthState(event.payload);
      setError(null);
    }).then((unlisten) => {
      listeners.push(unlisten);
    });

    // Listen for OAuth callback received (contains code + code_verifier)
    listen<{ code: string; state: string; code_verifier: string }>(
      "auth:callback-received",
      async (event) => {
        if (!active) return;
        const { code, code_verifier } = event.payload;

        try {
          await exchangeAuthCode(
            code,
            code_verifier,
            "djinn://auth/callback",
            CLIENT_ID
          );
          // This triggers auth:state-changed event on success
          setError(null);
        } catch (e) {
          console.error("Token exchange failed:", e);
          setError(`Authentication failed: ${e}`);
        }
      }
    ).then((unlisten) => {
      listeners.push(unlisten);
    });

    // Listen for silent refresh success - refresh UI state
    listen("auth:silent-refresh-success", async () => {
      if (!active) return;
      try {
        const state = await authGetState();
        setAuthState(state);
        setError(null);
      } catch (e) {
        console.error("Failed to get auth state after silent refresh:", e);
      }
    }).then((unlisten) => {
      listeners.push(unlisten);
    });

    return () => {
      active = false;
      listeners.forEach((unlisten) => unlisten());
    };
  }, []);

  if (!authState?.isAuthenticated) {
    return (
      <main className="flex min-h-screen items-center justify-center bg-background text-foreground">
        <div className="flex w-full max-w-md flex-col items-center gap-4 rounded-lg border p-8 text-center">
          <h1 className="text-2xl font-semibold">Sign in required</h1>
          <p className="text-sm text-muted-foreground">
            {error || "Please sign in to continue to DjinnOS."}
          </p>
          <Button onClick={() => authLogin()}>Sign in</Button>
        </div>
      </main>
    );
  }

  return (
    <main className="flex min-h-screen bg-background text-foreground">
      <aside className="w-64 border-r p-4">
        <h2 className="mb-4 text-sm font-semibold uppercase text-muted-foreground">Profile</h2>
        <div className="mb-4 flex items-center gap-3">
          {authState.user?.picture ? (
            <img
              src={authState.user.picture}
              alt={authState.user.name || authState.user.email || "User avatar"}
              className="h-10 w-10 rounded-full"
            />
          ) : (
            <div className="h-10 w-10 rounded-full bg-muted" />
          )}
          <div className="min-w-0">
            <p className="truncate text-sm font-medium">{authState.user?.name || "Unknown user"}</p>
            <p className="truncate text-xs text-muted-foreground">{authState.user?.email || "No email"}</p>
          </div>
        </div>
        <Button variant="secondary" size="sm" onClick={() => authLogout()}>
          Sign out
        </Button>
        {sidebarContent ? <div className="mt-6">{sidebarContent}</div> : null}
      </aside>
      <div className="flex min-h-screen flex-1 flex-col">{children}</div>
    </main>
  );
}
