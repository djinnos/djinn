import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

describe("electron commands browser runtime", () => {
  beforeEach(() => {
    vi.resetModules();
    vi.unstubAllGlobals();
    window.localStorage.clear();
    Object.defineProperty(window, "electronAPI", {
      value: undefined,
      writable: true,
      configurable: true,
    });
  });

  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
    window.localStorage.clear();
  });

  it("boots against an HTTP server without window.electronAPI", async () => {
    window.localStorage.setItem("djinn.serverBaseUrl", "http://browser.test:4123/");

    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      json: async () => ({ status: "ok", version: "1.2.3" }),
    });
    vi.stubGlobal("fetch", fetchMock);

    const commands = await import("./commands");

    await expect(commands.getServerUrl()).resolves.toBe("http://browser.test:4123");
    await expect(commands.getServerPort()).resolves.toBe(4123);
    await expect(commands.getConnectionMode()).resolves.toEqual({
      type: "remote",
      url: "http://browser.test:4123",
    });

    await expect(commands.getServerStatus()).resolves.toEqual({
      base_url: "http://browser.test:4123",
      port: 4123,
      is_healthy: true,
      has_error: false,
      error_message: null,
      server_version: "1.2.3",
      update_available: false,
    });

    await expect(commands.retryServerConnection()).resolves.toBe("http://browser.test:4123");

    expect(fetchMock).toHaveBeenCalledWith("http://browser.test:4123/health");
    expect(commands.isElectronRuntime()).toBe(false);
  });

  it("stores browser remote URLs through the shared runtime boundary", async () => {
    const commands = await import("./commands");

    await commands.setConnectionMode({ type: "remote", url: "http://configured.test:9000/" });

    await expect(commands.getServerUrl()).resolves.toBe("http://configured.test:9000");
    expect(window.localStorage.getItem("djinn.serverBaseUrl")).toBe("http://configured.test:9000");
  });
});
