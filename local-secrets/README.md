# `local-secrets/`

Untracked drop zone for sensitive files referenced by `kubectl create secret
--from-file` during local kind smoke tests. Everything in this directory
(other than this README and `.gitkeep`) is gitignored — see the matching
rule in the repo root `.gitignore`.

## What goes here

| File | Source | Used by |
|---|---|---|
| `github-app.private-key.pem` | Download from `github.com/settings/apps/<your-app>` → "Private keys" → "Generate a private key" | `kubectl create secret generic djinn-github-app --from-file=private-key.pem=local-secrets/github-app.private-key.pem` |
| `vault.key` *(optional)* | `openssl rand -out local-secrets/vault.key 32` | `kubectl create secret generic djinn-vault-key --from-file=vault.key=local-secrets/vault.key` — only if you want to pin a specific AES key; otherwise the Helm chart auto-generates one. |

## Example: create the GitHub App secret for the kind smoke test

```bash
kubectl create secret generic djinn-github-app --namespace djinn \
  --from-literal=app-id='<APP_ID>' \
  --from-literal=client-id='<CLIENT_ID>' \
  --from-literal=client-secret='<CLIENT_SECRET>' \
  --from-file=private-key.pem=local-secrets/github-app.private-key.pem
```

That's the canonical first-time secret. The deployment-to-org binding
(`GITHUB_ORG_LOGIN` / `GITHUB_INSTALLATION_ID`) is **optional** here:
the UI's installation picker handles it on first visit by listing
installations from `GET /app/installations` and writing the binding
into `org_config` when you click a row.

### Optional — pre-bind for CI / fully-automated deploys

Skip the UI picker by adding the binding keys to the Secret:

```bash
kubectl create secret generic djinn-github-app --namespace djinn \
  --from-literal=app-id='<APP_ID>' \
  --from-literal=client-id='<CLIENT_ID>' \
  --from-literal=client-secret='<CLIENT_SECRET>' \
  --from-literal=org-login='<ORG_LOGIN>' \
  --from-literal=installation-id='<INSTALLATION_ID>' \
  --from-file=private-key.pem=local-secrets/github-app.private-key.pem
```

Get the `installation-id` by opening
`https://github.com/organizations/<ORG_LOGIN>/settings/installations`
(or `https://github.com/settings/installations` for personal
accounts) → "Configure" on the Djinn App → numeric tail of the URL.
An optional `--from-literal=org-id='<NUMERIC_ID>'` can be added too.
When env binding is present the picker is skipped entirely (the env
binding wins over any DB row).

## Why a separate directory

Keeps secrets out of the repo root where stray `git add -A` could pick
them up, and gives `kubectl create secret --from-file` a stable relative
path that works the same on every developer's machine.
