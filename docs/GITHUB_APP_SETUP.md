# GitHub App Setup

Djinn uses a GitHub **App** (not an OAuth App) for repo access. Installation
access tokens scope what the server can see to whatever accounts/orgs have
installed the App, and any commits created server-side are attributed to the
bot identity `djinn-bot[bot]`.

There are two supported setup paths:

| Path | When to use | Requires browser? |
|------|-------------|-------------------|
| **Manifest bootstrap** | Fresh local/Tilt deployments; self-serve evaluation | Yes — two GitHub clicks |
| **Production Secret** | Production clusters; infra-as-code; CI/CD-managed secrets | No — values come from Helm or a pre-created Kubernetes Secret |

---

## Credential-source precedence

**The Kubernetes Secret (Helm `secrets.githubApp.*` / env vars) always takes
precedence over persisted credentials.** The historical claim that "persisted
credentials always win over env vars" is no longer accurate.

The server resolves GitHub App credentials in this deterministic order:

1. **Secret / env vars** (`GITHUB_APP_ID`, `GITHUB_APP_CLIENT_ID`,
   `GITHUB_APP_CLIENT_SECRET`, `GITHUB_APP_PRIVATE_KEY`) — loaded from the
   mounted Kubernetes Secret or direct environment variables.
   - If **any** core `GITHUB_APP_*` variable is attempted (present in the
     environment, even if set-but-empty) and the resulting configuration is
     **invalid or incomplete**, the server treats this as a **fatal error**.
     It will **not** silently fall through to persisted credentials or
     self-setup. The operator must fix the Secret and restart.
   - If all required fields are present and valid, the Secret is used
     regardless of what persisted credentials may exist.

2. **Persisted credentials** — previously saved via the manifest setup flow,
   stored in the encrypted credential vault (`__GITHUB_APP_CONFIG`).
   - These are only consulted when no Secret/env source is configured.
   - If the persisted blob **exists but cannot be decrypted** (wrong vault
     key, corrupted ciphertext), the server surfaces a
     `credentials_unrecoverable` state. It does **not** fall through to
     self-setup.

3. **Self-setup** (manifest bootstrap) — only available when
   `DJINN_ENABLE_SELF_SETUP=true` **and** neither a valid Secret nor valid
   persisted credentials exist.

| Secret/env present | Secret valid | Persisted valid | Self-setup flag | Effective state | Setup routes exposed? |
|---|---|---|---|---|---|
| Yes | Yes | Any | Any | Secret credentials used | No |
| Yes | No | Any | Any | **Fatal: invalid Secret** | No |
| No | — | Yes (decryptable) | Any | Persisted credentials used | No |
| No | — | Yes (undecryptable) | Any | `credentials_unrecoverable` | No |
| No | — | No | `true` | Self-setup available | Yes |
| No | — | No | `false`/unset | Unconfigured | No |

---

## Path 1: Manifest bootstrap (self-serve / local)

The manifest bootstrap path lets you create and configure a GitHub App from
your browser with zero manual credential entry. It is gated behind
`DJINN_ENABLE_SELF_SETUP=true` because it exposes setup routes that should
not be accessible on production deployments managed via infrastructure-as-code.

### Prerequisites

- A Djinn deployment running with `DJINN_ENABLE_SELF_SETUP=true`.
- No GitHub App Secret mounted (or all `GITHUB_APP_*` env vars unset).
- The server must be reachable from your browser at its `DJINN_PUBLIC_URL`.

For local/Tilt, `values.local.yaml` already sets `env.enableSelfSetup: true`:

```yaml
env:
  enableSelfSetup: true
  publicUrl: "http://localhost:3000"
```

### Steps

1. **Start the server.** On first boot with self-setup enabled and no usable
   credentials, the server generates a **one-time setup token** and prints a
   setup URL to the boot log:

   ```
   GitHub App not configured. Complete setup at:
     http://localhost:3000/auth/github/create-app?token=<setup-token>
   ```

   > **Setup token handling:**
   > - The token is **one-time use** — it is consumed when you open the URL.
   > - It is logged at boot time only. Do not paste it into random UI fields.
   > - The UI will never display or request the raw token.
   > - If the token expires or is leaked, restart the server to generate a
   >   fresh one.
   > - The server stores only a digest of the token, never the raw value.

2. **Open the setup URL in your browser.** The server redirects you to
   GitHub's "Create GitHub App" manifest page with every field pre-filled:
   app name, callback URL, permissions, webhook target, and events.

3. **Click "Create GitHub App for \<account\>".** GitHub creates the App and
   redirects back to Djinn's `/auth/github/app-manifest-callback` carrying a
   one-time `code`.

4. **Djinn exchanges the code.** The server POSTs the code to GitHub's
   manifest conversion endpoint, receives the App's id, slug, client
   id/secret, webhook secret, and private key, then **persists them in the
   encrypted credential vault** and hot-reloads the configuration into the
   running process — no restart required.

5. **Install the App.** After the manifest exchange succeeds, you are
   redirected to GitHub's App install/authorize page. Grant the App access
   to the repositories you want Djinn to manage.

6. **OAuth callback.** GitHub redirects back to Djinn's
   `/auth/github/app-setup-callback`, which completes the install
   continuation, creates the bootstrap admin user, and lands you on the
   task board.

The setup-session cookie (`djinn_setup_session`) is invalidated after
successful credential persistence. If the manifest exchange fails before
credentials are saved, the setup session remains valid so you can retry
without restarting.

### After bootstrap

Once the manifest flow completes and credentials are persisted:

- The setup routes (`/auth/github/create-app`, `/auth/github/app-manifest-callback`)
  are no longer reachable (no valid setup session/token).
- Subsequent boots load persisted credentials from the vault.
- You can later override with a production Secret to replace persisted
  credentials (Secret always wins).

---

## Path 2: Production Secret

For production deployments, provide GitHub App credentials through the Helm
chart's `secrets.githubApp` values or a pre-existing Kubernetes Secret.
Self-setup is **disabled by default** (`env.enableSelfSetup: false`) and
setup routes are never exposed.

### Option A: Helm values (chart-managed Secret)

Set the credentials in your values override:

```yaml
secrets:
  githubApp:
    appId: "123456"
    clientId: "Iv1.xxxxxxxxxxxx"
    clientSecret: "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
    privateKey: |
      -----BEGIN RSA PRIVATE KEY-----
      ...
      -----END RSA PRIVATE KEY-----
```

The chart renders a Kubernetes Secret named `<release>-github-app` and mounts
it on the server Deployment. The server reads the credentials from the
corresponding `GITHUB_APP_*` environment variables.

### Option B: Pre-existing Secret

Point `existingSecret` at a Secret you manage out-of-band (e.g. via Sealed
Secrets, External Secrets Operator, or Vault Agent Injector):

```yaml
secrets:
  githubApp:
    existingSecret: "my-github-app-secret"
```

The Secret must expose these keys:

| Key | Value |
|-----|-------|
| `GITHUB_APP_ID` | Numeric App ID (e.g. `123456`) |
| `GITHUB_APP_SLUG` | App's public slug (e.g. `my-djinn-bot`) |
| `GITHUB_APP_CLIENT_ID` | OAuth client ID (e.g. `Iv1.xxxxxxxxxxxx`) |
| `GITHUB_APP_CLIENT_SECRET` | OAuth client secret |
| `GITHUB_APP_PRIVATE_KEY` | PEM-encoded RSA private key |

> **Note:** The slug is used for constructing the install URL
> (`https://github.com/apps/<slug>`). All other fields are required for
> authentication and API access.

### First install

After deploying with a valid Secret, open the Djinn web client. If you have
not yet installed the App on your GitHub account/org, click **Connect
GitHub** with `?install=1` to land on the App's install page. Grant access
to the repos you want Djinn to touch.

### Self-setup stays off

When `env.enableSelfSetup` is `false` (the production default), the server
never exposes setup routes or self-setup affordances. Even if credentials are
missing, the UI shows the manual setup guidance — not a setup CTA. This
prevents accidental self-setup on production clusters where credentials
should come from infrastructure.

---

## Recovery and reset

### Undecryptable persisted credentials (`credentials_unrecoverable`)

If previously persisted credentials exist but cannot be decrypted — for
example because the vault encryption key was lost, rotated incorrectly, or the
ciphertext is corrupted — the server surfaces a `credentials_unrecoverable`
state.

**Symptoms:**

- The UI shows: *"Stored credentials cannot be recovered"*
- `/setup/status` returns `credentials_unrecoverable: true` and/or
  `setup_state: "credentials_unrecoverable"`

**Resolution options:**

1. **Restore the vault key** — if you have a backup of the AES key used by
   `djinn-db`'s credential vault (stored in the `djinn-vault-key` Kubernetes
   Secret by the Helm chart), restore it and restart the server. The
   persisted credentials will decrypt and load normally.

2. **Clear persisted credentials and reconfigure** — if the vault key is
   lost and cannot be restored:
   - Clear the persisted `__GITHUB_APP_CONFIG` entry from the credential
     vault (this requires database access; consult your DBA or use the
     server's admin tooling).
   - Then either:
     - Provide credentials via a production Secret (Path 2 above), or
     - Enable self-setup (`DJINN_ENABLE_SELF_SETUP=true`) and restart to
       trigger a fresh manifest bootstrap.

> **Important:** The server does **not** silently fall through from
> undecryptable persisted credentials to self-setup. You must explicitly
> clear the persisted entry or provide a valid Secret.

### Invalid or incomplete Secret

If the mounted Kubernetes Secret contains `GITHUB_APP_*` variables but the
values are invalid or incomplete (e.g. malformed PEM, missing client secret):

- The server treats this as **fatal** — it will not start accepting requests
  that depend on GitHub App credentials.
- The UI shows: *"GitHub App Secret is invalid"*
- **Fix:** Correct the Secret values and restart the server.

### Vault key management

The Helm chart provisions the vault key automatically on first install:

```yaml
secrets:
  vaultKey:
    existingSecret: ""
    key: ""  # chart auto-generates a 32-byte AES key
```

- On `helm upgrade`, the chart preserves the existing key from the Secret
  so data encrypted under it stays decryptable.
- Set `key` explicitly to pin a specific key (e.g. for cross-environment
  sharing or backup restore).
- Set `existingSecret` to manage the key out-of-band.

**Back up the vault key.** If you lose it and have persisted credentials,
they become undecryptable (see above).

---

## Setup token handling

When `DJINN_ENABLE_SELF_SETUP=true` and no usable credentials exist, the
server generates a one-time setup token at boot.

| Property | Value |
|----------|-------|
| **Generation** | Server boot, when self-setup is enabled and no credentials are available |
| **Storage** | Server stores only a **digest** (hash) of the token, never the raw value |
| **Lifetime** | Valid until consumed or until the server restarts |
| **Usage** | Single-use — opening the setup URL consumes the token |
| **Exposure** | Logged at boot time; the UI **never** displays or requests the raw token |
| **Rotation** | Restart the server to generate a fresh token |
| **Leak response** | If the token is leaked before use, restart the server to invalidate it |

The token is passed as a query parameter (`?token=<setup-token>`) in the
boot-log setup URL. It gates access to `/auth/github/create-app` so that
only the operator who can read the server logs can initiate the manifest
flow.

---

## GitHub App permissions

The manifest flow pre-configures these permissions on the GitHub App:

| Permission | Access level | Rationale |
|------------|-------------|-----------|
| **Contents** | Read & write | Clone repos, push branches, create commits |
| **Metadata** | Read-only | Required by GitHub; always granted automatically |
| **Pull requests** | Read & write | Open PRs, enable auto-merge, request reviews |

**Account permissions:** Leave all at "No access". The App operates on
repositories, not account-level settings.

**Who can install:** "Any account" unless you want to restrict to your own
organization.

### Webhook status

The webhook is configured as **inactive** (the "Active" checkbox is
unticked). Djinn does not consume webhook events today — the server polls
or uses on-demand API calls for status updates. Leaving the webhook inactive
avoids unnecessary event delivery and the need to expose a webhook endpoint.

If you configure the App manually (not via the manifest flow), also leave
the webhook inactive.

---

## Verification checklists

### Fresh local / Tilt setup

Complete this checklist after a fresh `tilt up` with no
`.tilt/github-app/` credential files:

- [ ] `values.local.yaml` has `env.enableSelfSetup: true`
- [ ] Server boot log contains a line with the setup URL:
      `http://localhost:3000/auth/github/create-app?token=<token>`
- [ ] Opening the setup URL in a browser redirects to GitHub's
      "Create GitHub App" manifest page
- [ ] Clicking "Create GitHub App" redirects back to Djinn's
      `/auth/github/app-manifest-callback`
- [ ] Djinn persists the returned credentials and redirects to the
      GitHub App install/authorize page
- [ ] After granting repo access, the OAuth callback completes and
      the user lands on the task board (bootstrap admin created)
- [ ] Subsequent sign-ins work without re-running setup
- [ ] `/setup/status` reports `appCredentialsConfigured: true`

### Production Secret override

Complete this checklist after deploying with a Helm-managed Secret:

- [ ] `env.enableSelfSetup` is `false` (default) — setup routes are
      not exposed
- [ ] The `djinn-github-app` Secret (or `existingSecret`) contains all
      required keys (`GITHUB_APP_ID`, `GITHUB_APP_CLIENT_ID`,
      `GITHUB_APP_CLIENT_SECRET`, `GITHUB_APP_PRIVATE_KEY`)
- [ ] Server starts without errors related to GitHub App configuration
- [ ] `/setup/status` reports `appCredentialsConfigured: true` and
      `credentialSource: "secret"`
- [ ] If the Secret contains invalid/incomplete values, the server
      reports a fatal error (not a silent fallback to self-setup or
      persisted credentials)
- [ ] Self-setup routes (`/auth/github/create-app`) return 404 or are
      not reachable

### Vault-key-loss recovery

Complete this checklist to verify the `credentials_unrecoverable` path:

- [ ] With valid persisted credentials, stop the server and replace the
      vault key with a different 32-byte AES key
- [ ] Restart the server — `/setup/status` reports
      `credentialsUnrecoverable: true` and/or
      `setup_state: "credentials_unrecoverable"`
- [ ] The UI shows the "Stored credentials cannot be recovered" screen
- [ ] **Recovery path A (restore key):** Restore the original vault key
      and restart — persisted credentials load normally
- [ ] **Recovery path B (clear + re-setup):** Clear the persisted
      `__GITHUB_APP_CONFIG` entry, enable self-setup, and restart —
      the manifest bootstrap flow is available again

---

## Migration notes

- The historical claim that "persisted credentials always win over env vars"
  is removed. The actual precedence is **Secret first, persisted second,
  self-setup third** (see [Credential-source precedence](#credential-source-precedence)).
- The `.tilt/github-app/` file-based credential path is still supported as
  an override for operators who already have credentials, but it is **not**
  the default local self-serve path. The manifest bootstrap (self-setup) is
  the recommended path for fresh local deployments.
- Previous releases accepted `GITHUB_OAUTH_CLIENT_ID` /
  `GITHUB_OAUTH_CLIENT_SECRET` as fallbacks. Those have been removed; only
  `GITHUB_APP_CLIENT_ID` / `GITHUB_APP_CLIENT_SECRET` are honoured.
- Installation tokens are 1-hour credentials. The server caches them
  in-process until T-5 minutes before expiry and re-mints on demand.
- Projects cache their `installation_id` on the `projects` row at
  `project_add_from_github` time so the push path never needs a user token.
