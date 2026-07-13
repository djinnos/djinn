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
  allowUserInstallations: true
```

With no `.tilt/github-app/` files, the chart deliberately omits the GitHub App
Secret, `GITHUB_APP_*` env variables, private-key path, and volume. That is an
*absent* Secret source, so persisted credentials from the manifest flow can
hot-load. If any inline credential file/value is present — or
`secrets.githubApp.existingSecret` is set — the chart renders the whole
Secret/env surface. A partial or malformed attempted configuration is therefore
still fatal and never silently falls through to self-setup.

`allowUserInstallations` is a local/solo-development opt-in. Production values
default it to `false`, preserving organization-only deployment binding.

### Isolated Tilt validation instance

To validate onboarding without touching an existing `kind-djinn` cluster,
default `.tilt/` state, local registry, or host forwards, use a distinct cluster,
registry, state directory, and ports. Bootstrap first so Tilt's production-
context guard sees the new kind context:

```bash
CLUSTER_NAME=djinn-validation \
REG_NAME=kind-registry-validation \
REG_PORT=15001 \
  bash scripts/kind/setup-kind.sh

TILT_ARGS=(
  --cluster-name djinn-validation
  --registry-name kind-registry-validation
  --registry-port 15001
  --state-dir /var/tmp/djinn-tilt-validation
  --api-port 13000
  --rpc-port 18443
  --postgres-port 15432
  --qdrant-http-port 16333
  --qdrant-grpc-port 16334
  --langfuse-port 15000
  --minio-port 19091
)

tilt up --context kind-djinn-validation --port 11350 -- "${TILT_ARGS[@]}"
```

The generated GitHub callback and web URLs use `http://localhost:13000`, and
Langfuse uses `http://localhost:15000`. Teardown must use the same Tiltfile
arguments; omitting them selects the historical defaults instead:

```bash
tilt down --context kind-djinn-validation -- "${TILT_ARGS[@]}"
kind delete cluster --name djinn-validation
docker rm -f kind-registry-validation
rm -rf /var/tmp/djinn-tilt-validation
```

### Steps

The current self-setup manifest is a **personal-account local-development
flow**. It posts to GitHub's personal App-registration endpoint and creates a
private App, so GitHub permits installation only on the owning personal
account. Organization-owned self-setup needs an organization-specific manifest
endpoint and is not implemented yet; pre-create the organization App and use
the Secret-based configuration path for that deployment shape. The initial
`org_config` binding must also be provisioned through an operator-controlled
migration; an uncorrelated public setup callback is deliberately not allowed to
create the deployment's first binding.

1. **Start the server.** On first boot with self-setup enabled and no usable
   credentials, the server generates a **one-time setup token** and prints a
   setup URL to the boot log:

   ```
   GitHub App not configured. Complete setup at:
     http://localhost:3000/auth/github/create-app?setup_token=<setup-token>
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
   app name, callback URLs, permissions, visibility, and OAuth-on-install.
   Webhooks remain disabled and the manifest does not submit a webhook URL.

3. **Click "Create GitHub App for \<personal-account\>".** GitHub creates the App and
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
   continuation and creates the bootstrap admin user. A fresh deployment then
   continues through provider/model setup and repository setup before showing
   the task board.

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

The chart renders a Kubernetes Secret named `<release>-github-app` with keys
`app-id`, `client-id`, `client-secret`, and `private-key.pem`. The Deployment
template maps these to `GITHUB_APP_*` env vars and mounts the private key
file — the server reads them from there automatically.

### Option B: Pre-existing Secret

Point `existingSecret` at a Secret you manage out-of-band (e.g. via Sealed
Secrets, External Secrets Operator, or Vault Agent Injector):

```yaml
secrets:
  githubApp:
    existingSecret: "my-github-app-secret"
```

The Secret must expose these keys (matching the chart-managed Secret's layout):

| Key | Value | How the chart uses it |
|-----|-------|----------------------|
| `app-id` | Numeric App ID (e.g. `123456`) | Mapped to `GITHUB_APP_ID` env var |
| `client-id` | OAuth client ID (e.g. `Iv1.xxxxxxxxxxxx`) | Mapped to `GITHUB_APP_CLIENT_ID` env var |
| `client-secret` | OAuth client secret | Mapped to `GITHUB_APP_CLIENT_SECRET` env var |
| `private-key.pem` | PEM-encoded RSA private key | Mounted at `GITHUB_APP_PRIVATE_KEY_PATH` (`/var/run/secrets/djinn/github-app/private-key.pem`) |

> **Important:** The Secret keys use **lowercase kebab-case** (`app-id`, not
> `GITHUB_APP_ID`). The Deployment template reads these keys and exposes
> them to the server container as `GITHUB_APP_*` environment variables.
> The private key is mounted as a file, not read from an env var.
>
> If you need to provide credentials via direct environment variables
> instead of the chart's Secret mechanism (e.g. using `extraEnv` or
> `extraEnvFrom`), set `GITHUB_APP_ID`, `GITHUB_APP_CLIENT_ID`,
> `GITHUB_APP_CLIENT_SECRET`, and `GITHUB_APP_PRIVATE_KEY_PATH` directly.
> Keep this separate from `secrets.githubApp.existingSecret`, which only
> supports the key names listed above.

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

If the mounted Kubernetes Secret exposes the chart's `app-id` / `client-id` /
`client-secret` keys (mapped to `GITHUB_APP_*` env vars) but the values are
invalid or incomplete (e.g. malformed PEM, missing client secret):

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

The token is passed as a query parameter (`?setup_token=<setup-token>`) in the
boot-log setup URL. It gates access to `/auth/github/create-app` so that
only the operator who can read the server logs can initiate the manifest
flow.

---

## GitHub App permissions

The manifest flow pre-configures these permissions on the GitHub App:

| Permission | Access level | Rationale |
|------------|-------------|-----------|
| **Actions** | Read-only | Read workflow runs, jobs, and logs for CI diagnostics |
| **Checks** | Read-only | Read check runs, suites, and annotations |
| **Contents** | Read & write | Clone repos, push branches, create commits |
| **Members** | Read-only | Verify organization membership and reconcile access |
| **Metadata** | Read-only | Required by GitHub; always granted automatically |
| **Pull requests** | Read & write | Open PRs, enable auto-merge, request reviews |

**Organization permissions:** Members read-only is the sole organization-level
grant. Djinn uses it to enforce organization membership and reconcile access;
it is not requested for personal-account installations. No account-level write
permissions are required.

**Who can install:** The manifest creates a private personal-account App, so
GitHub restricts installation to the owning account. Organization deployments
must currently use the manual/Secret path described above.

### Webhook status

The manifest omits `hook_attributes` entirely, so GitHub leaves the webhook
disabled. Djinn does not consume webhook events today — the server polls or
uses on-demand API calls for status updates. GitHub validates every webhook URL
that is included in a manifest even when `active` is false, so submitting a
`localhost` target breaks local onboarding. [GitHub's webhook guidance](https://docs.github.com/en/apps/creating-github-apps/registering-a-github-app/using-webhooks-with-github-apps)
confirms that an App with webhooks turned off does not need a webhook URL.

If you configure the App manually (not via the manifest flow), also leave
the webhook inactive.

---

## Verification checklists

### Fresh local / Tilt setup

Complete this checklist after a fresh `tilt up` with no
`.tilt/github-app/` credential files:

- [ ] `values.local.yaml` has `env.enableSelfSetup: true`
- [ ] Server boot log contains a line with the setup URL:
      `http://localhost:3000/auth/github/create-app?setup_token=<token>`
- [ ] Opening the setup URL in a browser redirects to GitHub's
      "Create GitHub App" manifest page
- [ ] Clicking "Create GitHub App" redirects back to Djinn's
      `/auth/github/app-manifest-callback`
- [ ] Djinn persists the returned credentials and redirects to the
      GitHub App install/authorize page
- [ ] After granting repo access, the OAuth callback completes and creates the
      bootstrap admin
- [ ] Provider/model setup and first-repository setup complete before the user
      lands on the task board
- [ ] Subsequent sign-ins work without re-running setup
- [ ] `/setup/status` reports `appCredentialsConfigured: true`

### Production Secret override

Complete this checklist after deploying with a Helm-managed Secret:

- [ ] `env.enableSelfSetup` is `false` (default) — setup routes are
      not exposed
- [ ] The `djinn-github-app` Secret (or `existingSecret`) contains all
      required keys (`app-id`, `client-id`, `client-secret`,
      `private-key.pem`)
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
