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

## Why a separate directory

Keeps secrets out of the repo root where stray `git add -A` could pick
them up, and gives `kubectl create secret --from-file` a stable relative
path that works the same on every developer's machine.
