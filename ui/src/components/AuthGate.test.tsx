import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, waitFor } from "@/test/test-utils";
import { AuthGate } from "@/components/AuthGate";
import {
  fetchAuthConfig,
  fetchCurrentUser,
  fetchInstallations,
  fetchSetupStatus,
  type AuthConfig,
  type SetupStatus,
} from "@/api/auth";

const SETUP_DOC_URL = "https://www.djinnai.io/docs/setup";

vi.mock("@/api/auth", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/api/auth")>();
  return {
    ...actual,
    fetchCurrentUser: vi.fn(),
    fetchSetupStatus: vi.fn(),
    fetchAuthConfig: vi.fn(),
    fetchInstallations: vi.fn(),
    selectInstallation: vi.fn(),
  };
});

/** A legacy / minimal setup-status payload with the new fields defaulted. */
function baseSetupStatus(overrides: Partial<SetupStatus> = {}): SetupStatus {
  return {
    needsAppInstall: true,
    appCredentialsConfigured: false,
    orgLogin: null,
    credentialSource: null,
    setupState: "unconfigured",
    setupError: null,
    setupRetryable: false,
    credentialsUnrecoverable: false,
    ...overrides,
  };
}

/**
 * A minimal auth-config payload with selfSetupAvailable defaulted.
 * Only `selfSetupAvailable` is used by AuthGate's rendering logic; the
 * remaining fields are required by the `AuthConfig` type.
 */
function baseAuthConfig(selfSetupAvailable = false): AuthConfig {
  return {
    configured: !selfSetupAvailable,
    missing: selfSetupAvailable ? ["GITHUB_APP_CLIENT_ID"] : [],
    setupDocUrl: SETUP_DOC_URL,
    selfSetupAvailable,
  };
}

describe("AuthGate", () => {
  beforeEach(() => {
    vi.mocked(fetchCurrentUser).mockReset();
    vi.mocked(fetchSetupStatus).mockReset();
    vi.mocked(fetchAuthConfig).mockReset();
    vi.mocked(fetchInstallations).mockReset();
    // Default mocks: unauthenticated, unconfigured, self-setup off.
    vi.mocked(fetchCurrentUser).mockResolvedValue(null);
    vi.mocked(fetchSetupStatus).mockResolvedValue(baseSetupStatus());
    vi.mocked(fetchAuthConfig).mockResolvedValue(baseAuthConfig(false));
  });

  // ─── Existing behavior preserved ────────────────────────────────────────

  it("renders the operator runbook screen when App credentials are missing", async () => {
    vi.mocked(fetchSetupStatus).mockResolvedValue(baseSetupStatus());
    render(
      <AuthGate>
        <div>signed-in app</div>
      </AuthGate>,
    );

    await waitFor(() => {
      expect(screen.getByText("GitHub App not configured")).toBeInTheDocument();
    });
  });

  it("renders the installation picker when App is configured but no binding exists", async () => {
    vi.mocked(fetchSetupStatus).mockResolvedValue(
      baseSetupStatus({
        needsAppInstall: true,
        appCredentialsConfigured: true,
        orgLogin: null,
        setupState: null,
      }),
    );
    vi.mocked(fetchInstallations).mockResolvedValue([
      {
        installationId: 99,
        accountLogin: "acme",
        accountId: 1,
        accountType: "Organization",
        repositorySelection: "all",
        htmlUrl: "https://github.com/organizations/acme/settings/installations/99",
      },
    ]);

    render(
      <AuthGate>
        <div>signed-in app</div>
      </AuthGate>,
    );

    await waitFor(() => {
      expect(screen.getByText("Pick a GitHub installation")).toBeInTheDocument();
    });
    expect(screen.getByText("acme")).toBeInTheDocument();
  });

  it("renders the GitHub sign-in button when fully configured but unauthed", async () => {
    vi.mocked(fetchSetupStatus).mockResolvedValue(
      baseSetupStatus({
        needsAppInstall: false,
        appCredentialsConfigured: true,
        orgLogin: "acme",
        setupState: "valid",
      }),
    );
    render(
      <AuthGate>
        <div>signed-in app</div>
      </AuthGate>,
    );

    await waitFor(() => {
      expect(
        screen.getByRole("button", { name: /Sign in with GitHub/i }),
      ).toBeInTheDocument();
    });
  });

  it("renders children when authed and configured", async () => {
    vi.mocked(fetchCurrentUser).mockResolvedValue({
      id: "1",
      login: "alice",
      name: null,
      avatarUrl: null,
      isAdmin: false,
      role: "engineer",
    });
    vi.mocked(fetchSetupStatus).mockResolvedValue(
      baseSetupStatus({
        needsAppInstall: false,
        appCredentialsConfigured: true,
        orgLogin: "acme",
        setupState: "valid",
      }),
    );
    render(
      <AuthGate>
        <div>signed-in app</div>
      </AuthGate>,
    );

    await waitFor(() => {
      expect(screen.getByText("signed-in app")).toBeInTheDocument();
    });
  });

  // ─── Self-setup guidance ────────────────────────────────────────────────

  it("renders the self-setup guidance screen when self-setup is available and credentials are missing", async () => {
    vi.mocked(fetchSetupStatus).mockResolvedValue(baseSetupStatus());
    vi.mocked(fetchAuthConfig).mockResolvedValue(baseAuthConfig(true));

    render(
      <AuthGate>
        <div>signed-in app</div>
      </AuthGate>,
    );

    await waitFor(() => {
      expect(screen.getByText("Set up GitHub access")).toBeInTheDocument();
    });
    // Should mention the boot-log setup URL flow.
    expect(
      screen.getByText(/setup URL printed in the server boot logs/i),
    ).toBeInTheDocument();
  });

  it("does not show a self-setup CTA when self-setup is disabled and credentials are missing", async () => {
    vi.mocked(fetchSetupStatus).mockResolvedValue(baseSetupStatus());
    vi.mocked(fetchAuthConfig).mockResolvedValue(baseAuthConfig(false));

    render(
      <AuthGate>
        <div>signed-in app</div>
      </AuthGate>,
    );

    await waitFor(() => {
      expect(screen.getByText("GitHub App not configured")).toBeInTheDocument();
    });
    expect(screen.queryByText("Set up GitHub access")).not.toBeInTheDocument();
  });

  it("falls back to the runbook screen when self-setup is available but the App is configured", async () => {
    // Self-setup only matters when credentials are missing. If the App is
    // configured (install picker path), we don't show setup guidance.
    vi.mocked(fetchSetupStatus).mockResolvedValue(
      baseSetupStatus({
        needsAppInstall: true,
        appCredentialsConfigured: true,
        setupState: null,
      }),
    );
    vi.mocked(fetchAuthConfig).mockResolvedValue(baseAuthConfig(true));
    vi.mocked(fetchInstallations).mockResolvedValue([
      {
        installationId: 1,
        accountLogin: "acme",
        accountId: 1,
        accountType: "Organization",
        repositorySelection: "all",
        htmlUrl: "https://github.com/organizations/acme/settings/installations/1",
      },
    ]);

    render(
      <AuthGate>
        <div>signed-in app</div>
      </AuthGate>,
    );

    await waitFor(() => {
      expect(screen.getByText("Pick a GitHub installation")).toBeInTheDocument();
    });
    expect(screen.queryByText("Set up GitHub access")).not.toBeInTheDocument();
  });

  // ─── Secret / token non-exposure ────────────────────────────────────────

  it("never asks the operator to paste GitHub App secrets or setup tokens", async () => {
    vi.mocked(fetchSetupStatus).mockResolvedValue(baseSetupStatus());
    vi.mocked(fetchAuthConfig).mockResolvedValue(baseAuthConfig(true));

    render(
      <AuthGate>
        <div>signed-in app</div>
      </AuthGate>,
    );

    await waitFor(() => {
      expect(screen.getByText("Set up GitHub access")).toBeInTheDocument();
    });

    // The rendered guidance should explicitly tell the user they don't need
    // to paste secrets or tokens.
    const bodyText = document.body.textContent ?? "";
    expect(bodyText).toMatch(/never need to paste secrets or tokens/i);
    // No password inputs or token prompts in the self-setup screen.
    expect(screen.queryByPlaceholderText(/paste.*secret/i)).not.toBeInTheDocument();
    expect(screen.queryByPlaceholderText(/paste.*token/i)).not.toBeInTheDocument();
    expect(screen.queryByLabelText(/private key/i)).not.toBeInTheDocument();
  });

  // ─── Fatal Secret error ─────────────────────────────────────────────────

  it("renders the fatal Secret error screen for invalid_secret state", async () => {
    vi.mocked(fetchSetupStatus).mockResolvedValue(
      baseSetupStatus({
        setupState: "invalid_secret",
        setupError: "GITHUB_APP_CLIENT_ID is empty",
      }),
    );
    vi.mocked(fetchAuthConfig).mockResolvedValue(baseAuthConfig(true));

    render(
      <AuthGate>
        <div>signed-in app</div>
      </AuthGate>,
    );

    await waitFor(() => {
      expect(
        screen.getByText("GitHub App Secret is invalid"),
      ).toBeInTheDocument();
    });
    expect(
      screen.getByText(/GITHUB_APP_CLIENT_ID is empty/i),
    ).toBeInTheDocument();
    // Fatal Secret must NOT silently fall back to self-setup CTA.
    expect(screen.queryByText("Set up GitHub access")).not.toBeInTheDocument();
  });

  // ─── Credentials unrecoverable recovery ─────────────────────────────────

  it("renders the recovery screen for credentials_unrecoverable state", async () => {
    vi.mocked(fetchSetupStatus).mockResolvedValue(
      baseSetupStatus({
        setupState: "unrecoverable",
        setupError: "Persisted credentials cannot be decrypted",
        credentialsUnrecoverable: true,
      }),
    );
    vi.mocked(fetchAuthConfig).mockResolvedValue(baseAuthConfig(true));

    render(
      <AuthGate>
        <div>signed-in app</div>
      </AuthGate>,
    );

    await waitFor(() => {
      expect(
        screen.getByText("Stored credentials cannot be recovered"),
      ).toBeInTheDocument();
    });
    expect(
      screen.getByText(/Persisted credentials cannot be decrypted/i),
    ).toBeInTheDocument();
    // Recovery screen must not show a generic missing-config message.
    expect(
      screen.queryByText("GitHub App not configured"),
    ).not.toBeInTheDocument();
  });

  it("renders the recovery screen when credentialsUnrecoverable flag is set without explicit state", async () => {
    vi.mocked(fetchSetupStatus).mockResolvedValue(
      baseSetupStatus({
        setupState: null,
        credentialsUnrecoverable: true,
        setupError: "Vault key mismatch",
      }),
    );

    render(
      <AuthGate>
        <div>signed-in app</div>
      </AuthGate>,
    );

    await waitFor(() => {
      expect(
        screen.getByText("Stored credentials cannot be recovered"),
      ).toBeInTheDocument();
    });
    expect(screen.getByText(/Vault key mismatch/i)).toBeInTheDocument();
  });

  // ─── Setup retry / progress ─────────────────────────────────────────────

  it("renders the setup progress / retry screen for retryable errors", async () => {
    vi.mocked(fetchSetupStatus).mockResolvedValue(
      baseSetupStatus({
        setupState: null,
        setupError: "Temporary GitHub API failure",
        setupRetryable: true,
      }),
    );
    vi.mocked(fetchAuthConfig).mockResolvedValue(baseAuthConfig(true));

    render(
      <AuthGate>
        <div>signed-in app</div>
      </AuthGate>,
    );

    await waitFor(() => {
      expect(
        screen.getByText("GitHub App setup in progress"),
      ).toBeInTheDocument();
    });
    expect(
      screen.getByText(/Temporary GitHub API failure/i),
    ).toBeInTheDocument();
    expect(
      screen.getByText(/setup URL from the boot logs/i),
    ).toBeInTheDocument();
  });

  // ─── Deterministic copy / branch isolation ──────────────────────────────

  it("renders deterministic distinct copy for each fatal/recovery/setup state", async () => {
    const headings = [
      "GitHub App Secret is invalid",
      "Stored credentials cannot be recovered",
      "GitHub App setup in progress",
      "Set up GitHub access",
      "GitHub App not configured",
    ];

    for (const heading of headings) {
      // Render once per state — use a custom container each time.
      vi.mocked(fetchSetupStatus).mockReset();
      vi.mocked(fetchAuthConfig).mockReset();
      vi.mocked(fetchCurrentUser).mockResolvedValue(null);

      // Pick the setup-status that produces this heading.
      switch (heading) {
        case "GitHub App Secret is invalid":
          vi.mocked(fetchSetupStatus).mockResolvedValue(
            baseSetupStatus({ setupState: "invalid_secret" }),
          );
          vi.mocked(fetchAuthConfig).mockResolvedValue(baseAuthConfig(true));
          break;
        case "Stored credentials cannot be recovered":
          vi.mocked(fetchSetupStatus).mockResolvedValue(
            baseSetupStatus({
              setupState: "unrecoverable",
              credentialsUnrecoverable: true,
            }),
          );
          vi.mocked(fetchAuthConfig).mockResolvedValue(baseAuthConfig(true));
          break;
        case "GitHub App setup in progress":
          vi.mocked(fetchSetupStatus).mockResolvedValue(
            baseSetupStatus({ setupRetryable: true }),
          );
          vi.mocked(fetchAuthConfig).mockResolvedValue(baseAuthConfig(true));
          break;
        case "Set up GitHub access":
          vi.mocked(fetchSetupStatus).mockResolvedValue(baseSetupStatus());
          vi.mocked(fetchAuthConfig).mockResolvedValue(baseAuthConfig(true));
          break;
        case "GitHub App not configured":
          vi.mocked(fetchSetupStatus).mockResolvedValue(baseSetupStatus());
          vi.mocked(fetchAuthConfig).mockResolvedValue(baseAuthConfig(false));
          break;
      }

      render(
        <AuthGate>
          <div>signed-in app</div>
        </AuthGate>,
      );

      await waitFor(() => {
        expect(screen.getByText(heading)).toBeInTheDocument();
      });
      // Clean up DOM for the next iteration.
      document.body.innerHTML = "";
    }
  });
});
