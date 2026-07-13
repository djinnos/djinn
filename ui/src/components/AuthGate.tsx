import {
  createContext,
  useContext,
  useRef,
  useState,
  type FormEvent,
  type ReactNode,
} from "react";
import { useQuery } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import { GithubIcon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import logoSvg from "@/assets/logo.svg";
import { LoadingScreen } from "@/components/LoadingScreen";
import { InstallationPicker } from "@/components/InstallationPicker";
import {
  fetchAuthConfig,
  fetchCurrentUser,
  fetchSetupStatus,
  startGithubLogin,
  type AuthConfig,
  type SetupStatus,
  type User,
} from "@/api/auth";

const SETUP_DOC_URL = "https://www.djinnai.io/docs/setup";

const AuthUserContext = createContext<User | null>(null);

function SetupLaunchPanel() {
  const [isRefreshing, setIsRefreshing] = useState(false);
  const [launchError, setLaunchError] = useState<string | null>(null);
  const refreshInFlight = useRef(false);
  const setupExpired =
    typeof window !== "undefined" &&
    new URLSearchParams(window.location.search).get("setup") === "expired";

  const handleSubmit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (refreshInFlight.current) return;

    const form = event.currentTarget;
    refreshInFlight.current = true;
    setIsRefreshing(true);
    setLaunchError(null);

    try {
      // Refresh immediately before the POST so the short-lived, HttpOnly
      // launch cookie cannot expire while the operator reads this screen.
      const refreshedConfig = await fetchAuthConfig();
      if (
        !refreshedConfig.selfSetupAvailable ||
        !refreshedConfig.setupLaunchAvailable
      ) {
        setLaunchError(
          "Secure setup is no longer available from this browser address. Use the one-time setup URL under Having trouble?",
        );
        return;
      }

      // Native submission preserves the server redirect/manifest flow. There
      // are deliberately no token-bearing fields in this form.
      form.submit();
    } catch {
      setLaunchError(
        "Could not refresh secure setup access. Check the server connection and try again.",
      );
    } finally {
      refreshInFlight.current = false;
      setIsRefreshing(false);
    }
  };

  return (
    <div className="w-full space-y-5 text-left">
      <div className="space-y-2 text-center">
        <h2 className="text-lg font-semibold">Connect GitHub</h2>
        <p className="text-sm text-muted-foreground">
          Connect Djinn only to the repositories you choose. GitHub shows the
          requested permissions before the App is installed.
        </p>
      </div>

      {setupExpired ? (
        <div
          role="status"
          aria-live="polite"
          className="rounded-lg border border-primary/30 bg-primary/10 px-4 py-3 text-sm text-foreground"
        >
          Your setup access expired. Secure access has been refreshed; continue
          when you are ready.
        </div>
      ) : null}

      <form
        action="/auth/github/setup-start"
        method="post"
        aria-label="Start GitHub App setup"
        aria-busy={isRefreshing}
        onSubmit={handleSubmit}
      >
        <Button
          type="submit"
          size="lg"
          className="h-11 w-full gap-2 px-6 text-base"
          disabled={isRefreshing}
          aria-describedby={launchError ? "github-setup-launch-error" : undefined}
        >
          <HugeiconsIcon icon={GithubIcon} size={20} />
          {isRefreshing ? "Refreshing secure access…" : "Continue with GitHub"}
        </Button>
      </form>

      {launchError ? (
        <p
          id="github-setup-launch-error"
          role="alert"
          className="text-center text-sm text-destructive"
        >
          {launchError}
        </p>
      ) : null}

      <div
        className="rounded-lg border border-border/60 bg-card/50 p-4"
        aria-labelledby="github-setup-steps"
      >
        <p
          id="github-setup-steps"
          className="mb-3 text-xs font-medium uppercase tracking-wide text-muted-foreground"
        >
          What happens next
        </p>
        <ol className="space-y-3 text-sm">
          {[
            ["Review permissions", "Confirm what Djinn can access."],
            ["Choose repositories", "Select only the repositories you want."],
            ["Return to Djinn", "Setup finishes here automatically."],
          ].map(([title, description], index) => (
            <li key={title} className="flex gap-3">
              <span
                className="flex h-5 w-5 shrink-0 items-center justify-center rounded-full bg-primary/15 text-xs font-semibold text-primary"
                aria-hidden="true"
              >
                {index + 1}
              </span>
              <span>
                <span className="font-medium text-foreground">{title}</span>
                <span className="block text-xs text-muted-foreground">
                  {description}
                </span>
              </span>
            </li>
          ))}
        </ol>
      </div>

      <p className="text-center text-xs text-muted-foreground">
        No tokens or private keys to copy into Djinn.
      </p>

      <details className="group rounded-lg border border-border/60 px-4 py-3 text-sm text-muted-foreground">
        <summary className="cursor-pointer select-none font-medium text-foreground outline-none focus-visible:rounded-sm focus-visible:ring-2 focus-visible:ring-ring/50">
          Having trouble?
        </summary>
        <div className="mt-3 space-y-3 border-t border-border/60 pt-3 text-xs leading-relaxed">
          <p>
            If setup has not started, use the one-time setup URL from the server
            boot logs. If it already started or the link expired, restart the
            local server to generate a new URL. Never paste that URL or its
            token into this page.
          </p>
          <a
            href={SETUP_DOC_URL}
            target="_blank"
            rel="noopener noreferrer"
            className="inline-block underline underline-offset-4 hover:text-foreground"
          >
            Read the full setup guide
          </a>
        </div>
      </details>
    </div>
  );
}

/**
 * Read the authenticated user from the nearest AuthGate.
 * Returns null when called outside an AuthGate (shouldn't happen in practice,
 * since AuthGate wraps the entire app).
 */
// eslint-disable-next-line react-refresh/only-export-components -- context hook colocated with its provider; used by 14 call sites.
export function useAuthUser(): User | null {
  return useContext(AuthUserContext);
}

export const AUTH_ME_QUERY_KEY = ["auth", "me"] as const;
export const AUTH_CONFIG_QUERY_KEY = ["auth", "config"] as const;

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
    // Always refetch on mount / window-focus. This query gates the entire
    // app on "is the deployment provisioned?", and caching a stale `false`
    // answer locks the UI on the "App not configured" screen for a minute
    // after the operator fixes the Secret.
    staleTime: 0,
  });

  // Auth config carries both whether self-setup is enabled and whether this
  // browser response received the origin-bound launch capability. If this
  // endpoint errors, we default to `null` and treat self-setup as unavailable,
  // preserving existing production behavior. We DO wait for it during the
  // initial loading phase so the gate doesn't flash the wrong screen.
  const {
    data: authConfig,
    isLoading: configLoading,
    isError: configIsError,
    error: configError,
  } = useQuery({
    queryKey: AUTH_CONFIG_QUERY_KEY,
    queryFn: fetchAuthConfig,
    retry: false,
    staleTime: 0,
  });

  if (userLoading || setupLoading || configLoading) {
    return <LoadingScreen message="Checking authentication..." />;
  }

  // Collapse the three possible loading/error sources into one set of props for
  // the shell, then let AuthBody pick the right screen. Auth-config errors are
  // non-fatal — if the endpoint is missing on an older server, we just won't
  // show self-setup guidance, which preserves the existing runbook screen.
  const reachError = setupIsError
    ? setupError instanceof Error
      ? setupError.message
      : "Could not reach the Djinn server."
    : userIsError
      ? userError instanceof Error
        ? userError.message
        : "Could not reach the Djinn server."
      : configIsError
        ? configError instanceof Error
          ? configError.message
          : "Could not reach the Djinn server."
        : null;

  const needsAppInstall = !setupStatus || setupStatus.needsAppInstall;
  const needsSignin = !needsAppInstall && !user;
  const authStateUnavailable = setupIsError || userIsError;

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
            setupStatus={authStateUnavailable ? null : setupStatus ?? null}
            authConfig={authConfig ?? null}
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
  authConfig,
  reachError,
}: {
  setupStatus: SetupStatus | null;
  authConfig: AuthConfig | null;
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

  // ─── Credentials missing — distinguish why and whether self-setup helps ───
  //
  // From here on, `needsAppInstall === true` and `appCredentialsConfigured
  // === false`. We branch on the credential-source state machine to show the
  // right operator-facing guidance. Fatal and recovery states take priority
  // over self-setup so the UI never silently falls through to a setup CTA
  // when the real problem is a broken Secret or an undecryptable vault.

  // Fatal: the mounted Secret exists but is invalid or incomplete. The
  // operator must fix it — self-setup is NOT offered as a silent fallback
  // because the Secret presence signals an intentional production deployment.
  if (setupStatus.setupState === "invalid_secret") {
    return (
      <div className="w-full space-y-4 text-left">
        <div className="space-y-2 text-center">
          <h2 className="text-lg font-semibold">GitHub App Secret is invalid</h2>
          <p className="text-sm text-muted-foreground">
            The mounted GitHub App Secret contains invalid or incomplete
            credentials. Fix the Secret values and restart the server.
          </p>
        </div>

        <div className="rounded-lg border border-border/60 bg-card/50 p-4 text-sm text-muted-foreground">
          <p>
            {setupStatus.setupError
              ? setupStatus.setupError
              : "The Secret is missing required fields or the values are malformed."}
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

  // Recovery: persisted credentials are present but cannot be decrypted
  // (wrong vault key, corrupt data). The operator must re-provision or
  // restore the vault key — this is not a generic "missing config" case.
  if (
    setupStatus.setupState === "unrecoverable" ||
    setupStatus.credentialsUnrecoverable
  ) {
    return (
      <div className="w-full space-y-4 text-left">
        <div className="space-y-2 text-center">
          <h2 className="text-lg font-semibold">
            Stored credentials cannot be recovered
          </h2>
          <p className="text-sm text-muted-foreground">
            Previously saved GitHub App credentials are present but cannot be
            decrypted. Reset the persisted credentials or restore the vault
            encryption key, then restart the server.
          </p>
        </div>

        <div className="rounded-lg border border-border/60 bg-card/50 p-4 text-sm text-muted-foreground">
          <p>
            {setupStatus.setupError
              ? setupStatus.setupError
              : "The persisted credential store could not be decrypted. Re-provision the GitHub App credentials to recover."}
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

  // Setup in progress or a retryable error. Direct the operator to restart
  // from the boot-log setup URL or the setup route — never ask them to paste
  // secrets or tokens into the browser.
  if (setupStatus.setupRetryable) {
    return (
      <div className="w-full space-y-4 text-left">
        <div className="space-y-2 text-center">
          <h2 className="text-lg font-semibold">
            GitHub App setup in progress
          </h2>
          <p className="text-sm text-muted-foreground">
            Setup is in progress or hit a temporary error. Restart the server
            and open the setup URL from the boot logs to retry.
          </p>
        </div>

        <div className="rounded-lg border border-border/60 bg-card/50 p-4 text-sm text-muted-foreground">
          <p>
            {setupStatus.setupError
              ? setupStatus.setupError
              : "Waiting for the setup flow to complete. If this persists, restart the server."}
          </p>
        </div>
      </div>
    );
  }

  // Self-setup enabled but credentials not yet configured — start the secure
  // same-origin setup flow directly. The server owns the short-lived launch
  // capability; the UI never receives or renders a setup token.
  if (
    authConfig?.selfSetupAvailable &&
    authConfig.setupLaunchAvailable
  ) {
    return <SetupLaunchPanel />;
  }

  // Self-setup is enabled, but this response did not establish the
  // origin-bound launch capability. This can happen on an older server or
  // when the UI was opened from a non-canonical address. Keep the secure
  // one-time URL as the real path forward and do not render a dead CTA.
  if (authConfig?.selfSetupAvailable) {
    return (
      <div className="w-full space-y-4 text-left">
        <div className="space-y-2 text-center">
          <h2 className="text-lg font-semibold">
            Open the GitHub setup link
          </h2>
          <p className="text-sm text-muted-foreground">
            Direct setup is not available from this browser address. Use the
            current one-time setup URL, or restart the local server if setup
            already started.
          </p>
        </div>

        <div className="rounded-lg border border-border/60 bg-card/50 p-4 text-sm text-muted-foreground">
          <ol className="space-y-3">
            <li className="flex gap-3">
              <span
                className="flex h-5 w-5 shrink-0 items-center justify-center rounded-full bg-primary/15 text-xs font-semibold text-primary"
                aria-hidden="true"
              >
                1
              </span>
              <span>
                If setup has not started, find the one-time URL in the server
                boot logs.
              </span>
            </li>
            <li className="flex gap-3">
              <span
                className="flex h-5 w-5 shrink-0 items-center justify-center rounded-full bg-primary/15 text-xs font-semibold text-primary"
                aria-hidden="true"
              >
                2
              </span>
              <span>
                If it already started, restart the local server to generate a
                new URL, then open it in this browser.
              </span>
            </li>
          </ol>
        </div>

        <p className="text-center text-xs text-muted-foreground">
          Treat the one-time URL like a password. Do not paste it into this
          page or share it.
        </p>

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

  // Self-setup disabled/unconfigured: server reachable but the App credentials
  // themselves are missing (no Secret mounted, env unset). The UI can't
  // recover automatically — point operators at the manual runbook.
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
