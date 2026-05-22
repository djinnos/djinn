# Djinn Web Client

React + TypeScript + Vite + shadcn/ui

The Djinn UI is a plain web application that talks to the Djinn server
over HTTP and SSE. The Electron wrapper was removed; the server now
runs in Docker and is reached at `http://127.0.0.1:8372` by default.

## Tech Stack

- **React 19** with TypeScript
- **Vite** for dev server and production bundling
- **Tailwind CSS 4.x** with a violet/zinc dark theme
- **shadcn/ui** accessible components

## Development

```bash
# Install dependencies
pnpm install

# Dev server (defaults to port 1420)
pnpm dev

# Production build
pnpm build

# Preview the production build
pnpm preview
```

## Configuration

- `VITE_DJINN_SERVER_URL` — override the server base URL. By default the UI
  issues same-origin requests (empty base), which is what you want in any
  deployment where the server hosts the embedded SPA (production, Tilt, Helm).
  Only set this at `pnpm dev` / `pnpm build` time when the UI is served from a
  different origin than the API — e.g. running Vite on `:1420` against a server
  on `:8372`. The override's host must match the server's `DJINN_PUBLIC_URL`
  exactly (don't mix `localhost` and `127.0.0.1` — they are distinct cookie
  origins and OAuth will break).
