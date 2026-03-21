import { useAuthStore } from "@/stores/authStore";
import { startGithubLogin, type AuthUser } from "@/tauri/commands";
import { listen } from "@tauri-apps/api/event";
import { type ReactNode, useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { Copy, Check } from "lucide-react";
import { GithubIcon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import logoSvg from "@/assets/logo.svg";

export function AuthGate({ children }: { children: ReactNode }) {
  const { isAuthenticated, isLoading, error, fetchState, setState } =
    useAuthStore();

  const [deviceCode, setDeviceCode] = useState<{
    userCode: string;
    verificationUri: string;
  } | null>(null);
  const [loginPending, setLoginPending] = useState(false);
  const [copied, setCopied] = useState(false);

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
        <div className="flex w-full max-w-md flex-col items-center gap-6 p-8 text-center">
          <div className="relative mb-2">
            <div
              className="pointer-events-none absolute left-1/2 top-1/2 -translate-x-1/2 -translate-y-1/2 h-16 w-16 rounded-full bg-purple-400/40"
              style={{ filter: "blur(40px)" }}
            />
            <img src={logoSvg} alt="Djinn" className="relative h-24 w-auto drop-shadow-[0_0_40px_rgba(168,139,250,0.35)]" />
          </div>

          {deviceCode ? (
            <>
              <div className="space-y-2">
                <h2 className="text-lg font-semibold">Enter code on GitHub</h2>
                <p className="text-sm text-muted-foreground">
                  Copy the code below and enter it at GitHub to continue.
                </p>
              </div>
              <button
                className="group flex items-center gap-3 rounded-lg bg-muted px-5 py-3 cursor-pointer hover:bg-muted/80 transition-colors"
                onClick={() => {
                  void navigator.clipboard.writeText(deviceCode.userCode);
                  setCopied(true);
                  setTimeout(() => setCopied(false), 2000);
                }}
              >
                <code className="select-all text-2xl font-bold tracking-[0.3em]">
                  {deviceCode.userCode}
                </code>
                {copied ? <Check className="h-4 w-4 text-emerald-500" /> : <Copy className="h-4 w-4 text-muted-foreground group-hover:text-foreground transition-colors" />}
              </button>
              <a
                href={deviceCode.verificationUri}
                target="_blank"
                rel="noopener noreferrer"
                className="text-sm underline text-foreground hover:text-foreground/80"
              >
                {deviceCode.verificationUri}
              </a>
              <p className="text-xs text-muted-foreground animate-pulse">
                Waiting for authorization...
              </p>
            </>
          ) : (
            <>
              <div className="space-y-2">
                <h2 className="text-lg font-semibold">Sign in required</h2>
                <p className="text-sm text-muted-foreground">
                  {error ? error : "Please sign in to continue to Djinn."}
                </p>
              </div>

              <Button
                onClick={() => void handleLogin()}
                disabled={loginPending}
                variant="outline"
                className="!bg-white !text-black hover:!bg-gray-100 !border-gray-300 gap-2 px-6 h-11 text-base"
              >
                <HugeiconsIcon icon={GithubIcon} size={20} />
                {loginPending ? "Starting..." : "Sign in with GitHub"}
              </Button>

              <p className="text-xs text-muted-foreground">
                By continuing you agree to our{" "}
                <a href="https://www.djinnai.io/terms" target="_blank" rel="noopener noreferrer" className="underline hover:text-foreground">Terms</a>
                {" "}and{" "}
                <a href="https://www.djinnai.io/privacy" target="_blank" rel="noopener noreferrer" className="underline hover:text-foreground">Privacy Policy</a>.
              </p>
            </>
          )}
        </div>
      </main>
    );
  }

  return <>{children}</>;
}
