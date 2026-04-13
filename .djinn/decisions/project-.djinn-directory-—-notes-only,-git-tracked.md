---
tags:
    - architecture
    - storage
    - v1
title: Project .djinn Directory — Notes Only, Git-Tracked
type: adr
---
# Project .djinn Directory — Notes Only, Git-Tracked

## Status
Accepted

## Context

In the Go server, each project's `.djinn/` directory contains:
- `djinn.db` (task database)
- `memory.db` (FTS5 index)
- `memory/` (markdown notes)
- `settings.json` (per-project config)
- `tasks/` (task data)
- `worktrees/` (git worktrees)
- `logs/` (operational logs)

This required `.gitignore` entries with carve-outs (`!.djinn/memory/`) to track notes while ignoring everything else.

## Decision

In the Rust rewrite, the per-project `.djinn/` directory contains **only knowledge base notes** (markdown files in typed folders: `decisions/`, `patterns/`, `research/`, `requirements/`, `reference/`, `design/`).

All other data lives at `~/.djinn/`:
- `djinn.db` — single global database (tasks, settings, note index/FTS5)
- Operational logs
- Global settings

Per-project config overrides are stored in the global DB, keyed by project path.

## Consequences

- **No `.gitignore` needed** for `.djinn/` — the entire directory is git-tracked content
- Notes are committed alongside code, providing architectural context in the same repo
- The memory.db FTS5 index is either in the global DB (keyed by project) or rebuilt from files on startup — no per-project SQLite file to ignore
- Worktrees are managed by git directly (`.git/worktrees/`), not stored under `.djinn/`
- Simpler mental model: `.djinn/` = knowledge, `~/.djinn/` = runtime state

## Relations

- [[Database Layer — rusqlite over libsql/Turso]] — single DB architecture
- [[Server Lifecycle — Desktop-Managed Daemon with Graceful Restart]] — global daemon reads from global DB
- [[requirements/v1-requirements]] — DB-01 (single DB at ~/.djinn/)