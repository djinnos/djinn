import { createContext, useContext, type ReactNode } from "react";
import { useQuery } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import { GithubIcon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import logoSvg from "@/assets/logo.svg";
import { LoadingScreen } from "@/components/LoadingScreen";
import { fetchCurrentUser, startGithubLogin, type User } from "@/api/auth";

const AuthUserContext = createContext<User | null>(null);

/**
 * Read the authenticated user from the nearest AuthGate.
 * Returns null when called outside an AuthGate (shouldn't happen in practice,
 * since AuthGate wraps the entire app).
 */
export function useAuthUser(): User | null {
  return useContext(AuthUserContext);
}

export const AUTH_ME_QUERY_KEY = ["auth", "me"] as const;

export function AuthGate({ children }: { children: ReactNode }) {
  const { data, isLoading, isError, error } = useQuery({
    queryKey: AUTH_ME_QUERY_KEY,
    queryFn: fetchCurrentUser,
    retry: false,
    staleTime: 60_000,
  });

  if (isLoading) {
    return <LoadingScreen message="Checking authentication..." />;
  }

  if (!data) {
    const errorMessage = isError
      ? error instanceof Error
        ? error.message
        : "Could not reach the Djinn server."
      : null;

    return (
      <main className="flex min-h-screen items-center justify-center bg-background text-foreground">
        <div className="flex w-full max-w-md flex-col items-center gap-6 p-8 text-center">
          <div className="relative mb-2">
            <div
              className="pointer-events-none absolute left-1/2 top-1/2 -translate-x-1/2 -translate-y-1/2 h-16 w-16 rounded-full bg-purple-400/40"
              style={{ filter: "blur(40px)" }}
            />
            <img
              src={logoSvg}
              alt="Djinn"
              className="relative h-24 w-auto drop-shadow-[0_0_40px_rgba(168,139,250,0.35)]"
            />
          </div>

          <div className="space-y-2">
            <h2 className="text-lg font-semibold">Sign in required</h2>
            <p className="text-sm text-muted-foreground">
              {errorMessage ?? "Please sign in to continue to Djinn."}
            </p>
          </div>

          <Button
            onClick={() => startGithubLogin()}
            variant="outline"
            className="!bg-white !text-black hover:!bg-gray-100 !border-gray-300 gap-2 px-6 h-11 text-base"
          >
            <HugeiconsIcon icon={GithubIcon} size={20} />
            Sign in with GitHub
          </Button>

          <p className="text-xs text-muted-foreground">
            By continuing you agree to our{" "}
            <a
              href="https://www.djinnai.io/terms"
              target="_blank"
              rel="noopener noreferrer"
              className="underline hover:text-foreground"
            >
              Terms
            </a>{" "}
            and{" "}
            <a
              href="https://www.djinnai.io/privacy"
              target="_blank"
              rel="noopener noreferrer"
              className="underline hover:text-foreground"
            >
              Privacy Policy
            </a>
            .
          </p>
        </div>
      </main>
    );
  }

  return (
    <AuthUserContext.Provider value={data}>{children}</AuthUserContext.Provider>
  );
}
