# Frontend Store Unit Tests (Vitest)

This repository is primarily Rust and does not include a Node package manifest at the root.

To satisfy the Zustand-store testing task, TypeScript test files were added under:

- `src/stores/taskStore.test.ts`
- `src/stores/epicStore.test.ts`
- `src/stores/projectStore.test.ts`
- shared fixtures: `src/test/fixtures.ts`

## Why `pnpm test` evidence is not available here

There is currently no `package.json` / `pnpm-lock.yaml` in this workspace, so `pnpm test` cannot be executed in this repository context.

A pre-existing test file (`tests/desktop_sse_event_handlers.test.ts`) also documents that Vitest must be added separately to run TS tests.

## How to run once JS tooling exists

After adding a frontend package manifest and Vitest scripts, run:

```bash
pnpm test
```

or directly:

```bash
pnpm vitest run src/stores/*.test.ts
```
