Tauri v2 desktop app — React 19 + TypeScript + Vite frontend, Rust backend.

## Dependency Management (pnpm)

- **Always run `pnpm install` after modifying `package.json`** — never leave the lockfile out of sync.
- **Always commit `pnpm-lock.yaml` alongside `package.json` changes** in the same commit.
- Never commit a `package.json` with dependency changes not reflected in `pnpm-lock.yaml`.
- Use `pnpm add <pkg>` / `pnpm add -D <pkg>` instead of hand-editing `package.json`.

## Build & Verify Commands

```bash
pnpm install              # install deps (use --frozen-lockfile in CI)
pnpm test                 # vitest (jsdom, vmThreads pool)
pnpm tsc --noEmit         # type-check without emitting
pnpm lint                 # eslint
pnpm build                # tsc + vite build
pnpm tauri:dev            # full Tauri dev (syncs sidecar first)
```

## Project Structure

- `@/` path alias → `src/`
- `src/api/` — MCP client, generated types, query hooks
- `src/components/` — UI components (shadcn/ui + custom)
- `src/pages/` — route pages
- `src/stores/` — Zustand stores
- `src/hooks/` — React hooks
- `src/test/` — test setup and mocks
- `src-tauri/` — Rust backend (Tauri)

## Tech Stack

- **Frontend**: React 19, TypeScript 5.9, Vite 7, Tailwind CSS 4
- **Desktop**: Tauri v2 (Rust backend)
- **State**: Zustand, TanStack Query
- **UI**: shadcn/ui (Radix + CVA), Lucide icons
- **Testing**: Vitest 4, Testing Library, jsdom
- **Storybook**: v9

## Testing

- Tests use jsdom with Tauri API mocks (`src/test/setup.ts`)
- Run `pnpm test` to execute; `pnpm test:watch` for watch mode
