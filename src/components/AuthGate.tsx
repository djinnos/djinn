import { authGetState, authLogin, authLogout, type AuthState } from "@/tauri/commands";
import { listen } from "@tauri-apps/api/event";
import { type ReactNode, useEffect, useState } from "react";
import { Button } from "@/components/ui/button";

type AuthGateProps = {
  children: ReactNode;
  sidebarContent?: ReactNode;
};

export function AuthGate({ children, sidebarContent }: AuthGateProps) {
  const [authState, setAuthState] = useState<AuthState | null>(null);

  useEffect(() => {
    let active = true;

    const loadState = async () => {
      const state = await authGetState();
      if (active) {
        setAuthState(state);
      }
    };

    loadState();

    const unlistenPromise = listen<AuthState>("auth:state-changed", (event) => {
      if (!active) return;
      setAuthState(event.payload);
    });

    return () => {
      active = false;
      unlistenPromise.then((unlisten) => unlisten());
    };
  }, []);

  if (!authState?.isAuthenticated) {
    return (
      <main className="flex min-h-screen items-center justify-center bg-background text-foreground">
        <div className="flex w-full max-w-md flex-col items-center gap-4 rounded-lg border p-8 text-center">
          <h1 className="text-2xl font-semibold">Sign in required</h1>
          <p className="text-sm text-muted-foreground">Please sign in to continue to DjinnOS.</p>
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
