# GitHub App Setup

Djinn uses a GitHub **App** (not an OAuth App) for repo access. Installation
access tokens scope what the server can see to whatever accounts/orgs have
installed the App, and any commits created server-side are attributed to
the bot identity `djinn-bot[bot]`.

This takes about 10 minutes. You do it once per deployment.

## 1. Create the App

1. Visit <https://github.com/settings/apps/new> (personal) or
   `https://github.com/organizations/<org>/settings/apps/new` (org-owned).
2. **GitHub App name** — e.g. "Djinn Bot". Must be globally unique.
3. **Homepage URL** — your server's public URL (e.g. `http://127.0.0.1:8372`
   for local dev).
4. **Identifying and authorizing users** → enable
   **Request user authorization (OAuth) during installation**. Callback URL:
   ```
   http://127.0.0.1:8372/auth/github/callback
   ```
   (Swap in your `DJINN_PUBLIC_URL` host for production.)
5. **Webhook** — disable unless you explicitly want webhook events;
   the server does not consume any today. Untick *Active*.
6. **Repository permissions** (minimum):
   - **Contents**: Read & write (clone, push, create branches).
   - **Metadata**: Read-only (listed automatically).
   - **Pull requests**: Read & write (open PRs, enable auto-merge, request
     review).
7. **Account permissions** — leave all at "No access".
8. **Where can this GitHub App be installed?** — "Any account" unless you
   want to restrict to your own org.
9. Click **Create GitHub App**.

## 2. Collect credentials

On the App's settings page you now have:

- **App ID** (numeric, shown at the top). → `GITHUB_APP_ID`.
- **Client ID** and **Generate a new client secret** → `GITHUB_APP_CLIENT_ID`
  and `GITHUB_APP_CLIENT_SECRET`.
- The App's public slug — the last URL segment on its public page
  (`https://github.com/apps/<slug>`). → `GITHUB_APP_SLUG`.
- Scroll to **Private keys** → **Generate a private key**. GitHub will
  download a `.pem` file. Either:
  - store it somewhere safe and set `GITHUB_APP_PRIVATE_KEY_PATH` to that
    path, or
  - paste the full PEM (with literal `\n` newlines or real line breaks)
    into `GITHUB_APP_PRIVATE_KEY` in your `.env`.

## 3. Fill in `.env`

Copy `server/.env.example` to `server/.env` and populate:

```
GITHUB_APP_ID=123456
GITHUB_APP_SLUG=djinn-bot
GITHUB_APP_CLIENT_ID=Iv1.xxxxxxxxxxxx
GITHUB_APP_CLIENT_SECRET=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
GITHUB_APP_PRIVATE_KEY_PATH=/abs/path/to/djinn-bot.2026-04-15.private-key.pem
```

Restart the server. On the first sign-in, if you have not yet installed the
App, click **Connect GitHub** with `?install=1` (or use the `Install Djinn`
button in the desktop app) to land on the install page. Grant it access to
the repos you want Djinn to touch.

## 4. Verify

```sh
curl -s http://127.0.0.1:8372/auth/me
# → 401 until you sign in, then a JSON identity.
```

And from the desktop MCP client:

- `github_app_installations` — should return the installations you granted.
- `github_list_repos` — should now be populated with repos from each
  installation.

## Notes

- Installation tokens are 1-hour credentials. The server caches them
  in-process until T-5 minutes before expiry and re-mints on demand.
- The classic OAuth App device-code flow (`oauth/github_oauth_app_legacy`)
  is retained *only* because the CoordinatorActor still uses a long-lived
  user token for pushes. That migration is tracked in `TODO.md`. No action
  required from operators.
- For now, both the new `GITHUB_APP_CLIENT_ID` / `CLIENT_SECRET` and the
  legacy `GITHUB_OAUTH_CLIENT_ID` / `CLIENT_SECRET` are read. A deprecation
  warning is logged when the legacy names are used.
