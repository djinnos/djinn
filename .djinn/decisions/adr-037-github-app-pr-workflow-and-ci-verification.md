---
title: "ADR-037: GitHub App PR Workflow and CI-Based Verification"
type: adr
tags: ["adr","architecture","github","verification","ci","oauth"]
---


# ADR-037: GitHub App PR Workflow and CI-Based Verification

**Status:** Draft
**Date:** 2026-03-19
**Related:** [[ADR-030: Repo-Committed Verification and Commit Hash Caching]], [[ADR-009: No Phases/Stacked Branches]]

---

## Context

### Verification is too expensive to run locally in parallel

The current flow is: worker edits code in a worktree → worker runs full-project verification commands during the session → submits work → verification pipeline runs the same full-project commands again → on failure, respawns the worker with error output → worker runs the commands again to fix.

With 3-4 concurrent agents in separate worktrees, this means multiple full-project builds/test suites competing for CPU on the same machine. Observed impact on the Djinn server project (2026-03-19 Langfuse traces, Rust workspace):

- Individual builds balloon from ~15s to 45-120s due to CPU contention
- Shell commands hit the 120s timeout waiting for compilation
- Tasks that should complete in 5-10 min take 20-30 min
- Agents enter tight fail loops: submit → verify fail → fix → submit → verify fail
- One task consumed 12 sessions and 2M input tokens before PM force-closed it

This problem is not Rust-specific — any project with expensive test suites (large Node monorepos, Python ML pipelines, Java/Gradle builds) will hit the same contention when multiple agents run full verification concurrently. Agents typically only change files in one or two modules, but verification runs against everything.

### Direct-push merges bypass human review

Currently, completed tasks merge via `git push origin {sha}:refs/heads/{target_branch}` — a direct push with no PR, no CI gate, and no human review step. For users who need approval workflows (SOC2, team review policies), this is a non-starter.

### No mechanism for user feedback on agent work

When an agent produces incorrect or suboptimal code, the only feedback path is for the user to manually intervene via the Djinn UI. There is no way for a user to leave review comments on the code that automatically feed back into the agent loop.

---

## Decision

### 1. GitHub App for PR Creation and Management

Djinn publishes a GitHub App. Users install it on their repos during project onboarding. The app gets permissions to:

- Create and update branches
- Create pull requests
- Read and respond to review comments
- Enable auto-merge on PRs
- Read CI check status

**Installation is per-org/account, not per-user.** One installation covers all selected repos for that org.

### 2. OAuth User-to-Server Authentication

Users authenticate via **GitHub App OAuth (user-to-server)** flow during Djinn setup. This is the same pattern used by Linear, Vercel, and similar tools.

Flow:
1. User clicks "Connect GitHub" in Djinn
2. Browser redirect to GitHub OAuth authorize endpoint with the App's client ID
3. User approves → GitHub redirects to local callback (reuse the existing Codex PKCE callback server on port 1455)
4. Djinn exchanges the auth code for a user access token + refresh token
5. Tokens stored encrypted in the credential vault under key `__OAUTH_GITHUB_APP`
6. Refresh flow uses the refresh token grant (same pattern as Codex OAuth)

The installation token (for bot actions) is derived from the user token + installation ID. PRs created this way show as authored by `djinn[bot]`.

**Why not a stored private key:** The App's private key is a master secret that can act on ALL installations. Storing it on user machines is a security risk. The OAuth flow scopes access to only the repos the user has authorized.

### 3. PR-Based Task Completion Flow

Replace direct-push merge with a PR workflow:

```
Worker completes task
  → push branch to origin (existing flow)
  → create PR via GitHub API (new)
  → enable auto-merge (new)
  → CI runs full verification (new)
  → user reviews / approves (new)
  → PR merges (GitHub handles)
  → webhook notifies Djinn (new)
  → next task unblocks
```

The PR description includes: task summary, files changed, acceptance criteria status, and a link back to the Djinn task.

### 4. Review Feedback Loop

When a user requests changes on a PR:

1. GitHub fires a `pull_request_review` webhook
2. Djinn receives it (via polling or webhook endpoint on Djinn Cloud)
3. Djinn extracts review comments and maps them to the task
4. A new worker session spawns with the review feedback as context
5. Worker pushes fixup commits to the same branch
6. Djinn re-requests review via the GitHub API

This loop continues until the user approves or the PM escalation ceiling is hit.

### 5. Scoped Local Verification, Full Verification in CI

Split verification into two tiers:

**Tier 1 — Local (fast, during session and pre-PR):**

Djinn is project-agnostic — verification scoping works via **file-pattern-to-command rules**, not language-specific heuristics. Users define rules in project config that map glob patterns to verification commands:

```yaml
verification_rules:
  # Rust workspace example
  - match: "crates/djinn-mcp/**"
    commands: ["cargo test -p djinn-mcp", "cargo clippy -p djinn-mcp -- -D warnings"]
  - match: "crates/djinn-db/migrations/**"
    commands: ["cargo test -p djinn-db"]

  # Node/TypeScript example
  - match: "src/components/**/*.tsx"
    commands: ["npm run test:components"]
  - match: "*.ts"
    commands: ["npm run test"]

  # Python example
  - match: "src/db/**/*.py"
    commands: ["pytest tests/db/"]

  # Catch-all fallback
  - match: "**"
    commands: ["cargo test --workspace"]
```

**Verification pipeline:**
1. `git diff --name-only` against target branch → list of changed files
2. Match each changed file against rules (glob patterns, evaluated in order)
3. Collect the unique set of commands from all matched rules (deduplicated)
4. Run the collected commands
5. If no specific rules match → run the fallback (`**` rule, or project-level default verification commands)

Rules are evaluated greedily — a file can match multiple rules, and all matched commands are collected. This handles cross-cutting changes naturally (e.g., a file touching both `crates/djinn-mcp/` and `crates/djinn-core/` triggers both crates' tests).

**Integration with ADR-038 specialists:** When a specialist has a `verification_command` set, that command is used *instead of* the pattern-matched rules for tasks assigned to that specialist. The pattern-based rules are the foundation for default Workers; specialists can override with domain-specific verification.

**Tier 2 — CI (authoritative, on PR):**
- CI runs the project's full verification suite (e.g., `cargo clippy --workspace` + `cargo test --workspace`)
- Runs on a dedicated CI runner with no CPU contention
- Auto-merge is gated on this check passing
- CI commands are project-defined, not Djinn-prescribed

This eliminates the parallel-build thrashing problem. Local verification catches obvious errors fast via scoped rules. CI is the authoritative gate running full verification without contention.

### 6. Credential Storage

Reuse the existing credential vault infrastructure:

| Key | Contents | Refresh |
|-----|----------|---------|
| `__OAUTH_GITHUB_APP` | User access token + refresh token (JSON) | Refresh token grant, same as Codex |
| `__GITHUB_INSTALLATION_ID` | Installation ID for the user's org | Static after setup |

The OAuth module gets a new flow variant alongside Copilot and Codex:

```
OAuthFlowKind::GitHubApp { client_id, client_secret }
```

### 7. Webhook Delivery

Two options for receiving GitHub webhooks when running locally:

**Option A — Polling:** Djinn periodically checks PR status via the GitHub API (e.g. every 30s for active PRs). Simple, no infrastructure, works behind NAT.

**Option B — Djinn Cloud relay:** Webhooks hit a Djinn Cloud endpoint, which forwards to the local instance via the existing SSE connection (or a WebSocket). Lower latency, but requires Cloud.

**Decision: Start with polling (Option A).** Add Cloud relay later when Djinn Cloud exists. The polling interval is only active while PRs are open, so the API cost is minimal.

---

## Consequences

### Positive
- Eliminates CPU contention from parallel full-workspace builds
- Human review gate satisfies SOC2 and team review policies
- Review feedback creates a natural agent improvement loop
- PR history provides auditable trail of all agent changes
- CI verification runs on dedicated infrastructure at full speed
- Opens the door to open-sourcing the project (contributors use standard PR flow)

### Negative
- Adds GitHub as a hard dependency for the PR workflow (git-only push remains as fallback)
- OAuth setup adds friction to onboarding
- Polling introduces latency for webhook delivery (30s worst case)
- CI minutes cost money (user's GitHub Actions quota)

### Mitigations
- Keep direct-push merge as a fallback for users who don't install the GitHub App
- OAuth flow reuses existing infrastructure (callback server, credential vault, refresh logic)
- Polling interval is configurable; Cloud relay eliminates it later
- Scoped local verification keeps the fast feedback loop for agents during sessions

---

## Implementation Phases

### Phase 1: Scoped Local Verification
- Add `verification_rules` to project config (file pattern → command mapping)
- Verification pipeline diffs changed files, matches against rules, runs only matched commands
- Fallback rule (`**`) preserves current full-verification behavior for unconfigured projects
- Worker system prompt updated to encourage scoped commands matching the project's rules
- No GitHub integration required — immediate win for build times

### Phase 2: GitHub App OAuth + PR Creation
- Publish Djinn GitHub App
- Implement `OAuthFlowKind::GitHubApp` with PKCE browser redirect
- Store tokens in credential vault
- On task completion: create PR instead of direct-push merge
- Enable auto-merge on PR creation

### Phase 3: CI Verification Gate
- Add GitHub Actions workflow template for Djinn-managed repos
- PR auto-merge gated on CI check passing
- Task unblocks on merge event (polled)
- Remove full-workspace local verification; keep only scoped tier-1

### Phase 4: Review Feedback Loop
- Poll for PR review events on open PRs
- Extract review comments → spawn worker session with feedback
- Worker pushes fixes → re-request review
- PM escalation if review loop exceeds threshold

---

## Relations

- [[ADR-030: Repo-Committed Verification and Commit Hash Caching]] — current verification model being extended
- [[ADR-009: No Phases/Stacked Branches]] — task branches merge to target; PR is the merge mechanism now
- [[ADR-031: Djinn Cloud Metered Inference Proxy]] — Cloud relay for webhooks in Phase 4
