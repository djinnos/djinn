---
tags:
    - adr
    - auth
    - clerk
    - jwt
title: Authentication — Clerk JWT Validation
type: adr
---
# ADR-004: Authentication — Clerk JWT Validation

## Status: Accepted

## Context

The original plan called for a custom license token system: Ed25519 JWT with device fingerprinting, monotonic timestamp logging, and 30-day expiry with periodic revalidation. This was 5 requirements (LIC-01 through LIC-05) of custom security code.

The desktop app already uses Clerk for user authentication. Clerk issues short-lived JWTs (60-second default lifetime) with RS256 signatures. Using Clerk for server auth eliminates the entire custom licensing subsystem.

## Decision

Authenticate the server via Clerk JWTs. The desktop passes its Clerk token to the server. The server validates the JWT against Clerk's JWKS endpoint.

### Auth flow

```
Desktop (Electron):
  1. User signs in via Clerk embedded component
  2. Clerk SDK issues session JWT (60s lifetime, auto-refreshed)
  3. Desktop spawns server, passes initial token

Server:
  4. Validates JWT: RS256 signature via Clerk JWKS
  5. Extracts user ID (sub claim) as identity
  6. Caches JWKS keys (1-hour TTL, refresh on signature failure)

On every MCP connection:
  7. Desktop calls Clerk.session.getToken() (fresh, <60s old)
  8. Passes token in MCP session handshake
  9. Server validates before accepting session
```

### Crate stack

```toml
jsonwebtoken = "9"    # JWT decode + RS256 validation
reqwest = { version = "0.12", features = ["json"] }  # Fetch JWKS
```

Or use `axum-jwks = "0.12"` for higher-level Axum integration (wraps jsonwebtoken 9).

### Token validation rules

- Algorithm: RS256 only (assert from config, never trust token's `alg` field)
- JWKS: Fetch from `https://api.clerk.com/v1/jwks`, cache 1 hour, invalidate on signature failure
- Claims: validate `exp`, `iat`, `iss` (Clerk frontend API URL)
- `azp` (authorized party): skip validation for Electron (origin may be custom URI scheme)
- `sub`: Clerk user ID — this is the server's identity for the session

### Revocation model

Clerk JWTs are not individually revocable. The 60-second lifetime IS the revocation window:
- Revoking a user's session in Clerk Dashboard → client's next `getToken()` call fails
- Tokens already in flight remain valid until expiry (max 60s)
- This is acceptable for a single-user desktop app

### Desktop-only spawn (v1)

For v1, the desktop always spawns the server. The token is passed at startup. The server won't start without a valid token.

### Headless mode (v2+)

Future: server can run headless (e.g., on a VPS). User copies/pastes a long-lived Clerk token or uses a Clerk Machine-to-Machine (M2M) token via CLI. This is a v2 concern.

## Consequences

### Positive
- Eliminates 5 custom licensing requirements (Ed25519 verification, device fingerprinting, monotonic timestamps, etc.)
- Leverages Clerk's existing auth infrastructure — user management, revocation, token refresh all handled
- Desktop already has Clerk integration — no new auth UI needed
- Server auth is just JWT validation (~20 lines of code)

### Negative
- Server requires network access to fetch JWKS on first start (one-time, then cached)
- 60-second token lifetime means desktop must refresh frequently (Clerk SDK handles this automatically)
- No offline-first server operation without a cached JWKS key (acceptable for v1 desktop-spawned mode)

### Supersedes
- LIC-01 through LIC-05 in requirements (custom license token system)

## Relations
- [[Project Brief]] — updates authentication constraint
- [[V1 Requirements]] — replaces LIC-01..05 with Clerk-based auth
- [[Pitfalls Research]] — JWT pitfalls (alg:none attacks) still apply to Clerk validation