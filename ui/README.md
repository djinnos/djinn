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

- `VITE_DJINN_SERVER_URL` — override the server base URL (defaults to
  `http://127.0.0.1:8372`). Set this at `pnpm dev` / `pnpm build` time
  if you run the server on a different host or port.
