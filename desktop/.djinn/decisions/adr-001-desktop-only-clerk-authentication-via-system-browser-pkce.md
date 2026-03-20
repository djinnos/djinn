---
tags:
    - adr
    - milestone-2
    - auth
    - clerk
title: 'ADR-001: Desktop-Only Clerk Authentication via System Browser PKCE'
type: adr
---
# ADR-001: Desktop-Only Clerk Authentication via System Browser PKCE

## Status: Accepted

## Context

The original design (server ADR-004) had the desktop passing Clerk JWTs to the server, with the server validating every MCP request via JWKS. This created several issues:

1. **Unnecessary coupling** — the server is a local daemon on the user's machine. Gating localhost HTTP behind JWT auth solves no real security problem.
2. **60-second token churn** — Clerk's short-lived JWTs meant the desktop had to call `getToken()` before every MCP request, adding constant network dependency to Clerk's API for what's localhost communication.
3. **Deep link complexity at startup** — the system browser → deep link → token → pass to server → server validates JWKS flow had many moving parts just to boot the app.
4. **Conflated concerns** — user authentication (identity for sync) was conflated with server access control (licensing/paywall), which aren't needed for v1.

The existing CLI (`/home/fernando/git/cli`) already implements Clerk auth via system browser OAuth PKCE (RFC 8252) successfully. The pattern is proven and well-documented in the CLI's ADR "Authentication Provider: Clerk (via System Browser)".

## Decision

**The server runs without authentication. The desktop owns auth entirely — Clerk sign-in via system browser OAuth PKCE, self-contained in the Tauri layer.**

### Server: Auth-Free

- No JWT validation, no JWKS fetching, no Bearer token middleware
- All HTTP endpoints (MCP, SSE, health) are plain HTTP on localhost
- The `--token` CLI arg and `DJINN_TOKEN` env var are removed (or ignored)
- `daemon.json` drops the `clerk_token` field — just pid, port, started_at
- Server can be started independently of any auth state

### Desktop: System Browser PKCE (Ported from CLI)

Port the CLI's proven auth pattern to Tauri:

```
1. User clicks "Sign In" (or auto-triggered on first launch)
2. Tauri main process generates PKCE (code_verifier + code_challenge) + state
3. Opens system browser to Clerk's hosted login:
   https://clerk.djinnai.io/oauth/authorize?
     client_id={id}&
     redirect_uri=djinn://auth/callback&
     response_type=code&
     scope=openid+profile+email+offline_access&
     code_challenge={challenge}&
     code_challenge_method=S256&
     prompt=login
4. User authenticates in browser (leverages existing Google/GitHub sessions)
5. Clerk redirects to djinn://auth/callback?code=...&state=...
6. tauri-plugin-deep-link intercepts, Rust code validates state, extracts code
7. Exchange code for tokens at Clerk's token endpoint
8. Store refresh_token encrypted on disk
9. Access token + user profile available to frontend via invoke()
```

### Redirect URI Strategy

| Environment | Redirect URI | Mechanism |
|---|---|---|
| Production (bundled) | `djinn://auth/callback` | `tauri-plugin-deep-link` custom protocol |
| Dev mode | `http://localhost:19876/auth/callback` | Ephemeral HTTP server (custom protocols unreliable in dev) |

Both URIs registered in Clerk OAuth app configuration.

### Token Storage

- **Refresh token**: Stored encrypted on disk via `tauri-plugin-stronghold` or OS keychain (keyring crate). File permissions 0o600.
- **Access token**: In-memory only (short-lived, refreshed silently)
- **ID token**: Decoded for user profile (name, email, picture). Not stored.

### Silent Refresh

Returning users never see a browser:
1. App starts → check for stored refresh token
2. If found: POST to Clerk's token endpoint with `grant_type=refresh_token`
3. Get fresh access + refresh tokens (Clerk may rotate refresh token)
4. Serialize concurrent refresh calls to handle rotation safely
5. 30-second buffer before expiry to prevent using nearly-expired tokens

### Frontend Auth State

- Auth managed entirely in Tauri Rust layer (no Clerk SDK in webview)
- Frontend gets state via `invoke("auth_get_state")` → `{isAuthenticated, user}`
- State changes pushed via Tauri events (`auth:state-changed`)
- `AuthGate` component in React: checks state on mount, shows app or sign-in

### Why Auth is Needed (Even Without Server Gating)

- **Sync**: Task sync between machines requires user identity
- **Email capture**: Beta period needs sign-up for waitlist/outreach
- **Future billing**: Clerk + Stripe integration when premium features arrive
- **Attribution**: User identity for session/activity tracking

## Consequences

### Positive
- Server is simpler — no auth code, no JWKS dependency, no network requirement on boot
- Desktop-server communication is plain HTTP — no token headers, no refresh dance per request
- Proven pattern — CLI already runs this flow in production
- Silent refresh means returning users never see a browser hop
- Auth concerns properly separated from server access control

### Negative
- System browser context switch on first sign-in (acceptable for developer audience)
- Clerk SaaS dependency for identity (mitigated by standard OIDC — provider-swappable)
- Refresh token on disk is a sensitive file (mitigated by encryption + 0o600 permissions)

### Supersedes
- Server ADR-004 (Authentication — Clerk JWT Validation) no longer applies to desktop-server communication
- Desktop AUTH-01 through AUTH-08 requirements rewritten (server token passing removed)

## Relations
- [[Roadmap]] — Phase 2 (Auth & Onboarding)
- [[V1 Requirements]] — AUTH requirements revised
- [[Milestone 2 Scope]] — scope boundaries for this phase
- [[Architecture Research]] — original auth design (superseded)