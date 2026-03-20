---
title: "ADR-039: Replace Clerk and GitHub App with GitHub OAuth App"
type: adr
tags: ["adr","architecture","github","oauth","auth","desktop"]
---

# ADR-039: Replace Clerk and GitHub App with GitHub OAuth App

**Status:** Accepted
**Date:** 2026-03-20
**Supersedes:** ADR-001 (Clerk Authentication), partially ADR-037 (GitHub App PR Workflow)

---

## Context

The current auth stack has two separate systems:

1. **Clerk** (`clerk.djinnai.io`) — desktop app user authentication via PKCE OAuth flow. Provides user identity (name, email, avatar) and session tokens.
2. **GitHub App** (`djinn-ai-bot`) — server-side device code flow for GitHub API access. Provides installation tokens for PR creation, comments, and CI status.

This dual-auth approach has several problems:

- **GitHub App requires org installation**: The app must be installed on each org by an admin. Users connecting their personal GitHub account cannot create PRs on org repos without this installation step. This was discovered when the PR creation flow failed with "No GitHub installation ID found" despite the user being authenticated.
- **Two auth flows during onboarding**: Users must complete Clerk login AND connect GitHub separately in Settings.
- **Extra dependency/cost**: Clerk is a paid service that adds no value over GitHub OAuth for a developer tool.
- **Installation token complexity**: The GitHub App derives installation tokens from user tokens, adding an unnecessary indirection layer. The user's OAuth token with `repo` scope can do everything directly.

## Decision

Replace both Clerk and the GitHub App with a single **GitHub OAuth App** using the device code flow.

### Client ID

`Ov23liBIL080Vt6WJs69`

### Scopes

- `repo` — full repository access (PRs, comments, push, CI status)
- `read:org` — read org membership (discover org repos)
- `user:email` — user email for identity

### Auth Flow

1. User launches desktop app → sees login screen
2. Clicks "Sign in with GitHub" → device code flow starts
3. User enters code on `github.com/login/device`
4. App polls until authorized → stores tokens in OS keyring
5. Desktop pushes token to server credential DB for API access
6. User is authenticated AND has GitHub API access — single step

### GitHub API Access

All GitHub API calls use the **user's OAuth access token** directly as a Bearer token. No installation tokens. PRs, comments, and CI status reads all appear as the authenticated user.

### Token Storage

- **Desktop**: OS keyring (same as current Clerk tokens)
- **Server**: credential DB under `__OAUTH_GITHUB_APP` key (same key as current GitHub App tokens)
- Desktop syncs token to server on login and after each refresh

### Token Refresh

GitHub OAuth tokens expire in 8 hours. Refresh tokens are long-lived. The existing refresh logic in `token_refresh.rs` is adapted for GitHub's token format.

## Changes

### Removed

- Clerk dependency entirely (`clerk.djinnai.io`, PKCE flow, Clerk client ID)
- GitHub App installation token derivation (`derive_installation_token`)
- GitHub Settings tab in the desktop UI (auth happens at login)
- `auth_callback.rs` and `dev_server.rs` (no PKCE callback needed)
- Installation ID storage and lookup

### Modified

- `desktop/src-tauri/src/auth.rs` — GitHub device code flow replaces Clerk PKCE
- `desktop/src-tauri/src/commands.rs` — device code commands replace PKCE commands
- `desktop/src-tauri/src/token_refresh.rs` — GitHub token refresh replaces Clerk refresh
- `desktop/src/components/AuthGate.tsx` — device code UI replaces browser redirect
- `server/crates/djinn-provider/src/github_api.rs` — user token replaces installation token
- `server/crates/djinn-provider/src/oauth/github_app.rs` — new client ID, scopes, no installation logic

## Consequences

### Positive

- Single-step onboarding: sign in with GitHub = authenticated + API access
- No org admin approval needed for GitHub API access
- Eliminates Clerk dependency and cost
- Simpler token flow (no installation token derivation)
- PRs created as the user (clear authorship)

### Negative

- Requires every user to have a GitHub account (acceptable for a dev tool)
- Loses multi-provider auth flexibility (no Google/email login)
- PRs show as user, not as a bot — less clear that AI generated the code
- `repo` scope is broad — grants full repo access, not fine-grained

### Neutral

- `git push` continues to use system git credentials (SSH keys, credential helpers). The OAuth token is not injected into git operations in this ADR — that may be addressed separately.
- The GitHub App can be re-added later as an optional layer for bot-identity comments and check runs if needed.
