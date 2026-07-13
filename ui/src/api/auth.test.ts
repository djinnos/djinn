import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { fetchAuthConfig, fetchSetupStatus } from "@/api/auth";

// Mock getBaseUrl so the fetch URL is deterministic and doesn't depend on
// window.location in jsdom.
vi.mock("@/api/serverUrl", () => ({
  getBaseUrl: vi.fn(() => "https://djinn.example.test"),
}));

/**
 * Stub global `fetch` to return a JSON response.
 */
function mockFetchOk(body: unknown) {
  const json = vi.fn().mockResolvedValue(body);
  const fetchMock = vi.fn().mockResolvedValue({
    ok: true,
    status: 200,
    json,
  });
  vi.stubGlobal("fetch", fetchMock);
  return { fetchMock, json };
}

beforeEach(() => {
  vi.unstubAllGlobals();
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("fetchAuthConfig", () => {
  it("maps self_setup_available=true from the server", async () => {
    mockFetchOk({
      configured: false,
      missing: ["GITHUB_APP_CLIENT_ID"],
      setup_doc_url: "https://example.test/setup",
      self_setup_available: true,
      setup_launch_available: true,
    });

    const config = await fetchAuthConfig();

    expect(config).toEqual({
      configured: false,
      missing: ["GITHUB_APP_CLIENT_ID"],
      setupDocUrl: "https://example.test/setup",
      selfSetupAvailable: true,
      setupLaunchAvailable: true,
    });
  });

  it("defaults selfSetupAvailable to false for legacy servers without the field", async () => {
    // Legacy server response — no self_setup_available key at all.
    mockFetchOk({
      configured: false,
      missing: ["GITHUB_APP_PRIVATE_KEY"],
      setup_doc_url: "https://example.test/setup",
    });

    const config = await fetchAuthConfig();

    expect(config.configured).toBe(false);
    expect(config.selfSetupAvailable).toBe(false);
    expect(config.setupLaunchAvailable).toBe(false);
  });

  it("defaults setupLaunchAvailable to false when an older server only advertises self-setup", async () => {
    mockFetchOk({
      configured: false,
      missing: ["GITHUB_APP_PRIVATE_KEY"],
      setup_doc_url: "https://example.test/setup",
      self_setup_available: true,
    });

    const config = await fetchAuthConfig();

    expect(config.selfSetupAvailable).toBe(true);
    expect(config.setupLaunchAvailable).toBe(false);
  });

  it("preserves selfSetupAvailable=false when the server explicitly sends false", async () => {
    mockFetchOk({
      configured: true,
      missing: [],
      setup_doc_url: "https://example.test/setup",
      self_setup_available: false,
      setup_launch_available: false,
    });

    const config = await fetchAuthConfig();

    expect(config.selfSetupAvailable).toBe(false);
    expect(config.setupLaunchAvailable).toBe(false);
    expect(config.configured).toBe(true);
  });
});

describe("fetchSetupStatus", () => {
  it("preserves existing fields and defaults new fields for legacy servers", async () => {
    // Legacy server — only the original three fields.
    mockFetchOk({
      needs_app_install: true,
      app_credentials_configured: false,
      org_login: null,
    });

    const status = await fetchSetupStatus();

    expect(status.needsAppInstall).toBe(true);
    expect(status.appCredentialsConfigured).toBe(false);
    expect(status.orgLogin).toBeNull();
    // New credential-source/recovery fields default to safe values.
    expect(status.credentialSource).toBeNull();
    expect(status.setupState).toBeNull();
    expect(status.setupError).toBeNull();
    expect(status.setupRetryable).toBe(false);
    expect(status.credentialsUnrecoverable).toBe(false);
  });

  it("maps credential_source=secret from the server", async () => {
    mockFetchOk({
      needs_app_install: false,
      app_credentials_configured: true,
      org_login: "acme",
      credential_source: "secret",
    });

    const status = await fetchSetupStatus();

    expect(status.credentialSource).toBe("secret");
    // Inferred setup state from credential_source.
    expect(status.setupState).toBe("valid");
  });

  it("maps credential_source=persisted from the server", async () => {
    mockFetchOk({
      needs_app_install: false,
      app_credentials_configured: true,
      org_login: "acme",
      credential_source: "persisted",
    });

    const status = await fetchSetupStatus();

    expect(status.credentialSource).toBe("persisted");
    expect(status.setupState).toBe("valid");
  });

  it("maps invalid_secret / fatal setup_state as a fatal state", async () => {
    mockFetchOk({
      needs_app_install: true,
      app_credentials_configured: false,
      org_login: null,
      credential_source: null,
      setup_state: "invalid_secret",
      setup_error: "GITHUB_APP_CLIENT_ID is empty",
      setup_retryable: false,
      credentials_unrecoverable: false,
    });

    const status = await fetchSetupStatus();

    expect(status.setupState).toBe("invalid_secret");
    expect(status.setupError).toBe("GITHUB_APP_CLIENT_ID is empty");
    expect(status.setupRetryable).toBe(false);
    expect(status.credentialsUnrecoverable).toBe(false);
    expect(status.credentialSource).toBeNull();
  });

  it("accepts the 'fatal' alias as invalid_secret setup state", async () => {
    mockFetchOk({
      needs_app_install: true,
      app_credentials_configured: false,
      org_login: null,
      setup_state: "fatal",
    });

    const status = await fetchSetupStatus();

    expect(status.setupState).toBe("invalid_secret");
  });

  it("maps credentials_unrecoverable=true from the server", async () => {
    mockFetchOk({
      needs_app_install: true,
      app_credentials_configured: false,
      org_login: null,
      credentials_unrecoverable: true,
      setup_error: "Persisted credentials cannot be decrypted",
      setup_retryable: false,
    });

    const status = await fetchSetupStatus();

    expect(status.credentialsUnrecoverable).toBe(true);
    expect(status.setupState).toBe("unrecoverable");
    expect(status.setupError).toBe("Persisted credentials cannot be decrypted");
    expect(status.credentialSource).toBeNull();
  });

  it("maps canonical setup_state=credentials_unrecoverable as the unrecoverable state", async () => {
    mockFetchOk({
      needs_app_install: true,
      app_credentials_configured: false,
      org_login: null,
      credential_source: null,
      setup_state: "credentials_unrecoverable",
      setup_error: "Persisted credentials cannot be decrypted",
      setup_retryable: false,
      credentials_unrecoverable: true,
    });

    const status = await fetchSetupStatus();

    expect(status.setupState).toBe("unrecoverable");
    expect(status.credentialSource).toBeNull();
    expect(status.setupError).toBe("Persisted credentials cannot be decrypted");
    expect(status.setupRetryable).toBe(false);
    expect(status.credentialsUnrecoverable).toBe(true);
  });

  it("maps setup_state=unconfigured explicitly", async () => {
    mockFetchOk({
      needs_app_install: true,
      app_credentials_configured: false,
      org_login: null,
      setup_state: "unconfigured",
    });

    const status = await fetchSetupStatus();

    expect(status.setupState).toBe("unconfigured");
  });

  it("maps canonical setup_state=valid_secret explicitly", async () => {
    mockFetchOk({
      needs_app_install: false,
      app_credentials_configured: true,
      org_login: "acme",
      setup_state: "valid_secret",
      credential_source: "secret",
    });

    const status = await fetchSetupStatus();

    expect(status.setupState).toBe("valid");
    expect(status.credentialSource).toBe("secret");
  });

  it("maps canonical setup_state=valid_persisted explicitly", async () => {
    mockFetchOk({
      needs_app_install: false,
      app_credentials_configured: true,
      org_login: "acme",
      setup_state: "valid_persisted",
      credential_source: "persisted",
    });

    const status = await fetchSetupStatus();

    expect(status.setupState).toBe("valid");
    expect(status.credentialSource).toBe("persisted");
  });

  it("does not surface raw setup tokens or secret material", async () => {
    // Even if the server mistakenly includes secret-like fields, the DTO
    // must not map them into the typed response.
    mockFetchOk({
      needs_app_install: true,
      app_credentials_configured: false,
      org_login: null,
      setup_token: "raw-secret-token-abc123",
      client_secret: "should-not-leak",
      private_key: "-----BEGIN PRIVATE KEY-----\n...",
      credential_source: "secret",
    });

    const status = await fetchSetupStatus();

    // The DTO should not expose any of these secret fields.
    const json = JSON.stringify(status);
    expect(json).not.toContain("raw-secret-token-abc123");
    expect(json).not.toContain("should-not-leak");
    expect(json).not.toContain("BEGIN PRIVATE KEY");
    // credential_source=secret maps to the source name, not the secret value.
    expect(status.credentialSource).toBe("secret");
  });

  it("defaults setup_retryable to false when absent", async () => {
    mockFetchOk({
      needs_app_install: true,
      app_credentials_configured: false,
      org_login: null,
    });

    const status = await fetchSetupStatus();

    expect(status.setupRetryable).toBe(false);
  });

  it("maps setup_retryable=true from the server", async () => {
    mockFetchOk({
      needs_app_install: true,
      app_credentials_configured: false,
      org_login: null,
      setup_retryable: true,
      setup_error: "Temporary failure",
    });

    const status = await fetchSetupStatus();

    expect(status.setupRetryable).toBe(true);
    expect(status.setupError).toBe("Temporary failure");
  });

  it("treats unknown credential_source values as null (defensive)", async () => {
    mockFetchOk({
      needs_app_install: true,
      app_credentials_configured: false,
      org_login: null,
      credential_source: "unknown_future_source",
    });

    const status = await fetchSetupStatus();

    expect(status.credentialSource).toBeNull();
    expect(status.setupState).toBeNull();
  });

  it("treats unknown setup_state values by falling back to credential inference", async () => {
    mockFetchOk({
      needs_app_install: false,
      app_credentials_configured: true,
      org_login: "acme",
      setup_state: "some_future_state",
      credential_source: "secret",
    });

    const status = await fetchSetupStatus();

    // Unknown setup_state falls through to credential-source inference.
    expect(status.credentialSource).toBe("secret");
    expect(status.setupState).toBe("valid");
  });
});
