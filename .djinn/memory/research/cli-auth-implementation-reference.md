---
tags:
    - research
    - auth
    - clerk
    - cli-reference
title: CLI Auth Implementation Reference
type: research
---
# CLI Auth Implementation Reference

Reference documentation for the existing Clerk system browser OAuth implementation in the CLI desktop app (`/home/fernando/git/cli`). This is the proven pattern that the new Tauri desktop should port.

## Source Files

| File | Purpose |
|------|---------|
| `apps/desktop/src/main/auth-service.ts` | Core auth logic: PKCE, browser open, callback, token exchange, refresh, storage |
| `apps/desktop/src/main/ipc-handlers/auth.ts` | IPC bridge: `auth:getState`, `auth:login`, `auth:logout`, `auth:stateChanged` |
| `apps/preload/index.ts` | Preload script exposing `window.electronAPI.auth` interface |
| `apps/desktop/src/renderer/components/auth/AuthGate.tsx` | Renderer auth gate component (reducer-based state machine) |

## CLI ADRs

- **Authentication Provider: Clerk (via System Browser)** — full auth architecture ADR at `decisions/authentication-provider-clerk`
- **Release, Licensing, and Distribution Strategy** — licensing context at `decisions/release-licensing-and-distribution-strategy`

## OAuth Flow Detail

### PKCE Generation
```
code_verifier = 32 random bytes → 43-char base64url string
code_challenge = SHA256(code_verifier) → base64url encoded
state = 16 random bytes → base64url (CSRF protection)
```

### Authorization URL
```
https://clerk.djinnai.io/oauth/authorize?
  client_id=rXf6AlZNrHOcJ2HV&
  redirect_uri={redirect_uri}&
  response_type=code&
  scope=openid+profile+email+offline_access&
  code_challenge={challenge}&
  code_challenge_method=S256&
  state={state}&
  prompt=login
```

### Redirect URIs
- Production: `djinn://auth/callback` (custom protocol, OS-routed)
- Dev: `http://localhost:19876/auth/callback` (ephemeral HTTP server)

### Token Exchange
POST to `https://clerk.djinnai.io/oauth/token`:
```
grant_type=authorization_code
code={authorization_code}
redirect_uri={redirect_uri}
client_id={client_id}
code_verifier={code_verifier}
```

Returns: `access_token`, `id_token`, `refresh_token`, `expires_in`

### Silent Refresh
POST to `https://clerk.djinnai.io/oauth/token`:
```
grant_type=refresh_token
refresh_token={stored_refresh_token}
client_id={client_id}
scope=openid+profile+email+offline_access
```

Key behaviors:
- Clerk may rotate refresh token on each use — always store the new one
- Serialize concurrent refresh calls with a single in-flight promise
- 30-second buffer before expiry
- On 400/401: clear stored token, user must re-authenticate

### Token Revocation (Sign-Out)
POST to `https://clerk.djinnai.io/oauth/token/revoke`:
```
token={refresh_token OR access_token}
client_id={client_id}
```
Best-effort — non-fatal if fails.

## Token Storage

### Electron (Current CLI)
- Refresh token: `${app.getPath('userData')}/auth/refresh-token`
- Encrypted via `safeStorage.encryptString()` (macOS keychain, Windows DPAPI, Linux libsecret)
- Fallback: plain text if encryption unavailable (dev)
- File permissions: 0o600

### Tauri (New Desktop — Equivalent)
- Refresh token: via `tauri-plugin-stronghold` (encrypted vault) or `keyring` crate (OS keychain)
- Access token: in-memory only
- File permissions: 0o600

## Auth State Machine

```
App Startup
    ↓
Check stored refresh token
    ↓
├── Found → Silent refresh → AUTHENTICATED
├── Not found → UNAUTHENTICATED → auto-trigger login or show Sign In
└── Refresh fails (400/401) → Clear token → UNAUTHENTICATED
```

### AuthGate States
- `checking` — initial state, calling `auth:getState`
- `authenticated` — user profile available, show app
- `unauthenticated` — show sign-in prompt
- `timeout` — auth check took too long, show error with retry

## IPC Interface (To Port to Tauri Commands)

```typescript
auth: {
  login: () => Promise<{ success: boolean; error?: string }>;
  logout: () => Promise<void>;
  getState: () => Promise<{
    isAuthenticated: boolean;
    user?: { sub: string; name?: string; email?: string; picture?: string };
  }>;
  onStateChanged: (callback: (state) => void) => () => void;
}
```

Tauri equivalent: `invoke("auth_login")`, `invoke("auth_logout")`, `invoke("auth_get_state")`, `listen("auth:state-changed")`

## User Profile Extraction

Two approaches (use both as fallback chain):
1. Decode ID token payload (no verification needed — received over HTTPS)
2. GET `https://clerk.djinnai.io/userinfo` with Bearer access_token

Claims: `sub` (required), `name`/`given_name`/`family_name`, `email`, `picture`/`image_url`

## Relations
- [[ADR-001: Desktop-Only Clerk Authentication via System Browser PKCE]] — decision to port this pattern
- [[Milestone 2 Scope]] — Phase 2 scope using this reference
- [[Architecture Research]] — original auth section (superseded)