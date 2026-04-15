# TODO

## GitHub App migration (feat/github-app follow-ups)

The initial migration landed in `feat/github-app`:
- New `djinn-provider::github_app` module (JWT mint, installation listing +
  token cache, installation-scoped reqwest client).
- `/auth/github/*` now uses the GitHub App's user-to-server OAuth (scopes
  dropped; `?install=1` sends the user to the install page post-auth).
- New MCP tools: `github_app_installations`, `github_app_install_url`.
- `github_list_repos` iterates installations and dedupes; each entry now
  carries `installation_id` and `account_login`.
- `project_add_from_github` clones with an installation token and
  configures `djinn-bot[bot]` as the local git identity.

### Still to do

- [ ] **Migrate the CoordinatorActor push path** to installation tokens.
  Today `dispatch.rs` and `task_merge.rs` still read the legacy user token
  stored under `__OAUTH_GITHUB_APP` (the device-code-flow token) and push
  as the authenticated user. Target: mint an installation token at push
  time and rewrite the remote URL so the push is attributed to
  `djinn-bot[bot]`. Touches: `server/crates/djinn-agent/src/actors/
  coordinator/dispatch.rs`, `server/crates/djinn-agent/src/task_merge.rs`,
  `server/crates/djinn-provider/src/oauth/github_oauth_app_legacy.rs`
  (retirement).
- [ ] **Regenerate desktop MCP types** to pick up the new tools and the
  extended `GithubRepoEntry` (`installation_id`, `account_login`) +
  `ProjectAddFromGithubParams.installation_id`. The new MCP tools added
  in this migration are:
  - `github_app_installations`
  - `github_app_install_url`
- [ ] **Remove the legacy `GITHUB_OAUTH_CLIENT_ID` / `CLIENT_SECRET`
  fallbacks in `server/src/server/auth.rs`** once operators have migrated
  their `.env` (target: next release).
