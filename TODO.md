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

## Follow-ups (out of scope for this branch)

- [ ] Thread the authenticated user's GitHub token from
  `user_auth_sessions` into MCP calls so `github_app_installations` /
  `github_list_repos` stop reading the legacy `__OAUTH_GITHUB_APP`
  credential row. Today those tools still fall back to the compat shim
  because MCP handlers don't carry the HTTP session context.
- [ ] One-shot ops cleanup pass to `DELETE FROM credentials WHERE
  key_name = '__OAUTH_GITHUB_APP'` after all deployed servers have
  stopped needing the compat shim.
