/**
 * Connection mode persistence.
 *
 * Connection mode persistence and management
 */

import * as fs from "node:fs";
import * as path from "node:path";
import { homedir } from "node:os";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export type ConnectionMode =
  | { type: "daemon" }
  | { type: "remote"; url: string }
  | { type: "ssh"; host_id: string }
  | { type: "wsl" };

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

function prefsPath(): string {
  return path.join(homedir(), ".djinn", "connection_mode.json");
}

/** Load the persisted connection mode, falling back to `{ type: "daemon" }` on any error. */
export function load(): ConnectionMode {
  const p = prefsPath();
  try {
    const content = fs.readFileSync(p, "utf-8");
    return JSON.parse(content) as ConnectionMode;
  } catch {
    return { type: "daemon" };
  }
}

/** Persist the connection mode to disk. */
export function save(mode: ConnectionMode): void {
  const p = prefsPath();
  const parent = path.dirname(p);
  fs.mkdirSync(parent, { recursive: true });
  const content = JSON.stringify(mode, null, 2);
  fs.writeFileSync(p, content);
}
