/**
 * Auth client for the web frontend.
 *
 * The prior Electron host owned the GitHub OAuth device-code flow
 * and persisted tokens locally. That plumbing was removed in the
 * Electron-strip migration. The Dockerized Rust server exposes its
 * own OAuth callback on port :1455, but the HTTP endpoints that the
 * desktop UI would call (`auth_get_state`, `start_github_login`,
 * `auth_login`, `auth_logout`) are not yet plumbed over HTTP.
 *
 * Until those endpoints exist, we operate in an "auth-disabled" mode:
 * the user is treated as authenticated with an anonymous profile so
 * the UI can render. Real auth needs a follow-up once the server
 * exposes `/auth/state`, `/auth/start`, `/auth/logout` routes.
 */

export interface AuthUser {
  sub: string;
  name?: string;
  email?: string;
  picture?: string;
}

export interface AuthState {
  isAuthenticated: boolean;
  user: AuthUser | null;
}

export interface DeviceCodeInfo {
  userCode: string;
  verificationUri: string;
}

const ANON_USER: AuthUser = {
  sub: "local-user",
  name: "Local User",
};

export async function authGetState(): Promise<AuthState> {
  return { isAuthenticated: true, user: ANON_USER };
}

export async function authLogin(): Promise<void> {
  // no-op placeholder — server does not yet expose a login HTTP endpoint.
}

export async function authLogout(): Promise<void> {
  // no-op placeholder.
}

export async function startGithubLogin(): Promise<DeviceCodeInfo> {
  throw new Error("GitHub login is not yet available in the web client.");
}

export async function attemptSilentAuth(): Promise<boolean> {
  return true;
}
