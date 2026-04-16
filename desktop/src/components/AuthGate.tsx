import { createContext, useContext, useState, type ReactNode } from "react";
import { useQuery } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import {
  InputGroup,
  InputGroupAddon,
  InputGroupInput,
} from "@/components/ui/input-group";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import { ArrowRight01Icon, GithubIcon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import logoSvg from "@/assets/logo.svg";
import { LoadingScreen } from "@/components/LoadingScreen";
import {
  fetchCurrentUser,
  fetchSetupStatus,
  startGithubLogin,
  startManifestProvision,
  type SetupStatus,
  type User,
} from "@/api/auth";

// The server's manifest-provision endpoint path. `/setup/status` intentionally
// does NOT return this — it's a stable, well-known route owned by the client.
const CREATE_APP_PATH = "/auth/github/create-app";
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

  // Server reachable but the App isn't installed OR no org is bound yet.
  // Either way, the operator needs to run the manifest flow.
  return (
    <div className="w-full space-y-6 text-left">
      <div className="space-y-1.5 text-center">
        <h2 className="text-lg font-semibold">Install the Djinn GitHub App</h2>
        <p className="text-sm text-muted-foreground">
          {setupStatus.orgLogin
            ? `Djinn is bound to github.com/${setupStatus.orgLogin}. Install the App there to continue.`
            : "Djinn needs a GitHub App on your organization. We'll walk you through GitHub's one-click create flow — the rest is automatic."}
        </p>
      </div>

      <CreateAppSection
        createAppUrl={CREATE_APP_PATH}
        defaultOrgLogin={setupStatus.orgLogin}
      />

      <ManualSetupCollapsible />

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

function CreateAppSection({
  createAppUrl,
  defaultOrgLogin,
}: {
  createAppUrl: string;
  defaultOrgLogin: string | null;
}) {
  // If the server has already bound an org (manifest flow interrupted between
  // "App created" and "App installed"), default the tab to "org" and prefill
  // so the operator only has to click through.
  const [target, setTarget] = useState<"personal" | "org">(
    defaultOrgLogin ? "org" : "personal",
  );
  const [orgLogin, setOrgLogin] = useState(defaultOrgLogin ?? "");

  const trimmedOrg = orgLogin.trim();
  const canSubmit = target === "personal" || trimmedOrg.length > 0;
  const submit = () =>
    startManifestProvision(
      createAppUrl,
      target === "org" ? trimmedOrg : undefined,
    );

  return (
    <div className="rounded-xl border border-border/60 bg-card/50 p-5 space-y-4">
      <Tabs
        value={target}
        onValueChange={(v) => setTarget((v as "personal" | "org") ?? "personal")}
      >
        <TabsList className="grid w-full grid-cols-2 h-9">
          <TabsTrigger value="personal">Personal account</TabsTrigger>
          <TabsTrigger value="org">Organization</TabsTrigger>
        </TabsList>
      </Tabs>

      <div className="min-h-[64px]">
        {target === "org" ? (
          <div className="space-y-1.5">
            <Label
              htmlFor="org-login"
              className="text-xs font-medium text-muted-foreground"
            >
              Organization handle
            </Label>
            <InputGroup>
              <InputGroupAddon className="pl-2.5 text-muted-foreground/70">
                @
              </InputGroupAddon>
              <InputGroupInput
                id="org-login"
                autoFocus
                autoComplete="off"
                spellCheck={false}
                placeholder="acme-inc"
                value={orgLogin}
                onChange={(e) =>
                  setOrgLogin(e.target.value.replace(/^@+/, ""))
                }
                onKeyDown={(e) => {
                  if (e.key === "Enter" && canSubmit) submit();
                }}
              />
            </InputGroup>
            <p className="text-xs text-muted-foreground">
              Same handle as in{" "}
              <code className="rounded bg-muted px-1 py-0.5 font-mono text-[11px]">
                github.com/acme-inc
              </code>
              . You must be an owner.
            </p>
          </div>
        ) : (
          <p className="text-xs text-muted-foreground leading-relaxed">
            The App will be created under your personal GitHub account and can
            only be installed there. Choose <span className="text-foreground">Organization</span>{" "}
            to create it under an org instead.
          </p>
        )}
      </div>

      <Button
        disabled={!canSubmit}
        onClick={submit}
        className="w-full gap-2 h-10"
      >
        <HugeiconsIcon icon={GithubIcon} size={18} />
        Continue to GitHub
      </Button>

      <p className="text-[11px] leading-relaxed text-muted-foreground/80 text-center">
        You can make this App public later in its GitHub settings to install it
        on additional orgs or accounts.
      </p>
    </div>
  );
}

function ManualSetupCollapsible() {
  const [open, setOpen] = useState(false);
  const callbackUrl = `${window.location.origin.replace(/5173/, "8372")}/auth/github/callback`;

  return (
    <Collapsible
      open={open}
      onOpenChange={setOpen}
      className="rounded-xl border border-border/60 bg-card/30 overflow-hidden"
    >
      <CollapsibleTrigger className="flex w-full items-center justify-between gap-2 px-4 py-3 text-sm text-muted-foreground transition-colors hover:bg-muted/30 hover:text-foreground">
        <span>Prefer to set it up manually?</span>
        <HugeiconsIcon
          icon={ArrowRight01Icon}
          size={14}
          className={`transition-transform ${open ? "rotate-90" : ""}`}
        />
      </CollapsibleTrigger>
      <CollapsibleContent className="px-4 pb-4 text-sm text-muted-foreground">
        <ol className="mt-1 list-decimal space-y-3 pl-5">
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
            <code className="rounded bg-muted px-1 py-0.5 font-mono text-xs">
              {callbackUrl}
            </code>
            . Enable "Request user authorization (OAuth) during installation".
          </li>
          <li>
            Copy the App's id, client id, client secret, private key, and
            webhook secret into the server's{" "}
            <code className="rounded bg-muted px-1 py-0.5 font-mono text-xs">
              .env
            </code>
            , then install the App on your organization.
          </li>
          <li>
            Restart the stack:{" "}
            <code className="rounded bg-muted px-1 py-0.5 font-mono text-xs">
              docker compose up -d
            </code>
            . Then reload this page.
          </li>
        </ol>
      </CollapsibleContent>
    </Collapsible>
  );
}
