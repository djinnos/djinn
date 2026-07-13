import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, userEvent, waitFor } from "@/test/test-utils";
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
 * A minimal auth-config payload. Tests that enable self-setup default to a
 * canonical origin with a launch capability; pass `false` explicitly to
 * exercise the secure one-time-URL fallback.
 */
function baseAuthConfig(
  selfSetupAvailable = false,
  setupLaunchAvailable = selfSetupAvailable,
): AuthConfig {
  return {
    configured: !selfSetupAvailable,
    missing: selfSetupAvailable ? ["GITHUB_APP_CLIENT_ID"] : [],
    setupDocUrl: SETUP_DOC_URL,
    selfSetupAvailable,
    setupLaunchAvailable,
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

  afterEach(() => {
    window.history.replaceState({}, "", "/");
    vi.restoreAllMocks();
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

  it("renders a server error instead of signed-out UI when /auth/me fails", async () => {
    vi.mocked(fetchCurrentUser).mockRejectedValue(new Error("auth service unavailable"));
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
      expect(screen.getByText("Can't reach the server")).toBeInTheDocument();
    });
    expect(screen.getByText("auth service unavailable")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /Sign in with GitHub/i })).not.toBeInTheDocument();
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

  // ─── Self-setup launch ──────────────────────────────────────────────────

  it("renders a direct GitHub setup action when self-setup is available and credentials are missing", async () => {
    vi.mocked(fetchSetupStatus).mockResolvedValue(baseSetupStatus());
    vi.mocked(fetchAuthConfig).mockResolvedValue(baseAuthConfig(true));

    render(
      <AuthGate>
        <div>signed-in app</div>
      </AuthGate>,
    );

    await waitFor(() => {
      expect(screen.getByRole("heading", { name: "Connect GitHub" })).toBeInTheDocument();
    });

    const form = screen.getByRole("form", { name: "Start GitHub App setup" });
    expect(form).toHaveAttribute("action", "/auth/github/setup-start");
    expect(form).toHaveAttribute("method", "post");
    expect(
      screen.getByRole("button", { name: "Continue with GitHub" }),
    ).toHaveAttribute("type", "submit");
    expect(screen.getByText("Review permissions")).toBeInTheDocument();
    expect(screen.getByText("Choose repositories")).toBeInTheDocument();
    expect(screen.getByText("Return to Djinn")).toBeInTheDocument();

    const troubleshooting = screen.getByText("Having trouble?").closest("details");
    expect(troubleshooting).not.toHaveAttribute("open");
    expect(troubleshooting).toContainElement(
      screen.getByText(/setup URL from the server boot logs/i),
    );
    expect(troubleshooting).toContainElement(
      screen.getByText(/restart the local server to generate a new URL/i),
    );
  });

  it("refreshes the launch capability immediately before native form submission", async () => {
    let resolveRefresh!: (config: AuthConfig) => void;
    const refresh = new Promise<AuthConfig>((resolve) => {
      resolveRefresh = resolve;
    });
    vi.mocked(fetchSetupStatus).mockResolvedValue(baseSetupStatus());
    vi.mocked(fetchAuthConfig)
      .mockResolvedValueOnce(baseAuthConfig(true))
      .mockImplementationOnce(() => refresh);
    const submitSpy = vi
      .spyOn(HTMLFormElement.prototype, "submit")
      .mockImplementation(() => undefined);

    render(
      <AuthGate>
        <div>signed-in app</div>
      </AuthGate>,
    );

    const button = await screen.findByRole("button", {
      name: "Continue with GitHub",
    });
    await userEvent.click(button);

    expect(fetchAuthConfig).toHaveBeenCalledTimes(2);
    expect(
      screen.getByRole("button", { name: "Refreshing secure access…" }),
    ).toBeDisabled();
    expect(
      screen.getByRole("form", { name: "Start GitHub App setup" }),
    ).toHaveAttribute("aria-busy", "true");
    expect(submitSpy).not.toHaveBeenCalled();

    resolveRefresh(baseAuthConfig(true));

    await waitFor(() => {
      expect(submitSpy).toHaveBeenCalledTimes(1);
    });
  });

  it("shows an accessible error and does not submit when capability refresh fails", async () => {
    vi.mocked(fetchSetupStatus).mockResolvedValue(baseSetupStatus());
    vi.mocked(fetchAuthConfig)
      .mockResolvedValueOnce(baseAuthConfig(true))
      .mockRejectedValueOnce(new Error("server unavailable"));
    const submitSpy = vi
      .spyOn(HTMLFormElement.prototype, "submit")
      .mockImplementation(() => undefined);

    render(
      <AuthGate>
        <div>signed-in app</div>
      </AuthGate>,
    );

    await userEvent.click(
      await screen.findByRole("button", { name: "Continue with GitHub" }),
    );

    expect(await screen.findByRole("alert")).toHaveTextContent(
      /Could not refresh secure setup access/i,
    );
    expect(submitSpy).not.toHaveBeenCalled();
    expect(
      screen.getByRole("button", { name: "Continue with GitHub" }),
    ).toBeEnabled();
  });

  it("does not submit when refreshed config revokes setup launch availability", async () => {
    vi.mocked(fetchSetupStatus).mockResolvedValue(baseSetupStatus());
    vi.mocked(fetchAuthConfig)
      .mockResolvedValueOnce(baseAuthConfig(true))
      .mockResolvedValueOnce(baseAuthConfig(true, false));
    const submitSpy = vi
      .spyOn(HTMLFormElement.prototype, "submit")
      .mockImplementation(() => undefined);

    render(
      <AuthGate>
        <div>signed-in app</div>
      </AuthGate>,
    );

    await userEvent.click(
      await screen.findByRole("button", { name: "Continue with GitHub" }),
    );

    expect(await screen.findByRole("alert")).toHaveTextContent(
      /Secure setup is no longer available from this browser address/i,
    );
    expect(submitSpy).not.toHaveBeenCalled();
  });

  it("explains an expired setup redirect after config refresh and keeps the CTA available", async () => {
    window.history.replaceState({}, "", "/?setup=expired");
    vi.mocked(fetchSetupStatus).mockResolvedValue(baseSetupStatus());
    vi.mocked(fetchAuthConfig).mockResolvedValue(baseAuthConfig(true));

    render(
      <AuthGate>
        <div>signed-in app</div>
      </AuthGate>,
    );

    const notice = await screen.findByRole("status");
    expect(notice).toHaveTextContent(/setup access expired/i);
    expect(notice).toHaveTextContent(/has been refreshed/i);
    expect(
      screen.getByRole("button", { name: "Continue with GitHub" }),
    ).toBeEnabled();
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
    expect(screen.queryByText("Connect GitHub")).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "Continue with GitHub" }),
    ).not.toBeInTheDocument();
  });

  it("shows the secure log-URL fallback without a fake CTA when launch capability is unavailable", async () => {
    vi.mocked(fetchSetupStatus).mockResolvedValue(baseSetupStatus());
    vi.mocked(fetchAuthConfig).mockResolvedValue(baseAuthConfig(true, false));

    render(
      <AuthGate>
        <div>signed-in app</div>
      </AuthGate>,
    );

    await waitFor(() => {
      expect(
        screen.getByRole("heading", { name: "Open the GitHub setup link" }),
      ).toBeInTheDocument();
    });
    expect(
      screen.getByText(/one-time URL in the server boot logs/i),
    ).toBeInTheDocument();
    expect(
      screen.getByText(/restart the local server to generate a new URL/i),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "Continue with GitHub" }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("form", { name: "Start GitHub App setup" }),
    ).not.toBeInTheDocument();
    expect(document.querySelectorAll("input")).toHaveLength(0);
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
    expect(screen.queryByText("Connect GitHub")).not.toBeInTheDocument();
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
      expect(screen.getByText("Connect GitHub")).toBeInTheDocument();
    });

    // The page explains that no secret copying is required, and the launch
    // form itself carries no token or credential fields.
    const bodyText = document.body.textContent ?? "";
    expect(bodyText).toMatch(/No tokens or private keys to copy into Djinn/i);
    const form = screen.getByRole("form", { name: "Start GitHub App setup" });
    expect(form.querySelectorAll("input")).toHaveLength(0);
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
    expect(screen.queryByText("Connect GitHub")).not.toBeInTheDocument();
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
      "Connect GitHub",
      "Open the GitHub setup link",
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
        case "Connect GitHub":
          vi.mocked(fetchSetupStatus).mockResolvedValue(baseSetupStatus());
          vi.mocked(fetchAuthConfig).mockResolvedValue(baseAuthConfig(true));
          break;
        case "Open the GitHub setup link":
          vi.mocked(fetchSetupStatus).mockResolvedValue(baseSetupStatus());
          vi.mocked(fetchAuthConfig).mockResolvedValue(
            baseAuthConfig(true, false),
          );
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
