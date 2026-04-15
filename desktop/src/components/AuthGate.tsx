import { createContext, useContext, type ReactNode } from "react";
import { useQuery } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import { GithubIcon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import logoSvg from "@/assets/logo.svg";
import { LoadingScreen } from "@/components/LoadingScreen";
import {
  fetchAuthConfig,
  fetchCurrentUser,
  startGithubLogin,
  startManifestProvision,
  type User,
} from "@/api/auth";

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

          <AuthBody
            isError={isError}
            errorMessage={
              error instanceof Error
                ? error.message
                : "Could not reach the Djinn server."
            }
          />

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

function AuthBody({
  isError,
  errorMessage,
}: {
  isError: boolean;
  errorMessage: string;
}) {
  const { data: cfg, isLoading } = useQuery({
    queryKey: ["auth", "config"],
    queryFn: fetchAuthConfig,
    retry: false,
    staleTime: 60_000,
  });

  if (isLoading) {
    return (
      <p className="text-sm text-muted-foreground">Checking server configuration…</p>
    );
  }

  // Server is reachable and GitHub App is configured → normal sign-in.
  if (cfg?.configured) {
    return (
      <>
        <div className="space-y-2">
          <h2 className="text-lg font-semibold">Sign in required</h2>
          <p className="text-sm text-muted-foreground">
            Please sign in to continue to Djinn.
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
      </>
    );
  }

  // Server unreachable and no config response.
  if (!cfg) {
    return (
      <div className="space-y-2">
        <h2 className="text-lg font-semibold">Can't reach the server</h2>
        <p className="text-sm text-muted-foreground">
          {isError ? errorMessage : "The Djinn server did not respond."}
        </p>
      </div>
    );
  }

  // Server reachable but GitHub App is not fully configured.
  return (
    <div className="w-full space-y-5 text-left">
      <div className="space-y-2 text-center">
        <h2 className="text-lg font-semibold">One step left</h2>
        <p className="text-sm text-muted-foreground">
          Djinn needs a GitHub App. The button below walks you through GitHub's
          one-click "Create App" page — Djinn handles the rest.
        </p>
      </div>

      {cfg.createAppUrl ? (
        <div className="flex justify-center">
          <Button
            onClick={() => startManifestProvision(cfg.createAppUrl!)}
            className="gap-2 px-6 h-11 text-base"
          >
            <HugeiconsIcon icon={GithubIcon} size={20} />
            Create Djinn GitHub App
          </Button>
        </div>
      ) : null}

      <details className="rounded-md border border-border bg-card/40 p-4 text-sm text-muted-foreground">
        <summary className="cursor-pointer text-foreground">
          Prefer to do it manually? →
        </summary>
        <ol className="mt-3 list-decimal space-y-3 pl-5">
          <li>
            Create a GitHub App at{" "}
            <a
              className="underline hover:text-foreground"
              href="https://github.com/settings/apps/new"
              target="_blank"
              rel="noopener noreferrer"
            >
              github.com/settings/apps/new
            </a>
            . Callback URL:{" "}
            <code className="rounded bg-muted px-1 font-mono text-xs">
              {`${window.location.origin.replace(/5173/, "8372")}/auth/github/callback`}
            </code>
            . Enable "Request user authorization (OAuth) during installation".
          </li>
          <li>
            Set the following environment variables in the server's{" "}
            <code className="rounded bg-muted px-1 font-mono text-xs">.env</code>{" "}
            (missing now):
            <ul className="mt-2 list-disc space-y-1 pl-5">
              {cfg.missing.map((k) => (
                <li key={k}>
                  <code className="rounded bg-muted px-1 font-mono text-xs">
                    {k}
                  </code>
                </li>
              ))}
            </ul>
          </li>
          <li>
            Restart the stack:{" "}
            <code className="rounded bg-muted px-1 font-mono text-xs">
              docker compose up -d
            </code>
            . Then reload this page.
          </li>
        </ol>
      </details>

      <div className="flex justify-center">
        <a
          href={cfg.setupDocUrl}
          target="_blank"
          rel="noopener noreferrer"
          className="inline-flex h-9 items-center justify-center rounded-md border border-input bg-background px-4 py-2 text-sm font-medium ring-offset-background transition-colors hover:bg-accent hover:text-accent-foreground"
        >
          Full setup guide →
        </a>
      </div>
    </div>
  );
}
