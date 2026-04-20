Web client — React 19 + TypeScript + Vite + Tailwind 4 + shadcn/ui.

The UI is a plain SPA that talks to the Dockerized Djinn server over HTTP/SSE.
There is no Electron host. The server base URL defaults to
`http://127.0.0.1:8372` and can be overridden with `VITE_DJINN_SERVER_URL`.

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
pnpm dev                  # Vite dev server
pnpm preview              # preview production build
```

## Project Structure

- `@/` path alias → `src/`
- `src/api/` — server client (HTTP + SSE + MCP), generated types, query hooks
- `src/api/serverUrl.ts` — single source of truth for the server base URL
- `src/components/` — UI components (shadcn/ui + custom)
- `src/pages/` — route pages
- `src/stores/` — Zustand stores
- `src/hooks/` — React hooks
- `src/test/` — test setup

## Tech Stack

- **Frontend**: React 19, TypeScript 5.9, Vite 7, Tailwind CSS 4
- **State**: Zustand, TanStack Query
- **UI**: shadcn/ui (Radix + CVA), Lucide icons
- **Testing**: Vitest 4, Testing Library, jsdom
- **Storybook**: v9
