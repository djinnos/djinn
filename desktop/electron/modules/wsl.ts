/**
 * WSL (Windows Subsystem for Linux) support.
 *
 * Ported from src-tauri/src/wsl.rs
 *
 * On Windows, the desktop app can launch and connect to a djinn-server running
 * inside the default WSL distribution. WSL2 shares localhost networking so the
 * server is reachable at 127.0.0.1:{port}.
 *
 * On non-Windows platforms all functions are no-ops / return false.
 */

import { execSync } from "node:child_process";
import { platform } from "node:os";
import { healthCheck } from "./server.js";

// ---------------------------------------------------------------------------
// WSL detection
// ---------------------------------------------------------------------------

/** Check if WSL is available on this machine (Windows only). */
export function isAvailable(): boolean {
  if (platform() !== "win32") {
    return false;
  }
  try {
    const result = execSync("wsl --status", { stdio: "pipe" });
    return result !== null;
  } catch {
    return false;
  }
}

// ---------------------------------------------------------------------------
// WSL daemon management
// ---------------------------------------------------------------------------

/**
 * Ensure a djinn-server daemon is running inside the default WSL distribution.
 *
 * Returns the base URL on success (always `http://127.0.0.1:{port}` because
 * WSL2 shares localhost).
 */
export async function ensureWslDaemon(port: number): Promise<string> {
  if (platform() !== "win32") {
    throw new Error(
      `WSL is not available on this platform (requested port ${port})`,
    );
  }

  // Check if already running.
  try {
    execSync("wsl -e pgrep -f djinn-server", { stdio: "pipe" });
  } catch {
    // Not running -- start it.
    console.log(`Starting djinn-server inside WSL on port ${port}`);
    try {
      execSync(
        `wsl -e sh -c "nohup djinn-server --port ${port} &>/dev/null &"`,
        { stdio: "pipe" },
      );
    } catch (e) {
      throw new Error(`WSL djinn-server start failed: ${e}`);
    }

    // Give it a moment to bind.
    await sleep(500);
  }

  const baseUrl = `http://127.0.0.1:${port}`;

  // Health-check loop.
  for (let i = 0; i < 40; i++) {
    if (await healthCheck(baseUrl)) {
      return baseUrl;
    }
    await sleep(250);
  }

  throw new Error(
    "WSL djinn-server started but health endpoint not reachable",
  );
}

// ---------------------------------------------------------------------------
// Utility
// ---------------------------------------------------------------------------

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
