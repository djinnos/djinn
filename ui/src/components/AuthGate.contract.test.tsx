import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, waitFor } from "@/test/test-utils";
import { AuthGate } from "@/components/AuthGate";
import { fetchAuthConfig, fetchCurrentUser } from "@/api/auth";

vi.mock("@/api/auth", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/api/auth")>();
  return {
    ...actual,
    // Keep the real fetchSetupStatus export. These two unrelated requests are
    // mocked so each test exercises exactly one raw /setup/status response.
    fetchCurrentUser: vi.fn(),
    fetchAuthConfig: vi.fn(),
  };
});

type BackendSetupState =
  | "unconfigured"
  | "valid_secret"
  | "valid_persisted"
  | "invalid_secret"
  | "credentials_unrecoverable";

interface BackendSetupStatus {
  needs_app_install: boolean;
  app_credentials_configured: boolean;
  org_login: string | null;
  credential_source: "secret" | "persisted" | null;
  setup_state: BackendSetupState;
  setup_error: string | null;
  setup_retryable: boolean;
  credentials_unrecoverable: boolean;
}

function authConfig(
  selfSetupAvailable: boolean,
  setupLaunchAvailable = selfSetupAvailable,
) {
  return {
    configured: false,
    missing: ["GITHUB_APP_ID"],
    setupDocUrl: "https://www.djinnai.io/docs/setup",
    selfSetupAvailable,
    setupLaunchAvailable,
  };
}

function stubSetupStatus(payload: BackendSetupStatus) {
  const fetchMock = vi.fn(async (input: string | URL | Request) => {
    const url =
      typeof input === "string"
        ? input
        : input instanceof URL
          ? input.toString()
          : input.url;
    if (!url.endsWith("/setup/status")) {
      throw new Error(`Unexpected fetch in AuthGate contract test: ${url}`);
    }
    return {
      ok: true,
      status: 200,
      json: vi.fn().mockResolvedValue(payload),
    } as unknown as Response;
  });
  vi.stubGlobal("fetch", fetchMock);
  return fetchMock;
}

describe("AuthGate backend setup-status contract", () => {
  beforeEach(() => {
    vi.mocked(fetchCurrentUser).mockReset();
    vi.mocked(fetchAuthConfig).mockReset();
    vi.mocked(fetchCurrentUser).mockResolvedValue(null);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("maps an invalid Secret payload to the fatal Secret screen", async () => {
    const fetchMock = stubSetupStatus({
      needs_app_install: true,
      app_credentials_configured: false,
      org_login: null,
      credential_source: null,
      setup_state: "invalid_secret",
      setup_error: "GITHUB_APP_CLIENT_ID is empty",
      setup_retryable: false,
      credentials_unrecoverable: false,
    });
    // A broken mounted Secret must win over the otherwise available self-setup
    // route; silently falling through would hide an operator error.
    vi.mocked(fetchAuthConfig).mockResolvedValue(authConfig(true));

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
      screen.getByText("GITHUB_APP_CLIENT_ID is empty"),
    ).toBeInTheDocument();
    expect(screen.queryByText("Connect GitHub")).not.toBeInTheDocument();
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it("maps an undecryptable persisted payload to the recovery screen", async () => {
    stubSetupStatus({
      needs_app_install: true,
      app_credentials_configured: false,
      org_login: null,
      credential_source: null,
      setup_state: "credentials_unrecoverable",
      setup_error: "Persisted credentials cannot be decrypted",
      setup_retryable: false,
      credentials_unrecoverable: true,
    });
    vi.mocked(fetchAuthConfig).mockResolvedValue(authConfig(true));

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
      screen.getByText("Persisted credentials cannot be decrypted"),
    ).toBeInTheDocument();
    expect(
      screen.queryByText("GitHub App not configured"),
    ).not.toBeInTheDocument();
  });

  it("maps an unconfigured payload to the manual runbook screen", async () => {
    stubSetupStatus({
      needs_app_install: true,
      app_credentials_configured: false,
      org_login: null,
      credential_source: null,
      setup_state: "unconfigured",
      setup_error: null,
      setup_retryable: false,
      credentials_unrecoverable: false,
    });
    vi.mocked(fetchAuthConfig).mockResolvedValue(authConfig(false));

    render(
      <AuthGate>
        <div>signed-in app</div>
      </AuthGate>,
    );

    await waitFor(() => {
      expect(screen.getByText("GitHub App not configured")).toBeInTheDocument();
    });
    expect(
      screen.queryByText("Stored credentials cannot be recovered"),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByText("GitHub App Secret is invalid"),
    ).not.toBeInTheDocument();
  });

  it("maps an unconfigured self-setup payload to the direct GitHub setup action", async () => {
    stubSetupStatus({
      needs_app_install: true,
      app_credentials_configured: false,
      org_login: null,
      credential_source: null,
      setup_state: "unconfigured",
      setup_error: null,
      setup_retryable: false,
      credentials_unrecoverable: false,
    });
    vi.mocked(fetchAuthConfig).mockResolvedValue(authConfig(true));

    render(
      <AuthGate>
        <div>signed-in app</div>
      </AuthGate>,
    );

    await waitFor(() => {
      expect(
        screen.getByRole("button", { name: "Continue with GitHub" }),
      ).toBeInTheDocument();
    });
    expect(
      screen.getByRole("form", { name: "Start GitHub App setup" }),
    ).toHaveAttribute("action", "/auth/github/setup-start");
    expect(screen.queryByText("GitHub App not configured")).not.toBeInTheDocument();
  });

  it("maps setup_retryable to the retry screen without losing setup_error", async () => {
    stubSetupStatus({
      needs_app_install: true,
      app_credentials_configured: false,
      org_login: null,
      credential_source: null,
      setup_state: "unconfigured",
      setup_error: "GitHub manifest exchange timed out",
      setup_retryable: true,
      credentials_unrecoverable: false,
    });
    vi.mocked(fetchAuthConfig).mockResolvedValue(authConfig(true));

    render(
      <AuthGate>
        <div>signed-in app</div>
      </AuthGate>,
    );

    await waitFor(() => {
      expect(screen.getByText("GitHub App setup in progress")).toBeInTheDocument();
    });
    expect(
      screen.getByText("GitHub manifest exchange timed out"),
    ).toBeInTheDocument();
    expect(screen.queryByText("Connect GitHub")).not.toBeInTheDocument();
  });
});
