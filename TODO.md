# TODO

## GitHub App migration

The GitHub App migration landed in `feat/github-app` and was finalised on
`feat/github-app-finalize`:

- `djinn-provider::github_app` module (JWT mint, installation listing +
  token cache, installation-scoped reqwest client, tiny user-token compat
  shim for reading the retired `__OAUTH_GITHUB_APP` credential row).
- `/auth/github/*` uses the GitHub App's user-to-server OAuth (App env
  vars only — no legacy `GITHUB_OAUTH_*` fallback).
- MCP tools: `github_app_installations`, `github_app_install_url`.
- `github_list_repos` iterates installations and dedupes; each entry
  carries `installation_id` and `account_login`.
- `project_add_from_github` clones with an installation token, persists
  the `installation_id` on the `projects` row (Migration 4), and
  configures `djinn-bot[bot]` as the local git identity.
- Coordinator push + PR creation reads the cached `installation_id` from
  the project row and uses an installation-scoped `GitHubApiClient`; no
  user token required.
- Provider OAuth MCP flow for `github-app` is gone — the device-code
  variant was retired.

## GitHub App migration complete

Finalised on `feat/github-app-cleanup`:

- HTTP MCP handler authenticates the incoming request and scopes the
  dispatch under a `tokio::task_local!` (`djinn_core::auth_context::
  SESSION_USER_TOKEN`) carrying the session's `github_access_token`.
- MCP tools (`github_app_installations`, `github_list_repos`,
  `project_add_from_github`, `github_search`, `github_fetch_file`) read
  that task-local via `current_user_token()`. No task-local → clear
  "sign in with GitHub required" error.
- `djinn-provider::github_app::user_token_compat` is deleted. The
  `GitHubApiClient::new(cred_repo)` / `with_base_url(cred_repo, _)`
  constructors are replaced by `for_session_user()` and the
  installation-scoped `for_installation[_with_base_url]`; the
  `AuthMode::UserToken { cred_repo }` variant is gone.
- Old host-path `project_add` MCP tool + `addProject(path)` desktop
  helper retired (no UI callers remained).
- `catalog::builtin::is_oauth_key_present` no longer recognises
  `GITHUB_APP_TOKEN` — the device-code flow is fully gone.

## Ops note

Existing deployments may still carry a stale `__OAUTH_GITHUB_APP` row in
the `credentials` table. It's unused and harmless; operators who want to
clean up can run `DELETE FROM credentials WHERE key_name =
'__OAUTH_GITHUB_APP'`. The `provider_remove` flow for `github_app` also
scrubs this row as a best-effort on user-initiated removal.
