import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, waitFor } from "@/test/test-utils";
import { AuthGate } from "@/components/AuthGate";
import {
  fetchCurrentUser,
  fetchInstallations,
  fetchSetupStatus,
} from "@/api/auth";

vi.mock("@/api/auth", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/api/auth")>();
  return {
    ...actual,
    fetchCurrentUser: vi.fn(),
    fetchSetupStatus: vi.fn(),
    fetchInstallations: vi.fn(),
    selectInstallation: vi.fn(),
  };
});

describe("AuthGate", () => {
  beforeEach(() => {
    vi.mocked(fetchCurrentUser).mockReset();
    vi.mocked(fetchSetupStatus).mockReset();
    vi.mocked(fetchInstallations).mockReset();
  });

  it("renders the operator runbook screen when App credentials are missing", async () => {
    vi.mocked(fetchCurrentUser).mockResolvedValue(null);
    vi.mocked(fetchSetupStatus).mockResolvedValue({
      needsAppInstall: true,
      appCredentialsConfigured: false,
      orgLogin: null,
    });
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
    vi.mocked(fetchCurrentUser).mockResolvedValue(null);
    vi.mocked(fetchSetupStatus).mockResolvedValue({
      needsAppInstall: true,
      appCredentialsConfigured: true,
      orgLogin: null,
    });
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
    vi.mocked(fetchCurrentUser).mockResolvedValue(null);
    vi.mocked(fetchSetupStatus).mockResolvedValue({
      needsAppInstall: false,
      appCredentialsConfigured: true,
      orgLogin: "acme",
    });
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
    });
    vi.mocked(fetchSetupStatus).mockResolvedValue({
      needsAppInstall: false,
      appCredentialsConfigured: true,
      orgLogin: "acme",
    });
    render(
      <AuthGate>
        <div>signed-in app</div>
      </AuthGate>,
    );

    await waitFor(() => {
      expect(screen.getByText("signed-in app")).toBeInTheDocument();
    });
  });
});
