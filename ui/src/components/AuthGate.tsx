import { createContext, useContext, type ReactNode } from "react";
import { useQuery } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import { GithubIcon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import logoSvg from "@/assets/logo.svg";
import { LoadingScreen } from "@/components/LoadingScreen";
import { InstallationPicker } from "@/components/InstallationPicker";
import {
  fetchCurrentUser,
  fetchSetupStatus,
  startGithubLogin,
  type SetupStatus,
  type User,
} from "@/api/auth";

const SETUP_DOC_URL = "https://www.djinnai.io/docs/setup";

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
  const {
    data: user,
    isLoading: userLoading,
    isError: userIsError,
    error: userError,
  } = useQuery({
    queryKey: AUTH_ME_QUERY_KEY,
    queryFn: fetchCurrentUser,
    retry: false,
    staleTime: 60_000,
  });
  // Also check whether the deployment itself is provisioned. A stale session
  // can outlive a wiped credential vault (user_auth_sessions and credentials
  // are separate tables), and the org_config row can be reset independently,
  // so we block on both "am I signed in?" AND "is the App+org bound?" to
  // avoid landing in a half-working main app.
  const {
    data: setupStatus,
    isLoading: setupLoading,
    isError: setupIsError,
    error: setupError,
  } = useQuery({
    queryKey: ["auth", "setup-status"],
    queryFn: fetchSetupStatus,
    retry: false,
    staleTime: 60_000,
  });

  if (userLoading || setupLoading) {
    return <LoadingScreen message="Checking authentication..." />;
  }

  // Collapse the two possible loading/error sources into one set of props for
  // the shell, then let AuthBody pick the right screen.
  const reachError = setupIsError
    ? setupError instanceof Error
      ? setupError.message
      : "Could not reach the Djinn server."
    : userIsError
      ? userError instanceof Error
        ? userError.message
        : "Could not reach the Djinn server."
      : null;

  const needsAppInstall = !setupStatus || setupStatus.needsAppInstall;
  const needsSignin = !needsAppInstall && !user;

  if (needsAppInstall || needsSignin) {
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
            setupStatus={setupStatus ?? null}
            reachError={reachError}
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
    <AuthUserContext.Provider value={user!}>{children}</AuthUserContext.Provider>
  );
}

function AuthBody({
  setupStatus,
  reachError,
}: {
  setupStatus: SetupStatus | null;
  reachError: string | null;
}) {
  // Server unreachable or /setup/status errored.
  if (!setupStatus) {
    return (
      <div className="space-y-2">
        <h2 className="text-lg font-semibold">Can't reach the server</h2>
        <p className="text-sm text-muted-foreground">
          {reachError ?? "The Djinn server did not respond."}
        </p>
      </div>
    );
  }

  // App + org are provisioned → normal sign-in.
  if (!setupStatus.needsAppInstall) {
    return (
      <>
        <div className="space-y-2">
          <h2 className="text-lg font-semibold">Sign in required</h2>
          <p className="text-sm text-muted-foreground">
            {setupStatus.orgLogin
              ? `Djinn is bound to github.com/${setupStatus.orgLogin}. Sign in with a member account to continue.`
              : "Please sign in to continue to Djinn."}
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

  // App credentials are present but no installation is bound yet — render
  // the in-UI picker so the operator can pick one without editing the
  // Secret. Env-driven `GITHUB_INSTALLATION_ID` short-circuits this branch
  // entirely on CI deploys (`needsAppInstall` flips to false on the server
  // when env binding is set).
  if (setupStatus.appCredentialsConfigured) {
    return <InstallationPicker />;
  }

  // Server reachable but the App credentials themselves are missing (no
  // Secret mounted, env unset). The UI can't recover automatically —
  // point operators at the runbook.
  return (
    <div className="w-full space-y-4 text-left">
      <div className="space-y-2 text-center">
        <h2 className="text-lg font-semibold">GitHub App not configured</h2>
        <p className="text-sm text-muted-foreground">
          {setupStatus.orgLogin
            ? `Djinn is bound to github.com/${setupStatus.orgLogin}, but the App credentials are missing or incomplete on the server.`
            : "This Djinn deployment has no GitHub App credentials wired in yet."}
        </p>
      </div>

      <div className="rounded-lg border border-border/60 bg-card/50 p-4 text-sm text-muted-foreground">
        <p>
          Set the <code className="rounded bg-muted px-1 py-0.5 font-mono text-xs">GITHUB_APP_*</code>{" "}
          env vars or mount the{" "}
          <code className="rounded bg-muted px-1 py-0.5 font-mono text-xs">djinn-github-app</code>{" "}
          Kubernetes Secret on the server Pod, then restart it. See{" "}
          <code className="rounded bg-muted px-1 py-0.5 font-mono text-xs">
            server/docker/README.md
          </code>{" "}
          for the runbook.
        </p>
      </div>

      <div className="text-center">
        <a
          href={SETUP_DOC_URL}
          target="_blank"
          rel="noopener noreferrer"
          className="text-xs text-muted-foreground underline-offset-4 hover:text-foreground hover:underline"
        >
          Read the full setup guide
        </a>
      </div>
    </div>
  );
}
