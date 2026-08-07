# `make tool-goldens` task-run-pod verification

> **STATUS: NOT YET EXECUTED — this file is a template, not evidence.**
>
> Every field below is a placeholder. No task-run pod has produced this
> transcript. Proposal `3vqc` acceptance criteria **AC5** and **AC6** are
> **NOT MET** until a real scheduler-created task-run pod fills this in.
>
> Do not read the tables below as a passing result, do not cite this file as
> proof that `make tool-goldens` runs in a pod, and do not mark AC5/AC6 met on
> the strength of it. A local developer worktree run is **not** a substitute
> and must not be pasted in here: the whole point of this outcome is that the
> command works in the environment agents actually get.

## What this file is for

Proposal `3vqc` requires proof that a tool-schema author working inside a
normal task-run pod can refresh every manifest-declared MCP artifact with one
command, and that doing it twice is a no-op the second time. A command that
only works on a maintainer's laptop does not close the loop the proposal
exists to close.

## Required environment

| Requirement | Value |
| --- | --- |
| Pod | A normal **scheduler-created task-run pod** for `djinnos/djinn`. Not a bespoke pod, not a debug shell built by hand. |
| Image | The runtime image from the deployment's effective `environment.image.runtime` configuration. |
| Starting tree | The candidate commit, clean: `git status --porcelain` must be empty before step 3. |
| Build outputs | Must **not** be pre-populated by hand. The normal task workspace and the configured shared dependency caches are allowed — they are part of the production task-run profile. |
| Database | Not required. Every Rust producer declares `SQLX_OFFLINE=true`. |
| Network | Allowed only as normal task-run pods allow it, for the locked `pnpm install` that `scripts/regenerate-tool-goldens.mjs` performs when `ui/node_modules` is absent. |
| Toolchain | Whatever the runtime image and the repository's toolchain declarations supply. Do not install a different Rust or pnpm to make the run pass. |
| Timeout | **30 minutes per `make tool-goldens` invocation.** |

## Provenance to record

| Field | Value |
| --- | --- |
| Runtime image digest | `TBD (sha256:…)` |
| Candidate commit SHA | `TBD` |
| Pod / task-run identifier | `TBD` |
| Run date (UTC) | `TBD` |
| `rustc --version` | `TBD` |
| `cargo --version` | `TBD` |
| `node --version` | `TBD` |
| `pnpm --version` | `TBD` |

## Commands to run, in order

```sh
# 1. provenance
rustc --version
cargo --version
node --version
pnpm --version

# 2. the tree must be clean before anything is regenerated
git status --porcelain

# 3. first regeneration (30 min budget, must exit 0)
make tool-goldens

# 4. second regeneration (30 min budget, must exit 0)
make tool-goldens

# 5. the second run must have changed nothing under the manifest's artifacts
git diff --exit-code -- $(node scripts/check-tool-goldens.mjs --paths)

# 6. the committed guard must agree (must exit 0)
make tool-goldens-check
```

## Results to record

| # | Command | Started (UTC) | Finished (UTC) | Duration | Exit code |
| --- | --- | --- | --- | --- | --- |
| 2 | `git status --porcelain` | TBD | TBD | TBD | TBD (and empty output) |
| 3 | `make tool-goldens` (first) | TBD | TBD | TBD | TBD (required: 0) |
| 4 | `make tool-goldens` (second) | TBD | TBD | TBD | TBD (required: 0) |
| 5 | `git diff --exit-code -- <manifest paths>` | TBD | TBD | TBD | TBD (required: 0) |
| 6 | `make tool-goldens-check` | TBD | TBD | TBD | TBD (required: 0) |

### Verbatim output of step 5 (zero-diff assertion)

```text
TBD — paste the exact output. It must be empty.
```

### Verbatim output of step 6 (`make tool-goldens-check`)

```text
TBD — paste the exact output.
```

## What counts as a failure

This outcome **fails**, and must be recorded as a failure rather than written
up as a pass, if any of the following happens:

- the `pnpm install` prerequisite fails;
- a declared toolchain is missing from the image;
- either `make tool-goldens` invocation exceeds its 30-minute budget;
- any command in the list exits non-zero;
- the second `make tool-goldens` leaves a diff in any manifest-declared
  artifact path.

Retrying under a hand-modified environment does not convert a failure into a
pass. If the command cannot run in a normal task-run pod, that is the finding.

## Provenance rule

The transcript must come from the candidate implementation commit, or from a
descendant that contains no change to the regeneration mechanism
(`scripts/regenerate-tool-goldens.mjs`, `scripts/tool-goldens.manifest.json`,
`scripts/lib/tool-goldens.mjs`, or the `tool-goldens*` targets in the
`Makefile`). A transcript from a commit that changed the mechanism does not
describe the mechanism being shipped.

No database cleanup is required; ordinary pod teardown removes the workspace.

## When this file is filled in

Replace the status banner at the top with the passing result, delete every
`TBD`, and only then mark proposal `3vqc` AC5 and AC6 as met.
