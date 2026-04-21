# Djinn Dev Container Features

Monorepo directory for the Dev Container Features Djinn publishes to
`ghcr.io/djinnos/<feature-id>`. Nine Features live here:

| Feature | Purpose |
|---|---|
| `djinn-agent-worker` | The musl-static worker binary + essentials (git, bash, tini). Required for every project. |
| `djinn-rust` | `rustup` + rust-analyzer (SCIP built in). |
| `djinn-typescript` | Node (via nvm) + typescript-language-server + `scip-typescript` + chosen PM. |
| `djinn-python` | Python (uv or pyenv) + pyright + `scip-python`. |
| `djinn-go` | Go tarball + gopls + `scip-go`. |
| `djinn-java` | SDKMAN + JDK + gradle/maven + jdtls + `scip-java`. |
| `djinn-clang` | clang + clangd + `scip-clang` (GitHub release binary). |
| `djinn-ruby` | rbenv + ruby-lsp + `scip-ruby`. |
| `djinn-dotnet` | .NET SDK + csharp-ls + `scip-dotnet`. |

Each language Feature sets `installsAfter: ["ghcr.io/djinnos/djinn-agent-worker"]`
so the worker layer is always present before the language tooling runs.

## Layout

```
devcontainer-features/
|-- src/
|   |-- djinn-agent-worker/
|   |   |-- devcontainer-feature.json
|   |   |-- install.sh
|   |   `-- bin/.gitkeep             # CI copies djinn-agent-worker here pre-publish
|   `-- djinn-<lang>/
|       |-- devcontainer-feature.json
|       `-- install.sh
|-- test/
|   |-- _global/scenarios.json       # scenario matrix for `devcontainer features test`
|   `-- <feature-id>/test.sh         # per-Feature smoke check
|-- .gitignore
`-- README.md (this file)
```

## Adding or updating a Feature

1. Create `src/djinn-<name>/` with `devcontainer-feature.json` and `install.sh`.
   - `install.sh` must start with `#!/usr/bin/env bash` and `set -euo pipefail`.
   - Detect the package manager with the helper pattern used by the existing
     Features (apt/apk/dnf/yum) and log a warning on unknown bases — do not
     fail hard.
   - Every `version`-style option is a free-form string; pass it through to the
     upstream installer (`rustup toolchain install "$TOOLCHAIN"`,
     `nvm install "$NODE_VERSION"`, etc.) without trying to validate.
2. Add a scenario to `test/_global/scenarios.json` and a `test/<name>/test.sh`
   that asserts the expected binaries appear on PATH.
3. Bump the `version` field in `devcontainer-feature.json` following semver.
4. Refresh version defaults if upstream has moved on — the
   `scripts/verify-feature-versions.sh` CI step logs drift warnings against
   upstream release APIs; when it warns, update defaults here and commit.

## Local testing

The upstream `@devcontainers/cli` runs the scenario matrix end-to-end:

```bash
# one-shot install
npm install -g @devcontainers/cli

# run all Features against the Ubuntu 22.04 base
devcontainer features test \
    --project-folder devcontainer-features \
    --features djinn-agent-worker \
    --features djinn-rust
```

Because each build pulls fresh base images, full runs take ~10 minutes locally.
CI runs this in parallel per-Feature jobs on tagged releases.

## CI publish flow

`.github/workflows/features-publish.yml` handles publishing:

1. **Build worker binary** job: compiles
   `server/crates/djinn-agent-worker` as `x86_64-unknown-linux-musl`, uploads
   the binary as an artifact.
2. **Verify versions** job: runs `scripts/verify-feature-versions.sh` which
   curls upstream release APIs and logs warnings when the defaults in
   `devcontainer-feature.json` are behind.
3. **Publish Features** job: downloads the worker artifact into
   `src/djinn-agent-worker/bin/djinn-agent-worker`, validates the Feature JSON
   shape, then invokes `devcontainers/action@v1` with
   `features-namespace: djinnos` to push to `ghcr.io/djinnos/<feature-id>`.

Triggers:

- `push` to `main` or `wip/multiuser-refactor-snapshot` under
  `devcontainer-features/**` or `server/crates/djinn-agent-worker/**`.
- Manual `workflow_dispatch`.
- Tag matching `feature-v*` (the actual publish happens here — see §8 of
  `/home/fernando/.claude/plans/phase3.5-devcontainer-features.md`).

The first publish happens when `feature-v1.0.0` is tagged after PR 9 merges.
Until then, the workflow builds + lints but does not push.

## Musl / glibc note

`djinn-agent-worker` is deliberately built as `x86_64-unknown-linux-musl` so the
same binary runs on Alpine/musl and Debian/glibc bases. The language Features
use whatever upstream toolchains ship (typically glibc) — a consequence is that
`djinn-rust`, `djinn-typescript`, etc. do **not** promise Alpine compatibility.
If you need Alpine, the worker will run; language tooling may not.
