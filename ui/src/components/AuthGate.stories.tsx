/**
 * Onboarding/AuthGate — the outermost gate (`App` wraps everything in it).
 *
 * AuthGate resolves three server facts on mount — `/auth/me`, `/setup/status`,
 * `/auth/config` (all via the shared `authFetchStub`) — and then renders one of
 * several full-screen branches BEFORE the app is reachable:
 *
 *   - deployment not provisioned  → self-setup panel / installation picker /
 *     operator runbook screens (keyed off `setup_state` + `self_setup_*`),
 *   - provisioned but signed out  → "Sign in with GitHub",
 *   - server unreachable          → "Can't reach the server",
 *   - fully ready                 → renders its children (the app shell).
 *
 * Each story drives ONE branch by overriding the relevant endpoints through the
 * `authFetchStub` setters in a decorator, then remounts via `key={ctx.id}` so
 * AuthGate's cached queries never leak across a story switch. A fresh
 * `QueryClient` (retry:false) per render keeps the error branches deterministic.
 */

import type { Meta, StoryObj } from "@storybook/react-vite";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { AuthGate } from "@/components/AuthGate";
import {
  installAuthFetchStub,
  resetStoryAuth,
  setStoryAuthConfig,
  setStoryAuthUser,
  setStoryInstallations,
  setStorySetupStatus,
  setStorySetupStatusError,
} from "@/storybook-mocks/authFetchStub";

installAuthFetchStub();

/** What AuthGate renders once every gate passes — a stand-in for the app. */
function SignedInContent() {
  return (
    <main className="flex min-h-screen items-center justify-center bg-background text-foreground">
      <div className="rounded-xl border border-primary/30 bg-primary/5 px-8 py-6 text-center">
        <p className="text-sm font-semibold">You&apos;re in.</p>
        <p className="mt-1 text-xs text-muted-foreground">
          AuthGate passed — this is where the app shell renders.
        </p>
      </div>
    </main>
  );
}

/** Each story sets up its endpoints here; the setup fn runs before render. */
function AuthGateStory({ setup }: { setup: () => void }) {
  resetStoryAuth();
  setup();
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: 0, gcTime: 0 } },
  });
  return (
    <QueryClientProvider client={queryClient}>
      <AuthGate>
        <SignedInContent />
      </AuthGate>
    </QueryClientProvider>
  );
}

const meta = {
  title: "Onboarding/AuthGate",
  component: AuthGateStory,
  parameters: { layout: "fullscreen" },
  // Remount on every story switch: AuthGate's `/auth/me` query has a 60s
  // staleTime, so without a fresh subtree the previous story's answer survives.
  decorators: [(Story, ctx) => <Story key={ctx.id} />],
} satisfies Meta<typeof AuthGateStory>;

export default meta;
type Story = StoryObj<typeof meta>;

/** Fully provisioned + signed in → AuthGate renders its children. */
export const SignedIn: Story = {
  args: { setup: () => {} },
};

/** Provisioned deployment, no session → the GitHub sign-in screen. */
export const SignInRequired: Story = {
  args: {
    setup: () => {
      setStoryAuthUser(null);
      setStorySetupStatus({
        needs_app_install: false,
        app_credentials_configured: true,
        org_login: "djinnos",
        setup_state: "valid",
      });
    },
  },
};

/**
 * Fresh local deployment with self-setup enabled and the origin-bound launch
 * capability present → the "Connect GitHub" self-setup panel.
 */
export const SelfSetupAvailable: Story = {
  args: {
    setup: () => {
      setStoryAuthUser(null);
      setStorySetupStatus({
        needs_app_install: true,
        app_credentials_configured: false,
        org_login: null,
        setup_state: "unconfigured",
      });
      setStoryAuthConfig({
        configured: false,
        missing: ["GITHUB_APP_ID"],
        setup_doc_url: "https://www.djinnai.io/docs/setup",
        self_setup_available: true,
        setup_launch_available: true,
      });
    },
  },
};

/**
 * Self-setup enabled but this browser origin can't launch it → fall back to
 * the "Open the GitHub setup link" one-time-URL guidance.
 */
export const SelfSetupLinkOnly: Story = {
  args: {
    setup: () => {
      setStoryAuthUser(null);
      setStorySetupStatus({
        needs_app_install: true,
        app_credentials_configured: false,
        org_login: null,
        setup_state: "unconfigured",
      });
      setStoryAuthConfig({
        configured: false,
        missing: ["GITHUB_APP_ID"],
        setup_doc_url: "https://www.djinnai.io/docs/setup",
        self_setup_available: true,
        setup_launch_available: false,
      });
    },
  },
};

/**
 * App credentials present (Secret mounted) but no installation bound yet →
 * the in-UI installation picker with two orgs to choose from.
 */
export const InstallationPicker: Story = {
  args: {
    setup: () => {
      setStoryAuthUser(null);
      setStorySetupStatus({
        needs_app_install: true,
        app_credentials_configured: true,
        org_login: null,
        setup_state: "unconfigured",
      });
      setStoryInstallations([
        {
          installationId: 42_000_001,
          accountLogin: "djinnos",
          accountId: 1,
          accountType: "Organization",
          repositorySelection: "all",
          htmlUrl: "https://github.com/organizations/djinnos/settings/installations/42000001",
        },
        {
          installationId: 42_000_002,
          accountLogin: "fernando",
          accountId: 2,
          accountType: "User",
          repositorySelection: "selected",
          htmlUrl: "https://github.com/settings/installations/42000002",
        },
      ]);
    },
  },
};

/** App credentials present, no installation bound, none available → empty picker. */
export const InstallationPickerEmpty: Story = {
  args: {
    setup: () => {
      setStoryAuthUser(null);
      setStorySetupStatus({
        needs_app_install: true,
        app_credentials_configured: true,
        org_login: null,
        setup_state: "unconfigured",
      });
      setStoryInstallations([]);
    },
  },
};

/** No credentials, self-setup disabled → the operator runbook screen. */
export const AppNotConfigured: Story = {
  args: {
    setup: () => {
      setStoryAuthUser(null);
      setStorySetupStatus({
        needs_app_install: true,
        app_credentials_configured: false,
        org_login: null,
        setup_state: "unconfigured",
      });
      setStoryAuthConfig({
        configured: false,
        missing: ["GITHUB_APP_ID", "GITHUB_APP_PRIVATE_KEY"],
        setup_doc_url: "https://www.djinnai.io/docs/setup",
        self_setup_available: false,
      });
    },
  },
};

/** Mounted Secret is malformed → the fatal "Secret is invalid" screen. */
export const InvalidSecret: Story = {
  args: {
    setup: () => {
      setStoryAuthUser(null);
      setStorySetupStatus({
        needs_app_install: true,
        app_credentials_configured: false,
        org_login: null,
        setup_state: "invalid_secret",
        setup_error: "GITHUB_APP_PRIVATE_KEY is not a valid PEM document.",
      });
    },
  },
};

/** Persisted credentials can't be decrypted → the recovery screen. */
export const CredentialsUnrecoverable: Story = {
  args: {
    setup: () => {
      setStoryAuthUser(null);
      setStorySetupStatus({
        needs_app_install: true,
        app_credentials_configured: false,
        org_login: null,
        setup_state: "unrecoverable",
        credentials_unrecoverable: true,
        setup_error: "The persisted credential store could not be decrypted.",
      });
    },
  },
};

/** Setup mid-flight / retryable error → the "setup in progress" screen. */
export const SetupInProgress: Story = {
  args: {
    setup: () => {
      setStoryAuthUser(null);
      setStorySetupStatus({
        needs_app_install: true,
        app_credentials_configured: false,
        org_login: null,
        setup_state: "unconfigured",
        setup_retryable: true,
        setup_error: "Waiting for the GitHub App manifest callback to complete.",
      });
    },
  },
};

/** `/setup/status` fails outright → the "can't reach the server" screen. */
export const ServerUnreachable: Story = {
  args: {
    setup: () => {
      setStorySetupStatusError(503);
    },
  },
};
