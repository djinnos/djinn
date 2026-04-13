/**
 * Central IPC handler registration.
 *
 * Maps every frontend `invoke(command, args)` call to the corresponding
 * Node.js module function that registers all IPC handlers.
 */

import { ipcMain, dialog, shell, BrowserWindow, type IpcMainInvokeEvent } from "electron";
import { execSync } from "node:child_process";
import * as fs from "node:fs";
import * as path from "node:path";
import { homedir } from "node:os";

import * as auth from "./modules/auth.js";
import * as tokenRefresh from "./modules/token-refresh.js";
import * as tokenSync from "./modules/token-sync.js";
import * as server from "./modules/server.js";
import * as ssh from "./modules/ssh.js";
import * as deploy from "./modules/deploy.js";
import * as connectionMode from "./modules/connection-mode.js";
import * as wsl from "./modules/wsl.js";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Shortcut to send events to the renderer. */
function makeSendEvent(win: BrowserWindow): server.SendEvent {
  return (event: string, payload?: unknown) => {
    if (!win.isDestroyed()) {
      win.webContents.send(event, payload);
    }
  };
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

const serverState = new server.ServerState();

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

export function registerIpcHandlers(mainWindow: BrowserWindow): void {
  const sendEvent = makeSendEvent(mainWindow);

  // ── Auth (GitHub OAuth) ──────────────────────────────────────────────

  ipcMain.handle("start_github_login", async () => {
    const dcr = await auth.startDeviceFlow();
    // Start polling in background — don't block the UI
    (async () => {
      try {
        const tokens = await auth.pollDeviceFlow(dcr.deviceCode, dcr.interval);
        const user = await auth.fetchGithubUser(tokens.access_token);
        const profile: auth.UserProfile = {
          sub: String(user.id),
          name: user.name ?? user.login,
          email: user.email ?? undefined,
          picture: user.avatar_url,
        };
        const session: auth.AuthSession = {
          accessToken: tokens.access_token,
          userProfile: profile,
        };
        auth.setAuthSession(session);

        // Store tokens
        const expiresAt = tokens.expires_in
          ? Math.floor(Date.now() / 1000) + tokens.expires_in
          : Math.floor(Date.now() / 1000) + 8 * 3600; // 8h default
        const stored: auth.StoredTokens = {
          access_token: tokens.access_token,
          refresh_token: tokens.refresh_token ?? "",
          expires_at: expiresAt,
          user_login: user.login,
          avatar_url: user.avatar_url,
        };
        await auth.storeToken(JSON.stringify(stored));

        // Update token refresh state
        tokenRefresh.setTokenState({
          accessToken: tokens.access_token,
          refreshToken: tokens.refresh_token ?? "",
          expiresAt,
          tokenType: tokens.token_type,
          scope: tokens.scope,
        });

        // Sync to server (best-effort)
        await tokenSync.syncTokensToServer(
          tokens.access_token,
          tokens.refresh_token ?? "",
          expiresAt,
          user.login,
        );

        sendEvent("auth:state-changed", {
          isAuthenticated: true,
          user: profile,
        });
      } catch (err) {
        sendEvent("auth:login-failed", {
          reason: err instanceof Error ? err.message : String(err),
        });
      }
    })();

    return {
      userCode: dcr.userCode,
      verificationUri: dcr.verificationUri,
    };
  });

  ipcMain.handle("auth_get_state", async () => {
    const session = auth.getAuthSession();
    if (session) {
      return { isAuthenticated: true, user: session.userProfile ?? null };
    }
    const ts = tokenRefresh.getTokenState();
    return { isAuthenticated: ts !== null, user: null };
  });

  ipcMain.handle("auth_login", async () => {
    // Delegates — the renderer calls start_github_login directly
  });

  ipcMain.handle("auth_logout", async () => {
    await tokenRefresh.logout();
    auth.setAuthSession(null);
    sendEvent("auth:state-changed", { isAuthenticated: false, user: null });
  });

  ipcMain.handle("attempt_silent_auth", async () => {
    const result = await tokenRefresh.attemptSilentAuthOnStartup();
    if (result.kind === "success") {
      const ts = tokenRefresh.getTokenState();
      if (ts) {
        try {
          const user = await auth.fetchGithubUser(ts.accessToken);
          const profile: auth.UserProfile = {
            sub: String(user.id),
            name: user.name ?? user.login,
            email: user.email ?? undefined,
            picture: user.avatar_url,
          };
          const session: auth.AuthSession = {
            accessToken: ts.accessToken,
            userProfile: profile,
          };
          auth.setAuthSession(session);
          sendEvent("auth:state-changed", {
            isAuthenticated: true,
            user: profile,
          });
          sendEvent("auth:silent-refresh-success");
          return true;
        } catch {
          /* fall through */
        }
      }
    }
    sendEvent("auth:silent-refresh-failed");
    sendEvent("auth:login-required");
    return false;
  });

  // ── Auth (token management) ──────────────────────────────────────────

  ipcMain.handle("get_auth_token", async () => {
    return tokenRefresh.getValidAccessToken();
  });

  ipcMain.handle("set_auth_token", async (_e: IpcMainInvokeEvent, args: { token: string }) => {
    // Legacy — store directly in session
    const session = auth.getAuthSession();
    if (session) {
      session.accessToken = args.token;
      auth.setAuthSession(session);
    }
  });

  ipcMain.handle("clear_auth_token", async () => {
    auth.setAuthSession(null);
    tokenRefresh.clearTokenState();
    await auth.clearToken();
  });

  ipcMain.handle("get_refresh_token", async () => {
    const raw = await auth.retrieveToken();
    if (!raw) return null;
    try {
      const parsed = JSON.parse(raw);
      return parsed.refresh_token ?? null;
    } catch {
      return null;
    }
  });

  ipcMain.handle("set_refresh_token", async (_e: IpcMainInvokeEvent, args: { token: string }) => {
    const raw = await auth.retrieveToken();
    const stored = raw ? JSON.parse(raw) : {};
    stored.refresh_token = args.token;
    await auth.storeToken(JSON.stringify(stored));
  });

  ipcMain.handle("clear_refresh_token", async () => {
    await auth.clearToken();
  });

  ipcMain.handle("perform_token_refresh", async () => {
    return tokenRefresh.performSilentRefresh();
  });

  ipcMain.handle("get_auth_state", async () => {
    return tokenRefresh.getTokenState();
  });

  ipcMain.handle("is_token_expired", async () => {
    return tokenRefresh.isTokenExpiredOrStale();
  });

  ipcMain.handle("logout", async () => {
    await tokenRefresh.logout();
    auth.setAuthSession(null);
  });

  ipcMain.handle("sync_github_tokens", async () => {
    const ts = tokenRefresh.getTokenState();
    if (!ts) return false;
    await tokenSync.syncTokensToServer(
      ts.accessToken,
      ts.refreshToken,
      ts.expiresAt,
      undefined,
    );
    return true;
  });

  // ── Server ───────────────────────────────────────────────────────────

  ipcMain.handle("greet", async (_e: IpcMainInvokeEvent, args: { name: string }) => {
    return `Hello, ${args.name}! You've been greeted from Electron!`;
  });

  ipcMain.handle("get_server_port", async () => {
    return serverState.port;
  });

  ipcMain.handle("get_server_url", async () => {
    return serverState.baseUrl;
  });

  ipcMain.handle("get_server_status", async () => {
    return serverState.toStatus();
  });

  ipcMain.handle("retry_server_connection", async () => {
    return server.retryConnection(serverState, sendEvent);
  });

  // ── Connection mode ──────────────────────────────────────────────────

  ipcMain.handle("get_connection_mode", async () => {
    return connectionMode.load();
  });

  ipcMain.handle("set_connection_mode", async (_e: IpcMainInvokeEvent, args: { mode: connectionMode.ConnectionMode }) => {
    connectionMode.save(args.mode);
  });

  ipcMain.handle("has_saved_connection_mode", async () => {
    try {
      return fs.existsSync(path.join(homedir(), ".djinn", "connection_mode.json"));
    } catch {
      return false;
    }
  });

  // ── SSH / Tunnel ─────────────────────────────────────────────────────

  ipcMain.handle("get_ssh_hosts", async () => {
    return ssh.loadHosts();
  });

  ipcMain.handle("save_ssh_host", async (_e: IpcMainInvokeEvent, args: { host: ssh.SshHost }) => {
    ssh.addHost(args.host);
  });

  ipcMain.handle("remove_ssh_host", async (_e: IpcMainInvokeEvent, args: { id: string }) => {
    ssh.removeHost(args.id);
  });

  ipcMain.handle("test_ssh_connection", async (_e: IpcMainInvokeEvent, args: { hostId: string }) => {
    const host = ssh.findHost(args.hostId);
    if (!host) throw new Error(`SSH host not found: ${args.hostId}`);
    return ssh.testConnection(host);
  });

  ipcMain.handle("get_tunnel_status", async (): Promise<ssh.TunnelStatus> => {
    if (!ssh.activeTunnelHostId()) {
      return { status: "disconnected" };
    }
    if (ssh.isActiveTunnelAlive()) {
      const port = ssh.activeTunnelLocalPort();
      return { status: "connected", local_port: port! };
    }
    return { status: "disconnected" };
  });

  ipcMain.handle("deploy_server_to_host", async (_e: IpcMainInvokeEvent, args: { hostId: string }) => {
    const host = ssh.findHost(args.hostId);
    if (!host) throw new Error(`SSH host not found: ${args.hostId}`);
    const result = await deploy.deployToHost(host);
    return `Deployed v${result.version} (${result.arch})`;
  });

  // ── Platform ─────────────────────────────────────────────────────────

  ipcMain.handle("check_wsl_available", async () => {
    return wsl.isAvailable();
  });

  ipcMain.handle("download_server_binary", async () => {
    return server.downloadServerBinary();
  });

  // ── Git ──────────────────────────────────────────────────────────────

  ipcMain.handle("check_git_remote", async (_e: IpcMainInvokeEvent, args: { projectPath: string }) => {
    try {
      const url = execSync("git remote get-url origin", {
        cwd: args.projectPath,
        encoding: "utf-8",
        timeout: 5000,
      }).trim();
      return url || null;
    } catch {
      return null;
    }
  });

  ipcMain.handle("list_git_branches", async (_e: IpcMainInvokeEvent, args: { projectPath: string }) => {
    try {
      const output = execSync("git branch --format=%(refname:short)", {
        cwd: args.projectPath,
        encoding: "utf-8",
        timeout: 5000,
      });
      return output.split("\n").map((b) => b.trim()).filter(Boolean);
    } catch {
      return [];
    }
  });

  ipcMain.handle("setup_git_remote", async (_e: IpcMainInvokeEvent, args: { projectPath: string; remoteUrl: string }) => {
    execSync(`git remote add origin ${args.remoteUrl}`, {
      cwd: args.projectPath,
      encoding: "utf-8",
      timeout: 5000,
    });
    const branch = execSync("git rev-parse --abbrev-ref HEAD", {
      cwd: args.projectPath,
      encoding: "utf-8",
      timeout: 5000,
    }).trim();
    execSync(`git push -u origin ${branch}`, {
      cwd: args.projectPath,
      encoding: "utf-8",
      timeout: 30000,
    });
    return `Pushed to ${args.remoteUrl} (branch: ${branch})`;
  });

  // ── Window controls ──────────────────────────────────────────────────

  ipcMain.handle("window:minimize", () => {
    mainWindow.minimize();
  });

  ipcMain.handle("window:toggleMaximize", () => {
    if (mainWindow.isMaximized()) {
      mainWindow.unmaximize();
    } else {
      mainWindow.maximize();
    }
  });

  ipcMain.handle("window:close", () => {
    mainWindow.close();
  });

  // ── Shell ────────────────────────────────────────────────────────────

  ipcMain.handle("shell:open-external", async (_e: IpcMainInvokeEvent, args: { url: string }) => {
    await shell.openExternal(args.url);
  });

  // ── Dialogs ──────────────────────────────────────────────────────────

  ipcMain.handle("select_directory", async (_e: IpcMainInvokeEvent, args?: { title?: string }) => {
    const result = await dialog.showOpenDialog(mainWindow, {
      properties: ["openDirectory"],
      title: args?.title,
    });
    return result.filePaths[0] ?? null;
  });

  ipcMain.handle("select_file", async (_e: IpcMainInvokeEvent, args?: { title?: string }) => {
    const result = await dialog.showOpenDialog(mainWindow, {
      properties: ["openFile"],
      title: args?.title,
    });
    return result.filePaths[0] ?? null;
  });
}
