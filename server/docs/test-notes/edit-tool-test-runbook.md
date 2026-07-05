---
permalink: test-notes/edit-tool-test-runbook
title: "Edit tool test runbook (vmpq)"
type: reference
tags: [epic-vmpq, safe-edit, test-notes, proposal-ksnr]
---

# Edit tool test runbook (vmpq)

This runbook lists the focused commands used to verify the `djinn-agent` edit
matcher/dispatch, the agent schema/prompt snapshots, and the
`djinn-mcp-extension` schema snapshots for the proposal `ksnr` surface work
in epic `vmpq`.

All commands run from the repository checkout and require no external services.

## Agent matcher and dispatch regression

Run the `djinn-agent` matcher unit tests and dispatch integration tests:

```bash
cd server
cargo test -p djinn-agent --lib -- 'fuzzy_tests::'
cargo test -p djinn-agent --lib -- 'edit_dispatch_tests::'
```

The `fuzzy_tests` module covers the unit-level strategy chain (exact,
line-trimmed, whitespace-normalized, indentation-flexible, escape-normalized,
trimmed-boundary, Unicode-normalized, block-anchor, context-aware) plus ambiguity,
guard rejection, no-match nearest-miss, CRLF preservation, and multi-byte Unicode
offsets. The `edit_dispatch_tests` module covers end-to-end dispatch outcomes and
telemetry metadata.

## Large-file no-match performance benchmark

Release-mode ignored benchmark over a 1 MB file with a 200-line `old_text`:

```bash
cd server
cargo test -p djinn-agent --lib --release -- \
  'fuzzy_tests::large_file_no_match_completes_under_budget' --ignored --nocapture
```

This enforces the proposal `ksnr` bound of under 250 ms for the no-match path.
It is intentionally ignored in regular `cargo test` runs because wall-clock
measurements in debug mode or noisy CI/dev pods are unsuitable for a hard bound.
Run it on a quiet machine before claiming performance closure.

Sample result on a modest development machine (quiet, release mode):

```text
test fuzzy_tests::large_file_no_match_completes_under_budget ... ok
```

The test asserts the elapsed time is less than 250 ms and that the outcome is
`NoMatch` with zero candidates. Memory is verified by proxy: the matcher returns
no `byte_range` for a no-match, so no large replacement buffer is allocated.

## Agent schema and prompt snapshots

Run the snapshot tests in `djinn-agent` to verify or update the worker tool
schemas and the worker prompt/tool-section wording:

```bash
cd server
cargo test -p djinn-agent --lib extension::tests::worker_tool_schemas
cargo test -p djinn-agent --lib tools_section_snapshot
```

If the snapshot content is intentionally changed (e.g. updated edit tool
descriptions), accept the new snapshots with `cargo insta test --accept` or by
reviewing the `.snap.new` files and running `cargo insta accept`.

## MCP extension schema snapshots

Run the `djinn-mcp-extension` schema tests to verify the public edit tool
description and input schema surface:

```bash
cd server
cargo test -p djinn-mcp-extension
```

As with the agent snapshots, accept updated `.snap` files only when the schema
change is intentional.

## When to run this runbook

Use this runbook when:

* Changing edit tool public descriptions or schema input properties in
  `djinn-agent` or `djinn-mcp-extension`.
* Adding new matcher strategies, guard conditions, or response metadata
  fields that should appear in snapshots or dispatch tests.
* Closing the proposal `ksnr` test matrix, including the large-file performance
  bound.

## Related references

* [[design/vmpq-roadmap]] — epic plan for the schema/snapshot/test wave.
* [[design/c77e-roadmap]] — matcher implementation strategy chain.
