---
tags:
    - scope
    - milestone-2
    - reference
title: Milestone 2 Scope
type: reference
---
# Milestone 2 Scope

## In Scope
- Clerk sign-in via system browser OAuth PKCE (RFC 8252) — port the CLI's proven `auth-service.ts` pattern to Tauri Rust
- Deep link registration via `tauri-plugin-deep-link` for `djinn://auth/callback` in production
- Dev-mode fallback: ephemeral HTTP server on localhost:19876 for auth callback
- PKCE flow: code_verifier + SHA256 code_challenge + state parameter (CSRF protection)
- Refresh token encrypted storage on disk (tauri-plugin-stronghold or OS keychain via keyring crate)
- Silent token refresh on app startup using stored refresh token — no browser hop for returning users
- Access token in-memory only, ID token decoded for user profile (sub, name, email, picture)
- Auth state exposed to frontend via Tauri commands (`auth_get_state`, `auth_login`, `auth_logout`) and events (`auth:state-changed`)
- `AuthGate` React component: checks auth state on mount, renders app or sign-in prompt
- Concurrent refresh call serialization (Clerk may rotate refresh tokens)
- 30-second expiry buffer to prevent using nearly-expired tokens
- Sign-out: best-effort token revocation at Clerk, clear stored refresh token, clear in-memory state
- Onboarding wizard (3 steps): server connection check → provider setup → project setup
- Skip available on every onboarding step, partial progress persisted
- First-run detection: show wizard if no projects registered and no provider configured

## Out of Scope
- Server-side JWT validation / JWKS / Bearer middleware — server is auth-free (ADR-001)
- Token passing from desktop to server (no `--token` CLI arg, no `DJINN_TOKEN` env, no `clerk_token` in daemon.json)
- License key system / feature gating / Stripe billing — deferred (ADR-002)
- Per-MCP-request token headers — desktop talks to server via plain HTTP
- Clerk SDK in the webview / embedded Clerk components — auth handled entirely in Tauri Rust layer
- Server-side user identity or session attribution — server doesn't know who the user is
- Onboarding sign-in as a mandatory blocking step — sign-in is available but not required for server access

## Preferences
- Follow the CLI's `auth-service.ts` implementation closely — it's battle-tested with Clerk's quirks
- Use `prompt=login` on Clerk auth URL to force login form (enables account switching)
- Scope: `openid profile email offline_access` (offline_access gives refresh tokens)
- File permissions 0o600 on any file containing tokens
- Parse ID token claims client-side for user profile (no separate `/userinfo` call needed, but use as fallback)
- Clerk configuration: `clerk.djinnai.io` domain, register both `djinn://auth/callback` and `http://localhost:19876/auth/callback` as redirect URIs

## Relations
- [[Roadmap]] — Phase 2 (Auth & Onboarding)
- [[V1 Requirements]] — AUTH and ONBOARD requirements
- [[ADR-001: Desktop-Only Clerk Authentication via System Browser PKCE]] — auth architecture
- [[ADR-002: Feature Licensing Deferred — Metabase-Style Open Core]] — licensing out of scope
- [[Architecture Research]] — original auth design (superseded by ADR-001)