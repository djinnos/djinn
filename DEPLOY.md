# Deploying Djinn

Djinn ships as a small Docker Compose stack. A single `docker compose up` brings up the server, the Dolt database, and the Qdrant vector store.

## Prerequisites

- Docker (with Compose v2 — `docker compose`, not `docker-compose`)
- ~2 GB free disk for Dolt + Qdrant data volumes

No Rust toolchain, no Node runtime, and no local binary install are required for the default flow.

## Stack overview

```
Claude Code / MCP client
        │
        │ stdio (docker exec -i djinn-server djinn-server --mcp-connect)
        ▼
┌───────────────────┐
│   djinn-server    │  (listens on 127.0.0.1:8372)
└─────┬──────┬──────┘
      │      │
      │      └──── gRPC/HTTP ────► Qdrant  (6333 HTTP, 6334 gRPC)
      │
      └──── MySQL ──────────────► Dolt    (3306)
```

All three services run on the host loopback so local tools can talk to them directly.

## Connecting Claude Code

Djinn does NOT ship a Claude Code plugin for MCP (plugins only support stdio with a bundled command; bundling a Rust binary defeats the containerized deploy). Instead, add djinn to your MCP config manually.

**User-level** (`~/.claude/mcp.json`, or whichever your Claude Code build reads):
```json
{
  "mcpServers": {
    "djinn": {
      "type": "stdio",
      "command": "docker",
      "args": ["exec", "-i", "djinn-server", "djinn-server", "--mcp-connect"]
    }
  }
}
```

**Project-level** (`.mcp.json` at the root of any project where you want djinn):
Same snippet. Claude Code picks it up automatically when the directory is opened.

The `docker exec -i` command spawns the MCP stdio bridge inside the already-running `djinn-server` container; it forwards stdio to the in-container HTTP endpoint at `http://127.0.0.1:8372/mcp`. No host-side djinn binary required.

The `plugin/` directory in this repo still ships the skills (`/djinn:plan`, `/djinn:breakdown`, `/djinn:init-project`) and hooks (status line, context monitor). Those remain Claude-Code-plugin-packaged since they don't need MCP transport — they work off the djinn MCP server connection you wired up above.

## Start / stop

```bash
# Foreground (logs stream to terminal)
docker compose up

# Detached
docker compose up -d

# Stop and remove containers (volumes are preserved)
docker compose down
```

After startup, verify djinn-server:

```bash
curl http://127.0.0.1:8372/health
```

## Data persistence

Volumes are bind-mounted under `~/.djinn/` so data survives `docker compose down` and is easy to back up:

| Path                  | Contents                                    |
|-----------------------|---------------------------------------------|
| `~/.djinn/dolt/`      | Dolt database files (tasks, memory, etc.)   |
| `~/.djinn/qdrant/`    | Qdrant vector collections                   |
| `~/.djinn/logs/`      | djinn-server logs                           |

To wipe state, stop the stack and delete the relevant directory. **Deleting `~/.djinn/dolt/` destroys task and memory data.**

## Observability (Langfuse)

An optional Langfuse profile is bundled for tracing LLM calls and agent runs:

```bash
docker compose --profile observability up
```

This adds Langfuse to the stack. Open the Langfuse UI at http://127.0.0.1:3000 and configure djinn-server's Langfuse credentials in settings.

### Pointing at a remote djinn-server

To use a djinn-server running on another host, change the `docker exec` target in your mcp.json to a remote docker context, or replace the command with any stdio proxy that forwards to the remote MCP endpoint. Make sure the remote host has its own `~/.djinn/dolt/` and `~/.djinn/qdrant/` volumes.

## Development workflow

For server-side Rust iteration, keep the data services up via Compose and rebuild only the server image when you change server code:

```bash
# Keep dolt + qdrant running in the background
docker compose up -d dolt qdrant

# After code changes, rebuild and restart the server container
docker compose up --build djinn-server
```

Alternatively, use `server/Makefile`'s `cargo watch` target for a faster loop. In that flow the server process runs on the host and connects to the compose-provided Dolt via `DJINN_MYSQL_URL=mysql://root@127.0.0.1:3306/djinn` (dolt's 3306 is published on the host by `docker-compose.yml`). Qdrant is similarly reachable at `127.0.0.1:6333` / `127.0.0.1:6334`.

Dolt and Qdrant stay up across server rebuilds, so you do not lose state.
